//! The data-source contract between the TUI and the rest of bclean.
//!
//! The TUI never scans, classifies, or deletes anything itself. Everything goes through a
//! [`Backend`], which the integration layer implements twice: once for local mode (scanner,
//! rules engine, and actions in-process) and once for daemon mode (IPC client).
//!
//! All [`Backend`] methods are *requests*: they must return quickly and never block on I/O.
//! Results are delivered asynchronously as [`BackendEvent`]s through the [`EventSink`] handed
//! to [`Backend::start`]. A backend that can answer instantly (like the in-memory demo backend)
//! may push events from inside the request method itself; slower backends push from worker
//! threads or async tasks. The sink is cheap to clone and safe to use from any thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bclean_model::{ActionSpec, AlertEvent, Classification, VolumeStats};
use bclean_tree::Tree;

/// Source of scan data, classifications, and cleaning for the TUI.
///
/// Implementations must be non-blocking; see the module docs.
pub trait Backend: Send + 'static {
    /// Called exactly once when the UI starts. The backend keeps `sink` and uses it to push
    /// every [`BackendEvent`] (the initial tree or scan progress, classifications, dashboard
    /// data, plan results, and execution progress).
    fn start(&mut self, sink: EventSink);

    /// Requests that a [`bclean_tree::NodeFlags::SUMMARIZED`] directory be materialized.
    ///
    /// Answer with [`BackendEvent::Expanded`] (or [`BackendEvent::Error`]).
    fn expand(&mut self, path: PathBuf);

    /// Requests a rescan of `path` (a directory or the scan root).
    ///
    /// Answer with [`BackendEvent::Expanded`] for a subtree refresh, or with progress events
    /// followed by a new [`BackendEvent::Tree`] for a full rescan.
    fn rescan(&mut self, path: PathBuf);

    /// Hint that the UI is displaying `paths` and would like their classifications soon.
    ///
    /// Backends that classify eagerly can ignore this. Answer with
    /// [`BackendEvent::Classifications`].
    fn request_classifications(&mut self, paths: Vec<PathBuf>) {
        let _ = paths;
    }

    /// Requests a clean plan for the given paths. Answer with [`BackendEvent::Plan`].
    fn build_plan(&mut self, request: PlanRequest);

    /// Executes a previously returned plan. Answer with a stream of
    /// [`BackendEvent::Execution`] events ending in [`ExecutionEvent::Finished`].
    fn execute_plan(&mut self, plan: CleanPlan, dry_run: bool);

    /// Requests cancellation of the running execution (best effort).
    fn cancel_execution(&mut self) {}

    /// Requests fresh dashboard data. Answer with [`BackendEvent::Volumes`],
    /// [`BackendEvent::Alerts`], and/or [`BackendEvent::Status`].
    fn refresh_dashboard(&mut self) {}
}

impl<B: Backend + ?Sized> Backend for Box<B> {
    fn start(&mut self, sink: EventSink) {
        (**self).start(sink);
    }
    fn expand(&mut self, path: PathBuf) {
        (**self).expand(path);
    }
    fn rescan(&mut self, path: PathBuf) {
        (**self).rescan(path);
    }
    fn request_classifications(&mut self, paths: Vec<PathBuf>) {
        (**self).request_classifications(paths);
    }
    fn build_plan(&mut self, request: PlanRequest) {
        (**self).build_plan(request);
    }
    fn execute_plan(&mut self, plan: CleanPlan, dry_run: bool) {
        (**self).execute_plan(plan, dry_run);
    }
    fn cancel_execution(&mut self) {
        (**self).cancel_execution();
    }
    fn refresh_dashboard(&mut self) {
        (**self).refresh_dashboard();
    }
}

/// Cloneable, thread-safe handle a [`Backend`] uses to push events to the UI.
#[derive(Debug, Clone)]
pub struct EventSink {
    tx: tokio::sync::mpsc::UnboundedSender<BackendEvent>,
}

impl EventSink {
    /// Creates a connected sink/receiver pair.
    #[must_use]
    pub fn channel() -> (Self, EventReceiver) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, EventReceiver { rx })
    }

    /// Pushes one event. Returns `false` once the UI has shut down.
    #[allow(clippy::must_use_candidate)] // fire-and-forget is the common use
    pub fn send(&self, event: BackendEvent) -> bool {
        self.tx.send(event).is_ok()
    }

    /// Returns `true` once the UI has shut down and events are discarded.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Receiving half of [`EventSink::channel`].
#[derive(Debug)]
pub struct EventReceiver {
    rx: tokio::sync::mpsc::UnboundedReceiver<BackendEvent>,
}

impl EventReceiver {
    /// Takes one pending event without waiting.
    pub fn try_recv(&mut self) -> Option<BackendEvent> {
        self.rx.try_recv().ok()
    }

    /// Waits for the next event; `None` once every sink is dropped.
    pub async fn recv(&mut self) -> Option<BackendEvent> {
        self.rx.recv().await
    }
}

/// Everything a [`Backend`] can tell the UI.
#[derive(Debug, Clone)]
pub enum BackendEvent {
    /// A complete (possibly partial-scan) tree snapshot. Replaces the current tree; the UI keeps
    /// the current directory and selection by path. Send updated snapshots while a scan runs if
    /// cheap, or only once at the end.
    Tree(Arc<Tree>),
    /// Scan progress. Shown full-screen until the first tree arrives and in the status bar
    /// afterwards.
    Progress(ScanProgress),
    /// A materialized subtree for `path` (an answer to [`Backend::expand`] or a subtree rescan).
    /// The subtree's root corresponds to `path`.
    Expanded {
        /// Absolute path the subtree replaces.
        path: PathBuf,
        /// The subtree, rooted at `path`.
        subtree: Arc<Tree>,
    },
    /// Classifications keyed by absolute path. May arrive at any time, in any number of batches.
    Classifications(Vec<(PathBuf, Classification)>),
    /// Drops all known classifications (for example after a rules reload).
    ClearClassifications,
    /// Answer to [`Backend::build_plan`].
    Plan(CleanPlan),
    /// Plan execution progress.
    Execution(ExecutionEvent),
    /// Volume usage for the dashboard.
    Volumes(Vec<VolumeStats>),
    /// Currently active alerts for the dashboard.
    Alerts(Vec<AlertEvent>),
    /// Backend / daemon status line.
    Status(BackendStatus),
    /// A non-fatal error to show to the user.
    Error(String),
}

/// Scan progress counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanProgress {
    /// Root being scanned.
    pub root: PathBuf,
    /// `false` once the scan has completed (or was cancelled).
    pub running: bool,
    /// Files seen so far.
    pub files: u64,
    /// Directories seen so far.
    pub dirs: u64,
    /// Apparent bytes seen so far.
    pub apparent_bytes: u64,
    /// Allocated bytes seen so far.
    pub allocated_bytes: u64,
    /// Entries that could not be read.
    pub errors: u64,
    /// Path currently being read, if known.
    pub current: Option<PathBuf>,
    /// Time since the scan started.
    pub elapsed: Duration,
    /// Expected total entries (for example from the previous index), if known.
    pub expected_entries: Option<u64>,
}

impl ScanProgress {
    /// Total entries seen so far.
    #[must_use]
    pub const fn entries(&self) -> u64 {
        self.files.saturating_add(self.dirs)
    }
}

/// How the backend runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendMode {
    /// Scanner, rules, and actions run in-process.
    #[default]
    Local,
    /// Connected to the bclean daemon over IPC.
    Daemon,
}

/// Status line data for the dashboard and status bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendStatus {
    /// Local or daemon mode.
    pub mode: BackendMode,
    /// Whether the backend (daemon connection) is healthy.
    pub connected: bool,
    /// Daemon/application version, if known.
    pub version: Option<String>,
    /// Free-form detail ("watching 3 roots", "last scan 5m ago", ...).
    pub detail: Option<String>,
}

/// Which action to apply to plan items.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ActionOverride {
    /// Use each item's classified action (falling back to the backend default).
    #[default]
    RuleDefault,
    /// Force one action for every item (for example ncdu-style delete-in-place).
    Force(ActionSpec),
}

/// Request for [`Backend::build_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRequest {
    /// Request id echoed back in [`CleanPlan::request_id`].
    pub request_id: u64,
    /// Absolute paths to clean. Never contains a path and one of its descendants.
    pub paths: Vec<PathBuf>,
    /// Action selection.
    pub action: ActionOverride,
}

/// One item of a [`CleanPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    /// Absolute path.
    pub path: PathBuf,
    /// Action that will be applied.
    pub action: ActionSpec,
    /// Estimated bytes freed.
    pub estimated_bytes: u64,
    /// Why this item will be skipped (protected path, busy, ...), if it will.
    pub blocked: Option<String>,
    /// Extra information ("in use by pid 123 recently", rule id, ...).
    pub note: Option<String>,
}

/// A clean plan built by the backend.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanPlan {
    /// Backend-assigned plan id.
    pub id: u64,
    /// Id of the [`PlanRequest`] this answers.
    pub request_id: u64,
    /// Items in execution order.
    pub items: Vec<PlanItem>,
}

impl CleanPlan {
    /// Sum of estimated bytes of items that are not blocked.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|item| item.blocked.is_none())
            .map(|item| item.estimated_bytes)
            .fold(0, u64::saturating_add)
    }

    /// Number of items that will be acted on.
    #[must_use]
    pub fn actionable(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.blocked.is_none())
            .count()
    }
}

/// Result of executing one plan item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemOutcome {
    /// Done; `freed_bytes` is the measured amount if known.
    Done {
        /// Measured bytes freed, if the backend measured it.
        freed_bytes: Option<u64>,
    },
    /// Skipped (protected, busy, dry run, cancelled, ...).
    Skipped {
        /// Why.
        reason: String,
    },
    /// Failed.
    Failed {
        /// Error message.
        error: String,
    },
}

/// Streamed plan execution progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionEvent {
    /// Execution of `plan_id` began.
    Started {
        /// Plan id.
        plan_id: u64,
    },
    /// Item `index` of the plan is being processed.
    ItemStarted {
        /// Index into [`CleanPlan::items`].
        index: usize,
    },
    /// Item `index` finished.
    ItemFinished {
        /// Index into [`CleanPlan::items`].
        index: usize,
        /// Outcome.
        outcome: ItemOutcome,
    },
    /// Execution finished (successfully, partially, or cancelled).
    Finished(CleanReport),
}

/// Final report of a plan execution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanReport {
    /// Plan id.
    pub plan_id: u64,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Sum of estimates of the items that were attempted.
    pub estimated_bytes: u64,
    /// Measured bytes freed (for example volume free-space delta), if known.
    pub measured_freed_bytes: Option<u64>,
    /// Items completed.
    pub done: usize,
    /// Items skipped.
    pub skipped: usize,
    /// Items failed.
    pub failed: usize,
    /// Whether execution was cancelled before the end.
    pub cancelled: bool,
    /// Wall time.
    pub elapsed: Duration,
    /// Journal location or other follow-up information, if any.
    pub journal: Option<String>,
}
