#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Staleness signal providers: filesystem times, project activity, OS metadata, git state.
//!
//! *Stale* means *unused*. Each [`SignalProvider`] inspects one aspect of an item's usage and
//! produces a [`SignalReading`] (latest activity it saw and/or a "forces stale" verdict).
//! [`combine`] folds readings into a [`Staleness`] verdict given an idle threshold:
//!
//! * `last_activity` = the newest activity across all readings
//! * `stale` = any reading `forces_stale`, **or** `now - last_activity >= threshold`, **or**
//!   (when a project-idle threshold is given) the `PROJECT_ACTIVITY` reading alone is idle for
//!   at least that long. Build output is touched by every build (editors run `cargo check` on
//!   open), so the owning project's activity is the better signal for it.
//! * With no activity information at all and no forcing signal, the item is **not** stale
//!   (unknown is treated as fresh, to stay conservative).
//!
//! Providers:
//!
//! | kind               | provider                    | source                                         |
//! | ------------------ | --------------------------- | ---------------------------------------------- |
//! | `FS_TIMES`         | [`FsTimesProvider`]         | tree `max_mtime`, else a bounded walk (+atime) |
//! | `PROJECT_ACTIVITY` | [`ProjectActivityProvider`] | last commit, HEAD reflog/index mtime, sources  |
//! | `OS_LAST_USED`     | [`OsLastUsedProvider`]      | Spotlight `kMDItemLastUsedDate`, Windows atime |
//! | `GIT_STATE`        | [`GitStateProvider`]        | merged/deleted-branch and orphaned worktrees   |
//!
//! All providers are `Send + Sync` and cache per project / repository so they can be shared
//! across rayon workers.

mod fs_times;
mod git;
mod git_state;
mod os_last_used;
mod platform;
mod project;
mod walk;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use bclean_model::{SignalKind, SignalReading, Staleness};
use bclean_tree::{NodeId, Tree};

pub use fs_times::{AtimePolicy, FsTimesProvider, atime_trusted};
pub use git::{GitDirLink, read_git_link};
pub use git_state::{GitStateProvider, WorktreeInfo, WorktreeState, list_worktrees};
pub use os_last_used::{OsLastUsedProvider, os_last_used};
pub use project::{
    DEFAULT_PROJECT_MARKERS, DEFAULT_SKIP_DIRS, ProjectActivityProvider, ProjectInfo,
    find_project_root,
};

/// The item whose staleness is being evaluated.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// Absolute path.
    pub path: &'a Path,
    /// Whether the item is a directory.
    pub is_dir: bool,
    /// The scanned tree node for the item, when available (avoids filesystem walks).
    pub node: Option<(&'a Tree, NodeId)>,
}

impl<'a> Target<'a> {
    /// A target without tree data.
    #[must_use]
    pub const fn path(path: &'a Path, is_dir: bool) -> Self {
        Self {
            path,
            is_dir,
            node: None,
        }
    }

    /// A target backed by a tree node.
    #[must_use]
    pub fn node(tree: &'a Tree, node: NodeId, path: &'a Path) -> Self {
        Self {
            path,
            is_dir: tree.kind(node) == bclean_tree::EntryKind::Dir,
            node: Some((tree, node)),
        }
    }
}

/// Produces one kind of staleness signal.
pub trait SignalProvider: Send + Sync {
    /// The signal kind this provider produces.
    fn kind(&self) -> SignalKind;

    /// Reads the signal for `target`, or `None` if the provider has nothing to say.
    fn read(&self, target: &Target<'_>) -> Option<SignalReading>;
}

/// Thresholds applied when combining readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StalenessPolicy {
    /// Idle seconds after which the item is stale.
    pub threshold_secs: u64,
    /// Idle seconds of the owning project after which the item is stale regardless of its own
    /// activity.
    pub project_idle_secs: Option<u64>,
    /// Signals to consult.
    pub signals: Vec<SignalKind>,
}

impl StalenessPolicy {
    /// A policy consulting every signal kind.
    #[must_use]
    pub fn new(threshold_secs: u64) -> Self {
        Self {
            threshold_secs,
            project_idle_secs: None,
            signals: SignalKind::ALL.to_vec(),
        }
    }
}

/// Current time in Unix seconds.
#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Converts a [`SystemTime`] to Unix seconds (negative before the epoch).
#[must_use]
pub fn system_time_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

fn idle(now: i64, last: i64) -> u64 {
    u64::try_from(now.saturating_sub(last)).unwrap_or(0)
}

/// Folds readings into a verdict. See the crate docs for the exact rule.
#[must_use]
pub fn combine(signals: Vec<SignalReading>, policy: &StalenessPolicy, now: i64) -> Staleness {
    let last_activity = signals.iter().filter_map(|s| s.last_activity).max();
    let idle_secs = last_activity.map(|last| idle(now, last));
    let forced = signals.iter().any(|s| s.forces_stale);
    let idle_stale = idle_secs.is_some_and(|i| i >= policy.threshold_secs);
    let project_stale = policy.project_idle_secs.is_some_and(|limit| {
        signals
            .iter()
            .find(|s| s.kind == SignalKind::ProjectActivity)
            .and_then(|s| s.last_activity)
            .is_some_and(|last| idle(now, last) >= limit)
    });
    Staleness {
        stale: forced || idle_stale || project_stale,
        last_activity,
        idle_secs,
        threshold_secs: policy.threshold_secs,
        signals,
    }
}

/// A set of providers plus a fixed evaluation time.
pub struct StalenessEngine {
    providers: BTreeMap<SignalKind, Box<dyn SignalProvider>>,
    now: i64,
}

impl std::fmt::Debug for StalenessEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StalenessEngine")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("now", &self.now)
            .finish()
    }
}

impl Default for StalenessEngine {
    fn default() -> Self {
        Self::with_defaults(unix_now())
    }
}

impl StalenessEngine {
    /// An engine with no providers.
    #[must_use]
    pub fn empty(now: i64) -> Self {
        Self {
            providers: BTreeMap::new(),
            now,
        }
    }

    /// An engine with the default provider for every signal kind.
    #[must_use]
    pub fn with_defaults(now: i64) -> Self {
        let mut engine = Self::empty(now);
        engine.register(FsTimesProvider::default());
        engine.register(ProjectActivityProvider::default());
        engine.register(OsLastUsedProvider);
        engine.register(GitStateProvider::default());
        engine
    }

    /// Registers (or replaces) the provider for its signal kind.
    pub fn register(&mut self, provider: impl SignalProvider + 'static) {
        self.providers.insert(provider.kind(), Box::new(provider));
    }

    /// The evaluation time (Unix seconds).
    #[must_use]
    pub const fn now(&self) -> i64 {
        self.now
    }

    /// The registered provider for `kind`.
    #[must_use]
    pub fn provider(&self, kind: SignalKind) -> Option<&dyn SignalProvider> {
        self.providers.get(&kind).map(AsRef::as_ref)
    }

    /// Reads every signal in `policy.signals` and combines them.
    #[must_use]
    pub fn evaluate(&self, target: &Target<'_>, policy: &StalenessPolicy) -> Staleness {
        let mut kinds = policy.signals.clone();
        kinds.sort_unstable();
        kinds.dedup();
        let readings = kinds
            .iter()
            .filter_map(|kind| self.providers.get(kind))
            .filter_map(|p| p.read(target))
            .collect();
        combine(readings, policy, self.now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(kind: SignalKind, last: Option<i64>, forces: bool) -> SignalReading {
        SignalReading {
            kind,
            last_activity: last,
            forces_stale: forces,
            detail: String::new(),
        }
    }

    #[test]
    fn combine_uses_newest_activity() {
        let policy = StalenessPolicy::new(100);
        let s = combine(
            vec![
                reading(SignalKind::FsTimes, Some(500), false),
                reading(SignalKind::ProjectActivity, Some(950), false),
            ],
            &policy,
            1000,
        );
        assert!(!s.stale);
        assert_eq!(s.last_activity, Some(950));
        assert_eq!(s.idle_secs, Some(50));
        let s = combine(
            vec![reading(SignalKind::FsTimes, Some(800), false)],
            &policy,
            1000,
        );
        assert!(s.stale);
    }

    #[test]
    fn combine_forced_and_unknown() {
        let policy = StalenessPolicy::new(100);
        assert!(!combine(vec![], &policy, 1000).stale);
        let s = combine(
            vec![
                reading(SignalKind::FsTimes, Some(999), false),
                reading(SignalKind::GitState, None, true),
            ],
            &policy,
            1000,
        );
        assert!(s.stale);
    }

    #[test]
    fn combine_project_idle_overrides_fresh_fs() {
        let mut policy = StalenessPolicy::new(1_000);
        policy.project_idle_secs = Some(100);
        let s = combine(
            vec![
                reading(SignalKind::FsTimes, Some(990), false),
                reading(SignalKind::ProjectActivity, Some(800), false),
            ],
            &policy,
            1000,
        );
        assert!(s.stale);
        assert_eq!(s.idle_secs, Some(10));
    }

    #[test]
    fn future_timestamps_are_not_negative_idle() {
        let s = combine(
            vec![reading(SignalKind::FsTimes, Some(5_000), false)],
            &StalenessPolicy::new(10),
            1000,
        );
        assert_eq!(s.idle_secs, Some(0));
        assert!(!s.stale);
    }
}
