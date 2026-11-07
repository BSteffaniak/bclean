//! Symlink-safe, parallel removal with progress and cancellation.
//!
//! * Symlinks (and Windows junctions) are removed as links; their targets are never touched.
//! * Directory trees are removed by a work-stealing traversal (rayon): every subdirectory at
//!   every depth is a task, so wide and deep trees (`target/debug/deps`, `node_modules`) use all
//!   cores instead of one.
//! * On Unix the traversal works relative to open directory handles (`openat`/`unlinkat` with
//!   `O_NOFOLLOW`), so a directory swapped for a symlink mid-removal is never followed, and
//!   long paths are not re-resolved for every entry.
//! * Read-only entries (Windows attribute, or Unix directories lacking write permission) are
//!   made writable and the removal retried once.
//! * Every removed entry (file, link, or directory, including the root) increments a counter,
//!   which callers poll for progress. Cancellation is checked between entries.

use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::CancelToken;

/// Removes `path` (file, directory tree, or symlink) without following symlinks.
///
/// # Errors
///
/// * `path` does not exist.
/// * Some entry cannot be removed even after clearing read-only flags.
pub fn remove_path(path: &Path) -> io::Result<()> {
    remove_path_counted(path, &AtomicU64::new(0), &CancelToken::new())
}

/// Like [`remove_path`], adding one to `removed` for every entry removed (the root included),
/// and stopping early once `cancel` is cancelled.
///
/// Entries that fail are skipped so as much as possible is removed; the first error is
/// returned.
///
/// # Errors
///
/// * `path` does not exist.
/// * Some entry cannot be removed even after clearing read-only flags.
/// * [`io::ErrorKind::Interrupted`] if cancelled before finishing (what was removed so far
///   stays removed).
pub fn remove_path_counted(
    path: &Path,
    removed: &AtomicU64,
    cancel: &CancelToken,
) -> io::Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        remove_link_or_file(path)?;
        removed.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let ctx = Ctx {
        removed,
        cancel,
        error: Mutex::new(None),
    };
    imp::remove_dir_tree(path, &ctx)?;
    if cancel.is_cancelled() {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
    }
    ctx.error
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .map_or(Ok(()), Err)
}

/// Shared state of one removal.
struct Ctx<'a> {
    removed: &'a AtomicU64,
    cancel: &'a CancelToken,
    /// First error; later ones are dropped.
    error: Mutex<Option<io::Error>>,
}

impl Ctx<'_> {
    /// Records the outcome of removing one entry. Entries that vanished count as removed by
    /// someone else and are not errors.
    fn record(&self, result: io::Result<()>) {
        match result {
            Ok(()) => {
                self.removed.fetch_add(1, Ordering::Relaxed);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                let mut slot = self
                    .error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                slot.get_or_insert(err);
            }
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

#[cfg(unix)]
mod imp {
    //! Handle-relative removal with `openat`/`unlinkat`.

    use std::ffi::{CStr, CString};
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use super::Ctx;

    pub fn remove_dir_tree(path: &Path, ctx: &Ctx<'_>) -> io::Result<()> {
        let (parent, name) = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => (
                if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                },
                name,
            ),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "refusing to remove a path without a parent",
                ));
            }
        };
        let parent = CString::new(parent.as_os_str().as_bytes())?;
        // The parent itself may be reached through symlinks; only entries inside `path` are
        // opened with O_NOFOLLOW.
        let parent = open_dir(libc::AT_FDCWD, &parent, false)?;
        let name = CString::new(name.as_bytes())?;
        remove_dir_at(&parent, &name, ctx);
        Ok(())
    }

    /// Removes directory `name` inside `parent` and everything below it.
    fn remove_dir_at(parent: &OwnedFd, name: &CStr, ctx: &Ctx<'_>) {
        if ctx.cancelled() {
            return;
        }
        let dir = match open_dir(parent.as_raw_fd(), name, true) {
            Err(err) if is_permission(&err) => {
                // A directory without read/search permission: grant it to ourselves.
                add_owner_rwx_at(parent, name);
                open_dir(parent.as_raw_fd(), name, true)
            }
            other => other,
        };
        let dir = match dir {
            Ok(dir) => dir,
            Err(err) => return ctx.record(Err(err)),
        };
        let entries = match list(&dir) {
            Ok(entries) => entries,
            Err(err) => return ctx.record(Err(err)),
        };
        rayon::scope(|scope| {
            let dir = &dir;
            for (entry, is_dir) in &entries {
                if *is_dir {
                    scope.spawn(move |_| remove_dir_at(dir, entry, ctx));
                }
            }
            for (entry, _) in entries.iter().filter(|(_, is_dir)| !is_dir) {
                if ctx.cancelled() {
                    return;
                }
                ctx.record(unlink_at(dir, entry, 0));
            }
        });
        drop(dir);
        if !ctx.cancelled() {
            ctx.record(unlink_at(parent, name, libc::AT_REMOVEDIR));
        }
    }

    /// Lists `dir` as `(name, is_directory)` pairs, without following symlinks.
    fn list(dir: &OwnedFd) -> io::Result<Vec<(CString, bool)>> {
        // SAFETY: dup of a descriptor we own; fdopendir takes ownership of the duplicate.
        let dup = unsafe { libc::dup(dir.as_raw_fd()) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `dup` is a valid directory descriptor owned by nobody else.
        let stream = unsafe { libc::fdopendir(dup) };
        if stream.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so `dup` is still ours to close.
            unsafe { libc::close(dup) };
            return Err(err);
        }
        let mut entries = Vec::new();
        loop {
            // SAFETY: `stream` is a valid DIR* used only by this thread.
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            // SAFETY: readdir returned a valid dirent whose name is NUL-terminated; it stays
            // valid until the next readdir/closedir on `stream`, and is copied before that.
            let (name, d_type) =
                unsafe { (CStr::from_ptr((*entry).d_name.as_ptr()), (*entry).d_type) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = name.to_owned();
            let is_dir = match d_type {
                libc::DT_DIR => true,
                libc::DT_UNKNOWN => is_dir_at(dir, &name),
                _ => false,
            };
            entries.push((name, is_dir));
        }
        // SAFETY: `stream` is valid and not used afterwards; this also closes `dup`.
        unsafe { libc::closedir(stream) };
        Ok(entries)
    }

    fn open_dir(parent: libc::c_int, name: &CStr, no_follow: bool) -> io::Result<OwnedFd> {
        let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        if no_follow {
            flags |= libc::O_NOFOLLOW;
        }
        // SAFETY: `name` is NUL-terminated and `parent` is a directory descriptor or AT_FDCWD.
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// `unlinkat`, granting the containing directory owner write permission and retrying once
    /// if it lacks it.
    fn unlink_at(dir: &OwnedFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        let unlink = || {
            // SAFETY: `name` is NUL-terminated and `dir` is an open directory descriptor.
            if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flags) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        };
        match unlink() {
            Err(err) if is_permission(&err) => {
                add_owner_rwx(dir);
                unlink()
            }
            other => other,
        }
    }

    fn is_dir_at(dir: &OwnedFd, name: &CStr) -> bool {
        // SAFETY: an all-zero `stat` is a valid value for fstatat to overwrite.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor, NUL-terminated name, and a writable `stat`.
        let ok = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &raw mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0;
        ok && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR
    }

    /// Best-effort: adds owner read/write/search permission to an open directory.
    fn add_owner_rwx(dir: &OwnedFd) {
        // SAFETY: an all-zero `stat` is a valid value for fstat to overwrite.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor and a writable `stat`.
        if unsafe { libc::fstat(dir.as_raw_fd(), &raw mut st) } == 0 {
            // SAFETY: valid descriptor; only permission bits are changed.
            unsafe { libc::fchmod(dir.as_raw_fd(), (st.st_mode & 0o7777) | 0o700) };
        }
    }

    /// Best-effort: adds owner read/write/search permission to directory `name` in `parent`,
    /// only if it is a real directory (never a symlink's target).
    fn add_owner_rwx_at(parent: &OwnedFd, name: &CStr) {
        // SAFETY: an all-zero `stat` is a valid value for fstatat to overwrite.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid descriptor, NUL-terminated name, and a writable `stat`.
        let found = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                &raw mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0;
        if found && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            // SAFETY: valid descriptor and NUL-terminated name; only permission bits change.
            unsafe {
                libc::fchmodat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    (st.st_mode & 0o7777) | 0o700,
                    0,
                );
            }
        }
    }

    fn is_permission(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(libc::EACCES | libc::EPERM))
    }
}

#[cfg(not(unix))]
mod imp {
    //! Path-based removal (Windows and other platforms).

    use std::io;
    use std::path::{Path, PathBuf};

    use super::{Ctx, make_writable, remove_link_or_file, retry_writable};

    #[allow(clippy::unnecessary_wraps)] // signature shared with the Unix implementation
    pub fn remove_dir_tree(path: &Path, ctx: &Ctx<'_>) -> io::Result<()> {
        remove_dir(path, ctx);
        Ok(())
    }

    fn remove_dir(path: &Path, ctx: &Ctx<'_>) {
        if ctx.cancelled() {
            return;
        }
        let read = || std::fs::read_dir(path)?.collect::<io::Result<Vec<_>>>();
        let entries = match read().or_else(|_| {
            make_writable(path);
            read()
        }) {
            Ok(entries) => entries,
            Err(err) => return ctx.record(Err(err)),
        };
        let (dirs, others): (Vec<PathBuf>, Vec<PathBuf>) =
            entries.iter().map(std::fs::DirEntry::path).partition(|p| {
                // DirEntry::file_type does not follow links; junctions report as links.
                std::fs::symlink_metadata(p)
                    .is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
            });
        rayon::scope(|scope| {
            for dir in &dirs {
                scope.spawn(move |_| remove_dir(dir, ctx));
            }
            for other in &others {
                if ctx.cancelled() {
                    return;
                }
                ctx.record(remove_link_or_file(other));
            }
        });
        if !ctx.cancelled() {
            ctx.record(retry_writable(path, || std::fs::remove_dir(path)));
        }
    }
}

fn remove_link_or_file(path: &Path) -> io::Result<()> {
    match retry_writable(path, || std::fs::remove_file(path)) {
        Ok(()) => Ok(()),
        // Windows directory symlinks and junctions are removed with `remove_dir`, which
        // deletes the link itself, not the target's contents.
        Err(err) if cfg!(windows) && err.kind() != io::ErrorKind::NotFound => {
            std::fs::remove_dir(path).map_err(|_| err)
        }
        Err(err) => Err(err),
    }
}

/// Runs `op`; on permission failure makes `path` (and its parent) writable and retries once.
fn retry_writable(path: &Path, op: impl Fn() -> io::Result<()>) -> io::Result<()> {
    match op() {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            make_writable(path);
            if let Some(parent) = path.parent() {
                make_writable(parent);
            }
            op()
        }
        other => other,
    }
}

/// Best-effort: clears the read-only attribute (Windows) or adds owner write+exec permission to
/// directories (Unix). Never follows symlinks.
pub fn make_writable(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut perms = meta.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.is_dir() {
            perms.set_mode(perms.mode() | 0o700);
            std::fs::set_permissions(path, perms).ok();
        }
    }
    #[cfg(not(unix))]
    {
        if perms.readonly() {
            // Only clears the read-only attribute; this is the intended Windows semantics.
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(path, perms).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_nested_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("t");
        for i in 0..20 {
            let d = root.join(format!("a{i}/b/c/d"));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("f"), b"x").unwrap();
        }
        remove_path(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn counts_every_removed_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("t");
        for i in 0..10 {
            let d = root.join(format!("d{i}/sub"));
            std::fs::create_dir_all(&d).unwrap();
            for j in 0..5 {
                std::fs::write(d.join(format!("f{j}")), b"x").unwrap();
            }
        }
        // root + 10 × (d, sub, 5 files)
        let removed = AtomicU64::new(0);
        remove_path_counted(&root, &removed, &CancelToken::new()).unwrap();
        assert!(!root.exists());
        assert_eq!(removed.load(Ordering::Relaxed), 1 + 10 * 7);

        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let removed = AtomicU64::new(0);
        remove_path_counted(&file, &removed, &CancelToken::new()).unwrap();
        assert_eq!(removed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stops_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("t");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/f"), b"x").unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = remove_path_counted(&root, &AtomicU64::new(0), &cancel).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(root.join("a/f").exists());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious"), b"keep").unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("x/y/z")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("x/y/z/deep_link")).unwrap();
        remove_path(&root).unwrap();
        assert!(!root.exists());
        assert!(outside.join("precious").exists());

        // Removing a symlink directly removes only the link.
        let link = dir.path().join("direct");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        remove_path(&link).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(outside.join("precious").exists());
    }

    #[cfg(unix)]
    #[test]
    fn handles_read_only_dirs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ro");
        let inner = root.join("a/b/c/inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("f"), b"x").unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o500)).unwrap();
        // No read or search permission at all.
        let locked = root.join("locked");
        std::fs::create_dir_all(locked.join("deep")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        remove_path(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = remove_path(&dir.path().join("nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
