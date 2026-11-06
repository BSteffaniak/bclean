//! Symlink-safe, parallel removal.
//!
//! * Symlinks (and Windows junctions) are removed as links; their targets are never touched.
//! * Directories are removed with the entries of the top levels processed in parallel (rayon),
//!   and deeper subtrees handed to [`std::fs::remove_dir_all`], which does not follow symlinks
//!   and is resistant to symlink races.
//! * Read-only files (Windows attribute, or Unix directories lacking write permission) are made
//!   writable and the removal retried once.

use std::io;
use std::path::Path;

use rayon::prelude::*;

/// Directory levels processed in parallel before delegating to `remove_dir_all`.
const PARALLEL_DEPTH: usize = 2;

/// Removes `path` (file, directory tree, or symlink) without following symlinks.
///
/// # Errors
///
/// * `path` does not exist.
/// * Some entry cannot be removed even after clearing read-only flags.
pub fn remove_path(path: &Path) -> io::Result<()> {
    remove_entry(path, PARALLEL_DEPTH)
}

fn remove_entry(path: &Path, depth: usize) -> io::Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        return remove_link_or_file(path);
    }
    if depth == 0 {
        return retry_writable(path, || std::fs::remove_dir_all(path)).or_else(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                // `remove_dir_all` may stop at a read-only entry; fall back to a sequential
                // walk that fixes permissions as it goes.
                remove_slow(path)
            }
        });
    }
    let entries: Vec<_> = std::fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
    let errors: Vec<io::Error> = entries
        .par_iter()
        .filter_map(|entry| remove_entry(&entry.path(), depth - 1).err())
        .filter(|err| err.kind() != io::ErrorKind::NotFound)
        .collect();
    if let Some(err) = errors.into_iter().next() {
        return Err(err);
    }
    retry_writable(path, || std::fs::remove_dir(path))
}

fn remove_slow(path: &Path) -> io::Result<()> {
    let file_type = std::fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        return remove_link_or_file(path);
    }
    make_writable(path);
    for entry in std::fs::read_dir(path)? {
        remove_slow(&entry?.path())?;
    }
    retry_writable(path, || std::fs::remove_dir(path))
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
        remove_path(&root).unwrap();
        assert!(!root.exists());
    }
}
