//! Best-effort detection of targets that are currently in use.
//!
//! Two independent checks are performed:
//!
//! * **Lock files**: well-known build-tool and package-manager lock/state files inside the
//!   target (for example `target/debug/.cargo-lock`, `target/.rustc_info.json`,
//!   `node_modules/.package-lock.json`) or next to it (`Cargo.lock`, `package-lock.json`,
//!   `yarn.lock`, ...) that were modified within a recent window indicate a build or install in
//!   progress.
//! * **Process working directories**: a process whose current directory is inside the target.
//!   Linux reads `/proc/*/cwd`; macOS uses libproc `proc_pidinfo(PROC_PIDVNODEPATHINFO)` (only
//!   processes owned by the current user are visible without privileges). Windows has no cheap
//!   API for this and the check is skipped.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Lock/state files looked for *inside* the target.
const INNER_LOCKS: &[&str] = &[
    ".cargo-lock",
    "debug/.cargo-lock",
    "release/.cargo-lock",
    ".rustc_info.json",
    ".package-lock.json",
    ".yarn-integrity",
    ".modules.yaml",
    ".pnpm/lock.yaml",
    ".yarn-state.yml",
    "pip-delete-this-directory.txt",
    ".gradle/noVersion/buildLogic.lock",
    "build/.lock",
];

/// Package-manager lock files looked for *next to* the target.
const SIBLING_LOCKS: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "Gemfile.lock",
    "composer.lock",
    "go.sum",
];

/// Checks whether `path` appears to be in use.
///
/// Returns a human-readable reason, or `None` when nothing indicates activity. `window` is how
/// recently a lock file must have been modified to count; a zero window disables the lock-file
/// check.
#[must_use]
pub fn check_busy(path: &Path, window: Duration, check_processes: bool) -> Option<String> {
    if !window.is_zero() {
        let now = SystemTime::now();
        let recent = |candidate: &Path| -> bool {
            std::fs::symlink_metadata(candidate)
                .and_then(|m| m.modified())
                .is_ok_and(|mtime| now.duration_since(mtime).map_or(true, |age| age < window))
        };
        let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        if is_dir {
            for rel in INNER_LOCKS {
                let candidate = path.join(rel);
                if recent(&candidate) {
                    return Some(format!("{} was modified recently", candidate.display()));
                }
            }
        }
        if let Some(parent) = path.parent() {
            for name in SIBLING_LOCKS {
                let candidate = parent.join(name);
                if recent(&candidate) {
                    return Some(format!("{} was modified recently", candidate.display()));
                }
            }
        }
    }
    if check_processes && let Some(pid) = process_with_cwd_inside(path) {
        return Some(format!("process {pid} has its working directory inside"));
    }
    None
}

/// Returns the pid of a process whose working directory is inside `path`, if detectable.
#[must_use]
pub fn process_with_cwd_inside(path: &Path) -> Option<u32> {
    let target = std::fs::canonicalize(path).ok()?;
    platform::find_cwd_inside(&target)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::Path;

    pub fn find_cwd_inside(target: &Path) -> Option<u32> {
        let own = std::process::id();
        for entry in std::fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == own {
                continue;
            }
            if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd"))
                && cwd.starts_with(target)
            {
                return Some(pid);
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::CStr;
    use std::path::Path;

    pub fn find_cwd_inside(target: &Path) -> Option<u32> {
        let own = std::process::id();
        let mut pids: Vec<libc::c_int> = vec![0; 8192];
        loop {
            let bytes = libc::c_int::try_from(pids.len() * size_of::<libc::c_int>()).ok()?;
            // SAFETY: the buffer is valid for `bytes` bytes of writable `c_int`s.
            let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
            let count = usize::try_from(count).ok()?;
            if count < pids.len() {
                pids.truncate(count);
                break;
            }
            if pids.len() >= 1 << 20 {
                break;
            }
            pids.resize(pids.len() * 2, 0);
        }
        for pid in pids {
            let Ok(upid) = u32::try_from(pid) else {
                continue;
            };
            if upid == 0 || upid == own {
                continue;
            }
            // SAFETY: `proc_vnodepathinfo` is a plain C struct for which all-zero bytes is valid.
            let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
            let size = libc::c_int::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
            // SAFETY: `info` is a valid, writable buffer of `size` bytes for this flavor.
            let written = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDVNODEPATHINFO,
                    0,
                    std::ptr::addr_of_mut!(info).cast(),
                    size,
                )
            };
            if written != size {
                continue;
            }
            let raw = &info.pvi_cdir.vip_path;
            // SAFETY: `vip_path` is a contiguous `[[c_char; 32]; 32]` (MAXPATHLEN bytes) array;
            // viewing it as a flat byte slice of the same length is sound.
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(raw.as_ptr().cast::<u8>(), size_of_val(raw)) };
            let Ok(cstr) = CStr::from_bytes_until_nul(bytes) else {
                continue;
            };
            let Ok(text) = cstr.to_str() else {
                continue;
            };
            if !text.is_empty() && Path::new(text).starts_with(target) {
                return Some(upid);
            }
        }
        None
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use std::path::Path;

    /// No cheap API on this platform; always reports "not busy".
    pub const fn find_cwd_inside(_target: &Path) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_cargo_lock_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        assert!(check_busy(&target, Duration::from_secs(60), false).is_none());
        std::fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        assert!(check_busy(&target, Duration::from_secs(60), false).is_some());
        assert!(check_busy(&target, Duration::ZERO, false).is_none());
    }

    #[test]
    fn recent_sibling_lockfile_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("node_modules");
        std::fs::create_dir(&modules).unwrap();
        std::fs::write(dir.path().join("package-lock.json"), b"{}").unwrap();
        assert!(check_busy(&modules, Duration::from_secs(60), false).is_some());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn detects_child_process_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        // Give the child a moment to start.
        let mut found = None;
        for _ in 0..50 {
            found = process_with_cwd_inside(dir.path());
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().ok();
        child.wait().ok();
        assert_eq!(found, Some(child.id()));
    }
}
