//! Plan execution with safety checks, progress, cancellation, measurement, and journaling.
//!
//! # Journaling design
//!
//! [`Executor::execute`] is deliberately **synchronous** and performs no database I/O: all of
//! its work is blocking filesystem work (deletes, archives, shell commands), and keeping the
//! safety checks and the action for each item in one straight-line, non-async function keeps
//! them simple to reason about (no await points between "check" and "act"). Instead of writing
//! the journal itself, the executor builds a [`JournalEntry`] for every attempted item of a real
//! run and returns it in [`ItemResult::journal_entry`] (also delivered with
//! [`Progress::Finished`]). The caller persists them with the async
//! [`crate::Journal::record`] / [`crate::Journal::append`] — after the run, or per item by
//! forwarding entries from the progress callback over a channel when running the executor on a
//! blocking thread (for example `tokio::task::spawn_blocking`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use bclean_model::{ActionSpec, FallbackAction};
use serde::{Deserialize, Serialize};

use crate::archive::{archive_to, move_path};
use crate::busy::check_busy;
use crate::journal::{JournalEntry, JournalOutcome, PerformedAction, UndoInfo, new_entry_id};
use crate::plan::{EntryKind, Plan, PlanItem};
use crate::protect::{Protector, expand_tilde, normalize};
use crate::remove::remove_path;
use crate::shell::{self, render_template};
use crate::space::{SpaceProbe, SystemSpaceProbe};
use crate::{CancelToken, now_unix};

/// Executor settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorOptions {
    /// Check and report, but never modify anything (and produce no journal entries).
    pub dry_run: bool,
    /// Lock files modified within this window mark a target busy (zero disables).
    pub busy_window: Duration,
    /// Look for processes whose working directory is inside a target.
    pub check_processes: bool,
    /// Timeout for `COMMAND` actions.
    pub command_timeout: Duration,
    /// Path of the journal database. Its directory is protected from every action. The
    /// executor never writes it; see the module docs.
    pub journal_path: Option<PathBuf>,
}

impl Default for ExecutorOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            busy_window: Duration::from_secs(120),
            check_processes: true,
            command_timeout: Duration::from_secs(600),
            journal_path: None,
        }
    }
}

/// Why an item was not acted on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SkipReason {
    /// The path is protected.
    Protected(String),
    /// The path no longer exists.
    Missing,
    /// The entry type differs from the one planned.
    TypeChanged {
        /// Planned type.
        expected: EntryKind,
        /// Current type.
        found: EntryKind,
    },
    /// The target appears to be in use.
    Busy(String),
    /// `REPORT` actions never act.
    ReportOnly,
    /// The action's destination is invalid (for example inside the source).
    InvalidDestination(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protected(why) => write!(f, "protected: {why}"),
            Self::Missing => f.write_str("no longer exists"),
            Self::TypeChanged { expected, found } => {
                write!(f, "type changed from {expected:?} to {found:?}")
            }
            Self::Busy(why) => write!(f, "busy: {why}"),
            Self::ReportOnly => f.write_str("report only"),
            Self::InvalidDestination(why) => write!(f, "invalid destination: {why}"),
        }
    }
}

/// Result of one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ItemOutcome {
    /// Completed.
    Done,
    /// Dry run: would have been performed.
    DryRun,
    /// Refused before acting.
    Skipped(SkipReason),
    /// Attempted and failed.
    Failed(String),
    /// Not attempted (or interrupted) because the run was cancelled.
    Cancelled,
}

/// Per-item execution record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemResult {
    /// Plan item id.
    pub item_id: u64,
    /// Target path.
    pub path: PathBuf,
    /// Outcome.
    pub outcome: ItemOutcome,
    /// What was performed (or would be, in a dry run).
    pub performed: Option<PerformedAction>,
    /// A command's fallback was used.
    pub fallback_used: bool,
    /// Estimated bytes freed (the plan estimate, for completed space-freeing actions).
    pub estimated_freed: u64,
    /// Measured change in free space on the item's volume.
    pub measured_freed: Option<i64>,
    /// Id of [`Self::journal_entry`], if any.
    pub journal_id: Option<String>,
    /// Journal record for this item, produced for every attempted item of a real run (not for
    /// dry runs or `REPORT` items). The caller must persist it.
    pub journal_entry: Option<JournalEntry>,
    /// How to reverse the action, if reversible.
    pub undo: Option<UndoInfo>,
    /// Human-readable detail.
    pub detail: Option<String>,
}

/// Summary of a plan run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExecutionReport {
    /// Whether this was a dry run (explicit, or because the plan is shadow).
    pub dry_run: bool,
    /// Policy that produced the plan, if any.
    pub policy_id: Option<String>,
    /// Per-item results in plan order.
    pub results: Vec<ItemResult>,
    /// Sum of estimates for completed (or, in a dry run, would-be) items.
    pub estimated_freed: u64,
    /// Sum of measured free-space changes, when any measurement was available.
    pub measured_freed: Option<i64>,
    /// The run was cancelled before finishing.
    pub cancelled: bool,
}

impl ExecutionReport {
    /// Journal records of every attempted item, in plan order, for the caller to persist.
    pub fn journal_entries(&self) -> impl Iterator<Item = &JournalEntry> {
        self.results.iter().filter_map(|r| r.journal_entry.as_ref())
    }
}

/// Progress notifications.
#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    /// About to process an item.
    Started {
        /// Zero-based position.
        index: usize,
        /// Number of items.
        total: usize,
        /// The item.
        item: &'a PlanItem,
    },
    /// Finished an item.
    Finished {
        /// Zero-based position.
        index: usize,
        /// Number of items.
        total: usize,
        /// The result.
        result: &'a ItemResult,
    },
}

/// Runs [`Plan`]s safely.
pub struct Executor {
    options: ExecutorOptions,
    protector: Protector,
    probe: Box<dyn SpaceProbe>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("options", &self.options)
            .field("protector", &self.protector)
            .finish_non_exhaustive()
    }
}

struct Performed {
    action: PerformedAction,
    fallback_used: bool,
    undo: Option<UndoInfo>,
    frees_space: bool,
    detail: Option<String>,
}

impl Performed {
    const fn simple(action: PerformedAction, frees_space: bool) -> Self {
        Self {
            action,
            fallback_used: false,
            undo: None,
            frees_space,
            detail: None,
        }
    }
}

enum ActError {
    Skip(SkipReason),
    Fail(String),
    Cancelled,
}

impl Executor {
    /// Creates an executor. The journal's directory is automatically protected.
    #[must_use]
    pub fn new(protector: Protector, options: ExecutorOptions) -> Self {
        let protector = match options.journal_path.as_deref().and_then(Path::parent) {
            Some(dir) if !dir.as_os_str().is_empty() => protector.with_protected_dir(dir),
            _ => protector,
        };
        Self {
            options,
            protector,
            probe: Box::new(SystemSpaceProbe),
        }
    }

    /// Replaces the free-space probe (useful for tests).
    #[must_use]
    pub fn with_probe(mut self, probe: impl SpaceProbe + 'static) -> Self {
        self.probe = Box::new(probe);
        self
    }

    /// Options in effect.
    #[must_use]
    pub const fn options(&self) -> &ExecutorOptions {
        &self.options
    }

    /// Checks whether an item may be acted on right now, without modifying anything.
    ///
    /// # Errors
    ///
    /// * [`SkipReason::ReportOnly`] for `REPORT` items.
    /// * [`SkipReason::Protected`] if the path is protected.
    /// * [`SkipReason::Missing`] if the path no longer exists.
    /// * [`SkipReason::TypeChanged`] if its entry type differs from the plan.
    /// * [`SkipReason::Busy`] if it appears to be in use.
    pub fn preflight(&self, item: &PlanItem) -> Result<(), SkipReason> {
        if matches!(item.action, ActionSpec::Report) {
            return Err(SkipReason::ReportOnly);
        }
        self.check_path(&item.path, item.kind)
    }

    fn check_path(&self, path: &Path, expected: Option<EntryKind>) -> Result<(), SkipReason> {
        if let Some(why) = self.protector.check(path) {
            return Err(SkipReason::Protected(why));
        }
        let found = EntryKind::of(path).map_err(|_| SkipReason::Missing)?;
        if let Some(expected) = expected
            && expected != found
        {
            return Err(SkipReason::TypeChanged { expected, found });
        }
        if let Some(why) = check_busy(path, self.options.busy_window, self.options.check_processes)
        {
            return Err(SkipReason::Busy(why));
        }
        Ok(())
    }

    /// Executes `plan` in order. Shadow plans and dry-run executors never modify anything.
    ///
    /// Cancellation is checked between items and while waiting for commands; remaining items
    /// are reported as [`ItemOutcome::Cancelled`]. Journal records are returned, not written
    /// (see the module docs).
    pub fn execute(
        &self,
        plan: &Plan,
        cancel: &CancelToken,
        mut progress: impl FnMut(Progress<'_>),
    ) -> ExecutionReport {
        let dry_run = self.options.dry_run || plan.shadow;
        let total = plan.items.len();
        let mut report = ExecutionReport {
            dry_run,
            policy_id: plan.policy_id.clone(),
            ..ExecutionReport::default()
        };
        for (index, item) in plan.items.iter().enumerate() {
            progress(Progress::Started { index, total, item });
            let mut result = if cancel.is_cancelled() {
                report.cancelled = true;
                blank_result(item, ItemOutcome::Cancelled)
            } else if dry_run {
                self.dry_run_item(item)
            } else {
                self.run_item(item, cancel)
            };
            if matches!(result.outcome, ItemOutcome::Cancelled) {
                report.cancelled = true;
            }
            if !dry_run && !matches!(result.outcome, ItemOutcome::Skipped(SkipReason::ReportOnly)) {
                let entry = journal_entry(plan, item, &result);
                result.journal_id = Some(entry.id.clone());
                result.journal_entry = Some(entry);
            }
            report.estimated_freed = report
                .estimated_freed
                .saturating_add(result.estimated_freed);
            if let Some(measured) = result.measured_freed {
                report.measured_freed = Some(report.measured_freed.unwrap_or(0) + measured);
            }
            progress(Progress::Finished {
                index,
                total,
                result: &result,
            });
            report.results.push(result);
        }
        report
    }

    fn dry_run_item(&self, item: &PlanItem) -> ItemResult {
        if let Err(reason) = self.preflight(item) {
            return blank_result(item, ItemOutcome::Skipped(reason));
        }
        let mut result = blank_result(item, ItemOutcome::DryRun);
        result.performed = planned_action(&item.action);
        if frees_space(&item.action) {
            result.estimated_freed = item.size;
        }
        result.detail = Some(format!(
            "would {} {}",
            describe(&item.action),
            item.path.display()
        ));
        result
    }

    fn run_item(&self, item: &PlanItem, cancel: &CancelToken) -> ItemResult {
        if let Err(reason) = self.preflight(item) {
            return blank_result(item, ItemOutcome::Skipped(reason));
        }
        let before = self.probe.free_space(&item.path).ok();
        let acted = self.act(item, cancel);
        let after = self.probe.free_space(&item.path).ok();
        let measured = match (before, after) {
            (Some(b), Some(a)) => {
                Some(i64::try_from(a).unwrap_or(i64::MAX) - i64::try_from(b).unwrap_or(i64::MAX))
            }
            _ => None,
        };
        match acted {
            Ok(performed) => {
                let mut result = blank_result(item, ItemOutcome::Done);
                result.performed = Some(performed.action);
                result.fallback_used = performed.fallback_used;
                result.estimated_freed = if performed.frees_space { item.size } else { 0 };
                result.measured_freed = measured;
                result.detail = performed.detail;
                result.undo = performed.undo;
                result
            }
            Err(ActError::Skip(reason)) => blank_result(item, ItemOutcome::Skipped(reason)),
            Err(ActError::Fail(msg)) => {
                let mut result = blank_result(item, ItemOutcome::Failed(msg));
                result.measured_freed = measured;
                result
            }
            Err(ActError::Cancelled) => blank_result(item, ItemOutcome::Cancelled),
        }
    }

    fn act(&self, item: &PlanItem, cancel: &CancelToken) -> Result<Performed, ActError> {
        let path = &item.path;
        match &item.action {
            ActionSpec::Report => Err(ActError::Skip(SkipReason::ReportOnly)),
            ActionSpec::Delete => do_delete(path),
            ActionSpec::Trash => do_trash(path),
            ActionSpec::Command { run, cwd, fallback } => {
                self.do_command(item, run, cwd.as_deref(), *fallback, cancel)
            }
            ActionSpec::Archive { dest } => {
                let dest_dir = resolve_dest(path, dest)?;
                let archive = archive_to(path, &dest_dir)
                    .map_err(|e| ActError::Fail(format!("archive failed: {e}")))?;
                remove_path(path).map_err(|e| {
                    ActError::Fail(format!(
                        "archived to {} but removing the original failed: {e}",
                        archive.display()
                    ))
                })?;
                Ok(Performed {
                    action: PerformedAction::Archive,
                    fallback_used: false,
                    undo: Some(UndoInfo::Archive {
                        original: path.clone(),
                        archive: archive.clone(),
                    }),
                    frees_space: true,
                    detail: Some(format!("archived to {}", archive.display())),
                })
            }
            ActionSpec::Move { dest } => {
                let dest_dir = resolve_dest(path, dest)?;
                let name = path.file_name().ok_or_else(|| {
                    ActError::Skip(SkipReason::InvalidDestination("path has no name".into()))
                })?;
                let target = dest_dir.join(name);
                if let Some(why) = self.protector.check(&target) {
                    return Err(ActError::Skip(SkipReason::InvalidDestination(format!(
                        "destination protected: {why}"
                    ))));
                }
                move_path(path, &target)
                    .map_err(|e| ActError::Fail(format!("move failed: {e}")))?;
                Ok(Performed {
                    action: PerformedAction::Move,
                    fallback_used: false,
                    undo: Some(UndoInfo::Move {
                        from: path.clone(),
                        to: target.clone(),
                    }),
                    frees_space: true,
                    detail: Some(format!("moved to {}", target.display())),
                })
            }
        }
    }
}

/// Resolves a templated destination directory, refusing destinations inside the source.
fn resolve_dest(path: &Path, dest: &str) -> Result<PathBuf, ActError> {
    let rendered = expand_tilde(&render_template(dest, path, false));
    let dest_dir = if rendered.is_absolute() {
        rendered
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new(""))
            .join(rendered)
    };
    let dest_dir = normalize(&dest_dir);
    if dest_dir.starts_with(normalize(path)) {
        return Err(ActError::Skip(SkipReason::InvalidDestination(format!(
            "{} is inside the source",
            dest_dir.display()
        ))));
    }
    Ok(dest_dir)
}

impl Executor {
    fn do_command(
        &self,
        item: &PlanItem,
        run: &str,
        cwd: Option<&str>,
        fallback: FallbackAction,
        cancel: &CancelToken,
    ) -> Result<Performed, ActError> {
        let path = &item.path;
        let line = render_template(run, path, true);
        let cwd = PathBuf::from(render_template(cwd.unwrap_or("{parent}"), path, false));
        let failure = match shell::run(&line, &cwd, self.options.command_timeout, cancel) {
            Ok(outcome) if outcome.success() => {
                return Ok(Performed {
                    detail: Some(format!("ran `{line}`")),
                    ..Performed::simple(PerformedAction::Command, true)
                });
            }
            Ok(outcome) if outcome.cancelled => return Err(ActError::Cancelled),
            Ok(outcome) if outcome.timed_out => format!("`{line}` timed out"),
            Ok(outcome) if outcome.not_found() => {
                format!("`{line}`: tool not found ({})", outcome.stderr.trim())
            }
            Ok(outcome) => format!(
                "`{line}` exited with {:?}: {}",
                outcome.code,
                outcome.stderr.trim()
            ),
            Err(err) => format!("could not run `{line}` in {}: {err}", cwd.display()),
        };
        let fallback_fn: fn(&Path) -> Result<Performed, ActError> = match fallback {
            FallbackAction::None => return Err(ActError::Fail(failure)),
            FallbackAction::Delete => do_delete,
            FallbackAction::Trash => do_trash,
        };
        // The command may have partially run: re-check before falling back.
        match self.check_path(path, item.kind) {
            Ok(()) => {}
            Err(SkipReason::Missing) => {
                return Ok(Performed {
                    detail: Some(format!("{failure}; target is gone, no fallback needed")),
                    ..Performed::simple(PerformedAction::Command, true)
                });
            }
            Err(reason) => return Err(ActError::Skip(reason)),
        }
        let mut performed = fallback_fn(path)?;
        performed.fallback_used = true;
        performed.detail = Some(format!("{failure}; fell back to {fallback:?}"));
        Ok(performed)
    }
}

fn do_delete(path: &Path) -> Result<Performed, ActError> {
    remove_path(path).map_err(|e| ActError::Fail(format!("delete failed: {e}")))?;
    Ok(Performed::simple(PerformedAction::Delete, true))
}

fn do_trash(path: &Path) -> Result<Performed, ActError> {
    let trashed_at = now_unix();
    trash::delete(path).map_err(|e| ActError::Fail(format!("trash failed: {e}")))?;
    Ok(Performed {
        undo: Some(UndoInfo::Trash {
            original: path.to_path_buf(),
            trashed_at,
        }),
        ..Performed::simple(PerformedAction::Trash, false)
    })
}

const fn frees_space(action: &ActionSpec) -> bool {
    !matches!(action, ActionSpec::Trash | ActionSpec::Report)
}

const fn planned_action(action: &ActionSpec) -> Option<PerformedAction> {
    match action {
        ActionSpec::Delete => Some(PerformedAction::Delete),
        ActionSpec::Trash => Some(PerformedAction::Trash),
        ActionSpec::Command { .. } => Some(PerformedAction::Command),
        ActionSpec::Archive { .. } => Some(PerformedAction::Archive),
        ActionSpec::Move { .. } => Some(PerformedAction::Move),
        ActionSpec::Report => None,
    }
}

fn describe(action: &ActionSpec) -> String {
    match action {
        ActionSpec::Delete => "delete".to_owned(),
        ActionSpec::Trash => "move to trash".to_owned(),
        ActionSpec::Command { run, .. } => format!("run `{run}` for"),
        ActionSpec::Archive { dest } => format!("archive (into {dest}) and delete"),
        ActionSpec::Move { dest } => format!("move (into {dest})"),
        ActionSpec::Report => "report".to_owned(),
    }
}

fn blank_result(item: &PlanItem, outcome: ItemOutcome) -> ItemResult {
    let detail = match &outcome {
        ItemOutcome::Skipped(reason) => Some(reason.to_string()),
        ItemOutcome::Failed(msg) => Some(msg.clone()),
        _ => None,
    };
    ItemResult {
        item_id: item.id,
        path: item.path.clone(),
        outcome,
        performed: None,
        fallback_used: false,
        estimated_freed: 0,
        measured_freed: None,
        journal_id: None,
        journal_entry: None,
        undo: None,
        detail,
    }
}

fn journal_entry(plan: &Plan, item: &PlanItem, result: &ItemResult) -> JournalEntry {
    let outcome = match result.outcome {
        ItemOutcome::Done | ItemOutcome::DryRun => JournalOutcome::Done,
        ItemOutcome::Skipped(_) => JournalOutcome::Skipped,
        ItemOutcome::Failed(_) => JournalOutcome::Failed,
        ItemOutcome::Cancelled => JournalOutcome::Cancelled,
    };
    JournalEntry {
        id: new_entry_id(),
        at: now_unix(),
        plan_item_id: Some(item.id),
        policy_id: plan.policy_id.clone(),
        path: item.path.clone(),
        action: item.action.clone(),
        performed: result.performed,
        fallback_used: result.fallback_used,
        size: item.size,
        measured_freed: result.measured_freed,
        outcome,
        detail: result.detail.clone(),
        undo: result.undo.clone(),
        undoes: None,
    }
}
