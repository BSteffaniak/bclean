//! `FS_TIMES`: newest modification (and, when trustworthy, access) time in a subtree.
//!
//! # atime caveats
//!
//! Access times are only consulted when [`AtimePolicy`] allows it and the volume is not
//! mounted `noatime`:
//!
//! * Linux defaults to `relatime`: atime is updated only when it is older than mtime/ctime or
//!   more than 24 hours old, so it is accurate to about a day — fine for staleness thresholds
//!   measured in days. `noatime` mounts make atime useless and are detected via
//!   `/proc/self/mountinfo`.
//! * macOS/APFS updates atime similarly lazily; `noatime` is detected via `statfs`.
//! * Windows (NTFS) frequently has last-access updates disabled or delayed by up to an hour,
//!   so atime is never trusted there for `FS_TIMES` (`OS_LAST_USED` reports it separately).
//! * Only regular files' atimes count: directory atimes change on every listing, including
//!   bclean's own scans. Backup tools, indexers, and antivirus scanners that read file
//!   contents also bump atime, which can only make items look *fresher* (the safe direction).

use std::fmt::Write as _;
use std::path::Path;

use bclean_model::{SignalKind, SignalReading};
use bclean_tree::NodeFlags;

use crate::walk::{WalkLimits, bounded_walk};
use crate::{SignalProvider, Target, platform};

/// When to use access times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AtimePolicy {
    /// Never use atime.
    Never,
    /// Use atime when the volume is known not to be mounted `noatime` (and never on Windows).
    #[default]
    IfTrusted,
    /// Always use atime.
    Always,
}

/// Whether access times on the volume containing `path` are maintained (best effort).
#[must_use]
pub fn atime_trusted(path: &Path) -> bool {
    platform::atime_trusted(path)
}

/// `FS_TIMES` provider.
#[derive(Debug, Clone)]
pub struct FsTimesProvider {
    /// atime usage.
    pub atime: AtimePolicy,
    /// Walk bounds used when no tree data is available.
    pub limits: WalkLimits,
}

impl Default for FsTimesProvider {
    fn default() -> Self {
        Self {
            atime: AtimePolicy::IfTrusted,
            limits: WalkLimits {
                max_entries: 20_000,
                max_depth: 32,
            },
        }
    }
}

impl FsTimesProvider {
    fn use_atime(&self, path: &Path) -> bool {
        match self.atime {
            AtimePolicy::Never => false,
            AtimePolicy::Always => true,
            AtimePolicy::IfTrusted => atime_trusted(path),
        }
    }
}

impl SignalProvider for FsTimesProvider {
    fn kind(&self) -> SignalKind {
        SignalKind::FsTimes
    }

    fn read(&self, target: &Target<'_>) -> Option<SignalReading> {
        if let Some((tree, node)) = target.node {
            let info = tree.info(node);
            let complete = !info.flags.contains(NodeFlags::ERROR);
            if info.max_mtime > 0 && (complete || self.atime == AtimePolicy::Never) {
                return Some(SignalReading {
                    kind: SignalKind::FsTimes,
                    last_activity: Some(info.max_mtime),
                    forces_stale: false,
                    detail: "newest mtime in subtree (scan data)".to_owned(),
                });
            }
        }
        let want_atime = self.use_atime(target.path);
        let walk = bounded_walk(target.path, self.limits, want_atime, &|_| false);
        let last = match (walk.max_mtime, walk.max_atime) {
            (Some(m), Some(a)) => Some(m.max(a)),
            (m, a) => m.or(a),
        }?;
        let mut detail = if walk.max_atime.is_some_and(|a| Some(a) > walk.max_mtime) {
            "newest file atime in subtree".to_owned()
        } else {
            "newest mtime in subtree".to_owned()
        };
        if walk.truncated {
            let _ = write!(detail, " (partial: first {} entries)", walk.entries);
        }
        Some(SignalReading {
            kind: SignalKind::FsTimes,
            last_activity: Some(last),
            forces_stale: false,
            detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_tree::{EntryKind, EntryMeta, NodeId, Tree};
    use filetime::{FileTime, set_file_mtime};

    #[test]
    fn prefers_tree_max_mtime() {
        let mut tree = Tree::new("/x", EntryMeta::dir());
        tree.add_child(
            NodeId::ROOT,
            "f".as_ref(),
            EntryMeta::new(EntryKind::File, 1, 1, 4_242),
        );
        tree.recompute_aggregates();
        let p = Path::new("/x");
        let r = FsTimesProvider::default()
            .read(&Target::node(&tree, NodeId::ROOT, p))
            .unwrap();
        assert_eq!(r.last_activity, Some(4_242));
    }

    #[test]
    fn walks_without_tree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "x").unwrap();
        set_file_mtime(dir.path().join("f"), FileTime::from_unix_time(3_000, 0)).unwrap();
        set_file_mtime(dir.path(), FileTime::from_unix_time(1_000, 0)).unwrap();
        let provider = FsTimesProvider {
            atime: AtimePolicy::Never,
            ..FsTimesProvider::default()
        };
        let r = provider.read(&Target::path(dir.path(), true)).unwrap();
        assert_eq!(r.last_activity, Some(3_000));
    }
}
