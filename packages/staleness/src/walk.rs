//! Bounded filesystem walk collecting the newest modification (and access) time.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::system_time_secs;

/// Bounds for [`bounded_walk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkLimits {
    /// Maximum number of entries inspected.
    pub max_entries: usize,
    /// Maximum directory depth below the root (root = 0).
    pub max_depth: usize,
}

/// Result of [`bounded_walk`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalkResult {
    /// Newest mtime of any entry (directories included).
    pub max_mtime: Option<i64>,
    /// Newest atime of any regular file (only collected when requested).
    pub max_atime: Option<i64>,
    /// Entries inspected.
    pub entries: usize,
    /// Whether a limit was hit before the walk finished.
    pub truncated: bool,
}

impl WalkResult {
    fn note_mtime(&mut self, meta: &fs::Metadata) {
        if let Ok(m) = meta.modified() {
            let secs = system_time_secs(m);
            self.max_mtime = Some(self.max_mtime.map_or(secs, |cur| cur.max(secs)));
        }
    }

    fn note_atime(&mut self, meta: &fs::Metadata) {
        if let Ok(a) = meta.accessed() {
            let secs = system_time_secs(a);
            self.max_atime = Some(self.max_atime.map_or(secs, |cur| cur.max(secs)));
        }
    }
}

/// Walks `root` without following symlinks, skipping directories for which `skip_dir`
/// returns `true` (the root itself is never skipped).
///
/// Directory mtimes are included (adding or removing entries is activity); atimes are only
/// collected from regular files because directory atimes are bumped by any listing, including
/// bclean's own scans.
pub fn bounded_walk(
    root: &Path,
    limits: WalkLimits,
    want_atime: bool,
    skip_dir: &dyn Fn(&OsStr) -> bool,
) -> WalkResult {
    let mut out = WalkResult::default();
    let Ok(meta) = fs::symlink_metadata(root) else {
        return out;
    };
    out.entries = 1;
    out.note_mtime(&meta);
    if !meta.is_dir() {
        if want_atime && meta.is_file() {
            out.note_atime(&meta);
        }
        return out;
    }
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if out.entries >= limits.max_entries {
                out.truncated = true;
                return out;
            }
            out.entries += 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            out.note_mtime(&meta);
            if file_type.is_dir() {
                if skip_dir(&entry.file_name()) {
                    continue;
                }
                if depth + 1 > limits.max_depth {
                    out.truncated = true;
                } else {
                    stack.push((entry.path(), depth + 1));
                }
            } else if want_atime && file_type.is_file() {
                out.note_atime(&meta);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{FileTime, set_file_mtime};

    #[test]
    fn finds_newest_mtime_and_respects_skips_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("skip")).unwrap();
        fs::write(root.join("a/b/f"), "x").unwrap();
        fs::write(root.join("skip/g"), "x").unwrap();
        set_file_mtime(root.join("a/b/f"), FileTime::from_unix_time(2_000, 0)).unwrap();
        set_file_mtime(root.join("skip/g"), FileTime::from_unix_time(9_000, 0)).unwrap();
        for d in ["a/b", "a", "skip", ""] {
            set_file_mtime(root.join(d), FileTime::from_unix_time(1_000, 0)).unwrap();
        }
        let limits = WalkLimits {
            max_entries: 1_000,
            max_depth: 10,
        };
        let r = bounded_walk(root, limits, false, &|n| n == "skip");
        assert_eq!(r.max_mtime, Some(2_000));
        assert!(!r.truncated);

        let r = bounded_walk(
            root,
            WalkLimits {
                max_entries: 2,
                max_depth: 10,
            },
            false,
            &|_| false,
        );
        assert!(r.truncated);
        assert_eq!(r.entries, 2);
    }
}
