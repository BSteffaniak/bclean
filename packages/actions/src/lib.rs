#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Cleanup action execution (delete, trash, tool commands) with a journal and undo.
//!
//! This is the only bclean crate allowed to destroy data. Every destructive path goes through
//! [`Executor`], which:
//!
//! * refuses protected paths ([`Protector`]: config globs plus hardcoded never-touch paths),
//! * refuses entries that disappeared or changed type since planning,
//! * refuses targets that look busy (recent lock files, processes with a cwd inside),
//! * never follows symlinks when deleting,
//! * supports dry runs (and always dry-runs shadow plans),
//! * reports per-item progress, honours cancellation, measures freed space, and
//! * produces a journal record for every attempted item, which the caller persists in the
//!   SQLite [`Journal`] (via `switchy_database`) for audit and [`undo`].
//!
//! # Example
//!
//! ```no_run
//! use bclean_actions::{CancelToken, Executor, ExecutorOptions, Plan, Protector};
//!
//! let plan = Plan::default();
//! let executor = Executor::new(
//!     Protector::builtin(),
//!     ExecutorOptions { dry_run: true, ..ExecutorOptions::default() },
//! );
//! let report = executor.execute(&plan, &CancelToken::new(), |_| {});
//! assert!(report.dry_run);
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod archive;
pub mod busy;
mod executor;
pub mod journal;
mod plan;
pub mod protect;
pub mod remove;
pub mod shell;
pub mod space;
pub mod trim;
mod undo;

pub use executor::{
    ExecutionReport, Executor, ExecutorOptions, ItemOutcome, ItemResult, Progress, SkipReason,
};
pub use journal::{Journal, JournalEntry, JournalError, JournalOutcome, PerformedAction, UndoInfo};
pub use plan::{Candidate, EntryKind, Plan, PlanItem};
pub use protect::{ProtectError, Protector};
pub use space::{SpaceProbe, SystemSpaceProbe, free_space};
pub use undo::{UndoError, perform_undo, prepare_undo, undo};

/// Cooperative cancellation flag shared between a caller and a running executor.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Creates an un-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Current time in Unix seconds.
#[must_use]
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use bclean_model::{ActionSpec, Classification, ProtectConfig};

    use super::*;

    fn classification(action: ActionSpec, size: u64) -> Classification {
        Classification {
            rule_id: Some("test_rule".to_owned()),
            category: Some("test".to_owned()),
            ephemeral: None,
            confidence: bclean_model::Confidence::High,
            staleness: bclean_model::Staleness::default(),
            size,
            score: 1.0,
            action,
            auto: true,
            reasons: vec![],
        }
    }

    fn plan_for(items: &[(&Path, ActionSpec)]) -> Plan {
        let candidates: Vec<Candidate> = items
            .iter()
            .map(|(path, action)| {
                Candidate::new(
                    *path,
                    EntryKind::of(path).ok(),
                    classification(action.clone(), 100),
                )
            })
            .collect();
        Plan::from_candidates(&candidates)
    }

    fn executor(journal: Option<PathBuf>, dry_run: bool) -> Executor {
        Executor::new(
            Protector::builtin(),
            ExecutorOptions {
                dry_run,
                busy_window: Duration::ZERO,
                check_processes: false,
                command_timeout: Duration::from_secs(20),
                journal_path: journal,
            },
        )
    }

    fn make_tree(root: &Path) {
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file"), b"data").unwrap();
    }

    #[test]
    fn plan_serializes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        make_tree(&target);
        let plan = plan_for(&[(&target, ActionSpec::Delete)]);
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"kind\":\"DELETE\""));
        assert!(json.contains("\"DIR\""));
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);
        assert_eq!(plan.estimated_bytes(), 100);
        assert_eq!(plan.items[0].reason, "matched rule test_rule");
    }

    #[test]
    fn dry_run_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        make_tree(&target);
        let journal = dir.path().join("state/journal.sqlite3");
        let plan = plan_for(&[(&target, ActionSpec::Delete)]);
        let report =
            executor(Some(journal.clone()), true).execute(&plan, &CancelToken::new(), |_| {});
        assert!(report.dry_run);
        assert_eq!(report.results[0].outcome, ItemOutcome::DryRun);
        assert_eq!(report.estimated_freed, 100);
        assert!(target.join("sub/file").exists());
        assert!(!journal.exists(), "the executor never writes the journal");
        assert_eq!(
            report.journal_entries().count(),
            0,
            "dry runs produce no records"
        );

        // Shadow plans are dry runs even on a real executor.
        let mut shadow = plan;
        shadow.shadow = true;
        let report = executor(None, false).execute(&shadow, &CancelToken::new(), |_| {});
        assert!(report.dry_run);
        assert!(target.join("sub/file").exists());
    }

    #[tokio::test]
    async fn deletes_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b.log");
        make_tree(&a);
        std::fs::write(&b, b"log").unwrap();
        let plan = plan_for(&[(&a, ActionSpec::Delete), (&b, ActionSpec::Delete)]);
        let mut events = Vec::new();
        let mut removed = [0; 2];
        let report = executor(Some(dir.path().join("j/journal.sqlite3")), false).execute(
            &plan,
            &CancelToken::new(),
            |p| match p {
                Progress::Started { index, .. } => events.push(format!("start {index}")),
                Progress::Finished { index, .. } => events.push(format!("done {index}")),
                Progress::Advanced {
                    index, removed: n, ..
                } => removed[index] = n,
            },
        );
        assert_eq!(events, ["start 0", "done 0", "start 1", "done 1"]);
        // Every removed entry is reported: the tree's entries, and the single file.
        assert!(removed[0] > 1, "{removed:?}");
        assert_eq!(removed[1], 1);
        assert!(
            report
                .results
                .iter()
                .all(|r| r.outcome == ItemOutcome::Done)
        );
        assert!(!a.exists() && !b.exists());
        assert_eq!(report.estimated_freed, 200);
        assert!(report.measured_freed.is_some());
        assert_eq!(report.journal_entries().count(), 2);
        assert!(!dir.path().join("j/journal.sqlite3").exists());
        let journal = Journal::open(dir.path().join("j/journal.sqlite3"))
            .await
            .unwrap();
        journal.record(&report).await.unwrap();
        let entries = journal.entries().await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(Some(&entries[0].id), report.results[0].journal_id.as_ref());
        assert_eq!(entries[0].outcome, JournalOutcome::Done);
    }

    #[test]
    fn protected_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep");
        make_tree(&keep);
        let pattern = format!("{}/kee?", dir.path().to_string_lossy().replace('\\', "/"));
        let protector = Protector::from_config(&ProtectConfig {
            paths: vec![pattern],
            ..ProtectConfig::default()
        })
        .unwrap();
        let exec = Executor::new(
            protector,
            ExecutorOptions {
                busy_window: Duration::ZERO,
                check_processes: false,
                ..ExecutorOptions::default()
            },
        );
        let mut plan = plan_for(&[
            (&keep, ActionSpec::Delete),
            (&keep.join("sub"), ActionSpec::Delete),
        ]);
        // Hardcoded never-touch paths, even if a plan contains them.
        if let Some(home) = dirs::home_dir() {
            let mut item = plan.items[0].clone();
            item.path = home;
            item.kind = None;
            plan.items.push(item);
        }
        let report = exec.execute(&plan, &CancelToken::new(), |_| {});
        for result in &report.results {
            assert!(
                matches!(
                    result.outcome,
                    ItemOutcome::Skipped(SkipReason::Protected(_))
                ),
                "{result:?}"
            );
        }
        assert!(keep.join("sub/file").exists());

        // The journal directory is protected automatically.
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let exec = executor(Some(state.join("journal.sqlite3")), false);
        let plan = plan_for(&[(&state, ActionSpec::Delete)]);
        let report = exec.execute(&plan, &CancelToken::new(), |_| {});
        assert!(matches!(
            report.results[0].outcome,
            ItemOutcome::Skipped(SkipReason::Protected(_))
        ));
        assert!(state.exists());
    }

    #[test]
    fn missing_and_changed_type_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let changed = dir.path().join("changed");
        make_tree(&gone);
        make_tree(&changed);
        let plan = plan_for(&[(&gone, ActionSpec::Delete), (&changed, ActionSpec::Delete)]);
        std::fs::remove_dir_all(&gone).unwrap();
        std::fs::remove_dir_all(&changed).unwrap();
        std::fs::write(&changed, b"now a file").unwrap();
        let report = executor(None, false).execute(&plan, &CancelToken::new(), |_| {});
        assert_eq!(
            report.results[0].outcome,
            ItemOutcome::Skipped(SkipReason::Missing)
        );
        assert_eq!(
            report.results[1].outcome,
            ItemOutcome::Skipped(SkipReason::TypeChanged {
                expected: EntryKind::Dir,
                found: EntryKind::File
            })
        );
        assert!(changed.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_items_remove_only_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        make_tree(&real);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let plan = plan_for(&[(&link, ActionSpec::Delete)]);
        assert_eq!(plan.items[0].kind, Some(EntryKind::Symlink));
        let report = executor(None, false).execute(&plan, &CancelToken::new(), |_| {});
        assert_eq!(report.results[0].outcome, ItemOutcome::Done);
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(real.join("sub/file").exists());
    }

    #[test]
    fn busy_targets_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(target.join("debug/.cargo-lock"), b"").unwrap();
        let plan = plan_for(&[(&target, ActionSpec::Delete)]);
        let exec = Executor::new(
            Protector::builtin(),
            ExecutorOptions {
                check_processes: false,
                ..ExecutorOptions::default()
            },
        );
        let report = exec.execute(&plan, &CancelToken::new(), |_| {});
        assert!(matches!(
            report.results[0].outcome,
            ItemOutcome::Skipped(SkipReason::Busy(_))
        ));
        assert!(target.exists());
    }

    /// A process working in what a trim keeps (a database in `target/dev-db`) doesn't block
    /// the trim, while it does block deleting the whole directory.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn processes_in_kept_content_do_not_block_a_trim() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("dev-db/data")).unwrap();
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        std::fs::write(target.join("debug/deps/libx.rlib"), b"x").unwrap();
        let mut db = std::process::Command::new("sleep")
            .arg("5")
            .current_dir(target.join("dev-db/data"))
            .spawn()
            .unwrap();
        let exec = Executor::new(Protector::builtin(), ExecutorOptions::default());
        let keep = vec![bclean_model::KeepSpec {
            glob: "dev-db/**".to_owned(),
            executable: false,
        }];
        let delete = exec.preflight(&plan_for(&[(&target, ActionSpec::Delete)]).items[0]);
        let trim = plan_for(&[(&target, ActionSpec::Trim { keep })]);
        let report = exec.execute(&trim, &CancelToken::new(), |_| {});
        db.kill().ok();
        db.wait().ok();
        assert!(matches!(delete, Err(SkipReason::Busy(_))), "{delete:?}");
        assert!(
            matches!(report.results[0].outcome, ItemOutcome::Done),
            "{:?}",
            report.results[0]
        );
        assert!(target.join("dev-db/data").is_dir());
        assert!(!target.join("debug").exists());
    }

    #[test]
    fn report_never_acts_and_cancel_stops() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        make_tree(&a);
        let plan = plan_for(&[(&a, ActionSpec::Report)]);
        let report = executor(None, false).execute(&plan, &CancelToken::new(), |_| {});
        assert_eq!(
            report.results[0].outcome,
            ItemOutcome::Skipped(SkipReason::ReportOnly)
        );
        assert!(a.exists());

        let plan = plan_for(&[(&a, ActionSpec::Delete)]);
        let cancel = CancelToken::new();
        cancel.cancel();
        let report = executor(None, false).execute(&plan, &cancel, |_| {});
        assert!(report.cancelled);
        assert_eq!(report.results[0].outcome, ItemOutcome::Cancelled);
        assert!(a.exists());
    }

    #[cfg(unix)]
    #[test]
    fn command_runs_with_templating() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("my target's dir");
        make_tree(&target);
        let action = ActionSpec::Command {
            run: "rm -rf {path} && touch cleaned-{name}".to_owned(),
            cwd: None,
            fallback: bclean_model::FallbackAction::None,
        };
        let plan = plan_for(&[(&target, action)]);
        let report = executor(None, false).execute(&plan, &CancelToken::new(), |_| {});
        assert_eq!(report.results[0].outcome, ItemOutcome::Done, "{report:?}");
        assert_eq!(report.results[0].performed, Some(PerformedAction::Command));
        assert!(!target.exists());
        // cwd defaulted to {parent}.
        assert!(dir.path().join("cleaned-my target's dir").exists());
    }

    #[cfg(unix)]
    #[test]
    fn command_failure_applies_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        make_tree(&a);
        make_tree(&b);
        let missing_tool = ActionSpec::Command {
            run: "bclean-nonexistent-tool clean {path}".to_owned(),
            cwd: Some("{parent}".to_owned()),
            fallback: bclean_model::FallbackAction::Delete,
        };
        let failing = ActionSpec::Command {
            run: "exit 3".to_owned(),
            cwd: None,
            fallback: bclean_model::FallbackAction::None,
        };
        let plan = plan_for(&[(&a, missing_tool), (&b, failing)]);
        let report = executor(None, false).execute(&plan, &CancelToken::new(), |_| {});
        assert_eq!(report.results[0].outcome, ItemOutcome::Done);
        assert!(report.results[0].fallback_used);
        assert_eq!(report.results[0].performed, Some(PerformedAction::Delete));
        assert!(!a.exists());
        assert!(matches!(report.results[1].outcome, ItemOutcome::Failed(_)));
        assert!(b.exists());
    }

    #[tokio::test]
    async fn move_then_undo() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("project");
        make_tree(&src);
        let dest = dir.path().join("cold storage");
        let journal_path = dir.path().join("state/journal.sqlite3");
        let plan = plan_for(&[(
            &src,
            ActionSpec::Move {
                dest: dest.to_string_lossy().into_owned(),
            },
        )]);
        let report =
            executor(Some(journal_path.clone()), false).execute(&plan, &CancelToken::new(), |_| {});
        let result = &report.results[0];
        assert_eq!(result.outcome, ItemOutcome::Done, "{result:?}");
        assert!(!src.exists());
        assert!(dest.join("project/sub/file").exists());
        let id = result.journal_id.clone().unwrap();

        let journal = Journal::open(&journal_path).await.unwrap();
        journal.record(&report).await.unwrap();
        let restored = undo(&journal, &id).await.unwrap();
        assert_eq!(restored, src);
        assert!(src.join("sub/file").exists());
        assert!(!dest.join("project").exists());
        assert!(matches!(
            undo(&journal, &id).await,
            Err(UndoError::AlreadyUndone(_))
        ));
        assert!(journal.is_undone(&id).await.unwrap());
        assert!(matches!(
            undo(&journal, "nope").await,
            Err(UndoError::NotFound(_))
        ));
    }

    #[test]
    fn move_into_itself_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("project");
        make_tree(&src);
        let plan = plan_for(&[(
            &src,
            ActionSpec::Move {
                dest: "{path}/inner".to_owned(),
            },
        )]);
        let report = executor(None, false).execute(&plan, &CancelToken::new(), |_| {});
        assert!(matches!(
            report.results[0].outcome,
            ItemOutcome::Skipped(SkipReason::InvalidDestination(_))
        ));
        assert!(src.join("sub/file").exists());
    }

    #[tokio::test]
    async fn archive_then_undo() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("old");
        make_tree(&src);
        let journal_path = dir.path().join("state/journal.sqlite3");
        let plan = plan_for(&[(
            &src,
            ActionSpec::Archive {
                dest: "{parent}/archives".to_owned(),
            },
        )]);
        let report =
            executor(Some(journal_path.clone()), false).execute(&plan, &CancelToken::new(), |_| {});
        assert_eq!(report.results[0].outcome, ItemOutcome::Done);
        assert!(!src.exists());
        let journal = Journal::open(&journal_path).await.unwrap();
        journal.record(&report).await.unwrap();
        undo(&journal, report.results[0].journal_id.as_ref().unwrap())
            .await
            .unwrap();
        assert_eq!(std::fs::read(src.join("sub/file")).unwrap(), b"data");
    }

    #[tokio::test]
    async fn delete_is_not_reversible() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x");
        make_tree(&src);
        let journal_path = dir.path().join("state/journal.sqlite3");
        let plan = plan_for(&[(&src, ActionSpec::Delete)]);
        let report =
            executor(Some(journal_path.clone()), false).execute(&plan, &CancelToken::new(), |_| {});
        let journal = Journal::open(&journal_path).await.unwrap();
        journal.record(&report).await.unwrap();
        let err = undo(&journal, report.results[0].journal_id.as_ref().unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, UndoError::NotReversible { .. }), "{err}");
    }
}
