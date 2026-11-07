#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! The bclean engine: everything the daemon and local (`--no-daemon`) mode share.
//!
//! An [`Engine`] owns the loaded configuration and compiled rules, the persistent index
//! ([`bclean_index::Store`]: snapshots + history), the action [`Journal`], the in-memory trees
//! and classifications of every indexed root, the policy engine, and the alert router.
//!
//! * Scans run on `spawn_blocking` with the rules prune oracle ([`RulesOracle`]), are
//!   classified (with staleness), persisted as snapshots, and announced as protocol
//!   [`Event`]s on a broadcast channel ([`Engine::subscribe`]).
//! * Incremental updates ([`Engine::apply_dirty`], [`Engine::apply_batch`],
//!   [`Engine::rescan`]) rescan directories, graft them into the tree, re-classify the affected
//!   subtrees, bump the root's generation, and persist.
//! * Cleaning builds [`Plan`]s from paths ([`Engine::plan_for_paths`]) and executes them through
//!   the actions [`bclean_actions::Executor`], journaling every attempted item as it finishes.
//! * Monitoring samples volumes, records history rollups with retention, evaluates policies
//!   (prefetching history into a [`MemoryHistory`]), executes shadow/enforced plans, and raises
//!   alerts.
//!
//! The engine is UI-agnostic: it never prints and never talks to sockets.

mod dashboard;
mod refresh;
mod select;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bclean_actions::{
    CancelToken, Candidate, ExecutionReport, Executor, ExecutorOptions, ItemOutcome, ItemResult,
    Journal, JournalEntry, JournalError, Plan, Progress, ProtectError, Protector, UndoError,
};
use bclean_alerts::{AlertRouter, BmuxBuffer};
use bclean_config::{AppDirs, ConfigError, ExpandError, Expander, LoadOptions, LoadedConfig};
use bclean_index::{HistoryError, SnapshotMeta, Store, StoreError};
use bclean_model::{
    ActionSpec, AlertEvent, AlertSink, BudgetSpec, Classification, Confidence, Config, Reason,
    SelectSpec, Staleness, TriggerSpec, VolumeStats,
};
use bclean_policy::{Firing, MemoryHistory, PolicyEngine, PolicyError};
use bclean_protocol::{
    ClassifiedPath, DaemonStatus, Event, GrowthEntry, RootStatus, ScanProgressInfo, SnapshotRef,
};
use bclean_rules::{Explanation, RuleError, RuleSet};
use bclean_scan::{ProgressSnapshot, ScanBackend, ScanError, ScanOptions, ScanProgress};
use bclean_staleness::unix_now;
use bclean_tree::{NodeId, SizeMode, Tree};
use bclean_watch::{ChangeBatch, VolumeError, WatchConfig, WatchCursor};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};

pub use refresh::{Refresh, RefreshMode, RulesOracle};
pub use select::{Selection, SortOrder, sort_items};

/// Lookback used when prefetching volume samples for policy evaluation.
const VOLUME_LOOKBACK_SECS: i64 = 30 * 86_400;

/// Errors from the engine.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Configuration could not be loaded.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A path could not be expanded.
    #[error(transparent)]
    Expand(#[from] ExpandError),
    /// Rules failed to compile.
    #[error(transparent)]
    Rules(#[from] RuleError),
    /// Index store failure.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// History database failure.
    #[error(transparent)]
    History(#[from] HistoryError),
    /// Journal failure.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// Scan failure.
    #[error(transparent)]
    Scan(#[from] ScanError),
    /// Invalid protected-path pattern.
    #[error(transparent)]
    Protect(#[from] ProtectError),
    /// Invalid policy.
    #[error(transparent)]
    Policy(#[from] PolicyError),
    /// Undo failure.
    #[error(transparent)]
    Undo(#[from] UndoError),
    /// Volume enumeration failure.
    #[error(transparent)]
    Volume(#[from] VolumeError),
    /// Filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Something requested does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// Invalid request.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A background task panicked or was cancelled.
    #[error("background task failed: {0}")]
    Join(String),
}

impl EngineError {
    /// Stable machine code for IPC errors.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_)
            | Self::Undo(UndoError::NotFound(_))
            | Self::Scan(ScanError::Root { .. }) => "NOT_FOUND",
            Self::Invalid(_)
            | Self::Config(_)
            | Self::Expand(_)
            | Self::Rules(_)
            | Self::Protect(_)
            | Self::Policy(_)
            | Self::Undo(_) => "INVALID",
            _ => "INTERNAL",
        }
    }
}

impl From<tokio::task::JoinError> for EngineError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Join(e.to_string())
    }
}

/// Result alias.
pub type Result<T, E = EngineError> = std::result::Result<T, E>;

/// How to construct an [`Engine`].
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Configuration layers to load.
    pub load: LoadOptions,
    /// Override of the data directory (snapshots, history, journal).
    pub data_dir: Option<PathBuf>,
}

impl EngineOptions {
    /// Options for the current process with an optional explicit `--config` file.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Config`] if the working directory cannot be determined
    pub fn from_process(config: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            load: LoadOptions::from_process()?.with_explicit(config),
            data_dir: None,
        })
    }
}

/// Loaded configuration plus compiled rules.
#[derive(Debug)]
pub struct Settings {
    /// Composed configuration with provenance.
    pub loaded: LoadedConfig,
    /// Compiled rules.
    pub rules: Arc<RuleSet>,
    /// Expander used for `~` / env expansion.
    pub expander: Expander,
}

impl Settings {
    /// `monitor.index_roots`, expanded and made absolute (missing directories are skipped).
    #[must_use]
    pub fn index_roots(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self
            .config()
            .monitor
            .index_roots
            .iter()
            .filter_map(|r| match self.expander.expand_path(r) {
                Ok(p) => std::path::absolute(p).ok(),
                Err(e) => {
                    log::warn!("cannot expand index root {r}: {e}");
                    None
                }
            })
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Loads configuration and compiles its rules.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Config`] if loading fails
    /// * [`EngineError::Rules`] if a rule does not compile
    pub fn load(options: &LoadOptions) -> Result<Self> {
        let loaded = bclean_config::load(options)?;
        let rules = RuleSet::compile(&loaded.config, &options.expander)?;
        for warning in rules.warnings() {
            log::debug!("rule {}: {}", warning.rule, warning.message);
        }
        Ok(Self {
            loaded,
            rules: Arc::new(rules),
            expander: options.expander.clone(),
        })
    }

    /// The composed configuration.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.loaded.config
    }
}

/// Result of a full scan.
#[derive(Debug, Clone)]
pub struct ScanReport {
    /// Snapshot written for the root.
    pub snapshot: SnapshotRef,
    /// Wall time.
    pub elapsed: Duration,
    /// Paths that could not be read.
    pub issues: usize,
    /// Counters.
    pub stats: ProgressSnapshot,
    /// Backend that ran.
    pub backend: ScanBackend,
}

/// Action execution progress, published as JSON in [`Event::ActionProgress`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionProgress {
    /// A plan started executing.
    Started {
        /// Number of items.
        total: usize,
        /// Dry run.
        dry_run: bool,
    },
    /// An item is being processed.
    ItemStarted {
        /// Zero-based index.
        index: usize,
        /// Number of items.
        total: usize,
        /// Target path.
        path: PathBuf,
    },
    /// Periodic progress inside a long-running item.
    ItemAdvanced {
        /// Zero-based index.
        index: usize,
        /// Number of items.
        total: usize,
        /// Entries removed so far.
        removed: u64,
        /// Expected entries, if known from the index.
        entries: Option<u64>,
    },
    /// An item finished.
    ItemFinished {
        /// Zero-based index.
        index: usize,
        /// Number of items.
        total: usize,
        /// Result.
        result: Box<ItemResult>,
    },
    /// The plan finished.
    Finished {
        /// Dry run.
        dry_run: bool,
        /// Estimated bytes freed.
        estimated_freed: u64,
        /// Measured bytes freed, if known.
        measured_freed: Option<i64>,
        /// Cancelled before the end.
        cancelled: bool,
    },
}

/// One policy firing and what was done about it.
#[derive(Debug, Clone)]
pub struct PolicyOutcome {
    /// The firing.
    pub firing: Firing,
    /// The plan built for it, if any.
    pub plan: Option<Plan>,
    /// Execution report (dry run for shadow plans), if a plan was executed.
    pub report: Option<ExecutionReport>,
}

/// Directory under the data dir for expanded subtrees handed to IPC clients.
const EXPANDED_DIR: &str = "expanded";

#[derive(Debug, Default)]
struct RootState {
    tree: Option<Arc<Tree>>,
    classes: BTreeMap<PathBuf, Classification>,
    generation: u64,
    scanned_at: Option<i64>,
    cursor: Option<Vec<u8>>,
    backend: String,
    progress: Option<Arc<ScanProgress>>,
    /// When the in-memory tree first diverged from the persisted snapshot.
    unsaved_since: Option<Instant>,
    /// Last re-measurement of each recently refreshed summarized directory.
    summary_refreshed: BTreeMap<PathBuf, Instant>,
    /// Summarized directories with changes whose re-measurement was postponed.
    deferred_summaries: BTreeSet<PathBuf>,
    /// Full scans completed since the engine started.
    full_scans: u64,
    /// Incremental refreshes applied since the engine started.
    refreshes: u64,
    /// Watcher-requested subtree rescans waiting for the rescan budget.
    pending_rescans: BTreeSet<PathBuf>,
    /// Earliest start of the next watcher-requested rescan (see
    /// `monitor.rescan_budget_percent`).
    rescan_not_before: Option<Instant>,
}

#[derive(Debug)]
struct RootSlot {
    root: PathBuf,
    op: tokio::sync::Mutex<()>,
    state: RwLock<RootState>,
}

struct Inner {
    dirs: AppDirs,
    data_dir: PathBuf,
    load: LoadOptions,
    settings: RwLock<Arc<Settings>>,
    store: Store,
    journal: Journal,
    roots: Mutex<BTreeMap<PathBuf, Arc<RootSlot>>>,
    events: broadcast::Sender<Event>,
    router: RwLock<Arc<AlertRouter>>,
    bmux: BmuxBuffer,
    policy: tokio::sync::Mutex<PolicyEngine>,
    started_at: i64,
    watchers: Mutex<Vec<String>>,
    exec_lock: tokio::sync::Mutex<()>,
    /// Dashboard volume trends, keyed by mount point.
    volume_trends: Mutex<BTreeMap<String, dashboard::VolumeTrend>>,
}

/// The shared bclean engine. Cheap to clone (all state is behind an `Arc`).
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("data_dir", &self.inner.data_dir)
            .field("roots", &self.roots())
            .finish_non_exhaustive()
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

fn build_router(config: &Config, bmux: &BmuxBuffer) -> AlertRouter {
    AlertRouter::new(config.alerts.clone()).with_bmux_buffer(bmux.clone())
}

impl Engine {
    /// Loads configuration, compiles rules, and opens the store and journal.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Config`] / [`EngineError::Rules`] / [`EngineError::Policy`] for invalid
    ///   configuration
    /// * [`EngineError::Expand`] if the application directories cannot be determined
    /// * [`EngineError::Store`] / [`EngineError::Journal`] if the databases cannot be opened
    pub async fn open(options: EngineOptions) -> Result<Self> {
        let settings = Settings::load(&options.load)?;
        let dirs = options.load.expander.app_dirs()?;
        let data_dir = options
            .data_dir
            .clone()
            .unwrap_or_else(|| dirs.data.clone());
        std::fs::create_dir_all(&data_dir)?;
        // Expanded-subtree files are only meaningful to the client they were written for.
        let _ = std::fs::remove_dir_all(data_dir.join(EXPANDED_DIR));
        let store = Store::open(&data_dir).await?;
        let journal = Journal::open(data_dir.join("journal.sqlite3")).await?;
        let policy = PolicyEngine::new(settings.config().policies.clone())?;
        let bmux = BmuxBuffer::new(256);
        let router = build_router(settings.config(), &bmux);
        let (events, _) = broadcast::channel(1024);
        Ok(Self {
            inner: Arc::new(Inner {
                dirs,
                data_dir,
                load: options.load,
                settings: RwLock::new(Arc::new(settings)),
                store,
                journal,
                roots: Mutex::new(BTreeMap::new()),
                events,
                router: RwLock::new(Arc::new(router)),
                bmux,
                policy: tokio::sync::Mutex::new(policy),
                started_at: unix_now(),
                watchers: Mutex::new(Vec::new()),
                volume_trends: Mutex::new(BTreeMap::new()),
                exec_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Current settings.
    #[must_use]
    pub fn settings(&self) -> Arc<Settings> {
        Arc::clone(&self.inner.settings.read())
    }

    /// Application directories.
    #[must_use]
    pub fn dirs(&self) -> &AppDirs {
        &self.inner.dirs
    }

    /// Data directory in use.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// Journal database path.
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.inner.data_dir.join("journal.sqlite3")
    }

    /// Snapshot file path for `root`.
    #[must_use]
    pub fn snapshot_path(&self, root: &Path) -> PathBuf {
        self.inner.store.snapshot_path(root)
    }

    /// Subscribes to engine events.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    fn emit(&self, event: Event) {
        let _ = self.inner.events.send(event);
    }

    /// Reloads configuration from disk, rebuilding rules, policies, and alert sinks, and
    /// re-classifying every loaded root.
    ///
    /// # Errors
    ///
    /// * Same as [`Settings::load`] and [`PolicyEngine::new`]; on error the old settings stay
    pub async fn reload(&self) -> Result<()> {
        let load = self.inner.load.clone();
        let settings = tokio::task::spawn_blocking(move || Settings::load(&load)).await??;
        let policy = PolicyEngine::new(settings.config().policies.clone())?;
        *self.inner.router.write() = Arc::new(build_router(settings.config(), &self.inner.bmux));
        let rules = Arc::clone(&settings.rules);
        *self.inner.settings.write() = Arc::new(settings);
        *self.inner.policy.lock().await = policy;
        for slot in self.slots() {
            let _op = slot.op.lock().await;
            let Some(tree) = slot.state.read().tree.clone() else {
                continue;
            };
            let rules = Arc::clone(&rules);
            let classes =
                tokio::task::spawn_blocking(move || refresh::classify_all(&rules, &tree)).await?;
            slot.state.write().classes = classes;
        }
        log::info!("configuration reloaded");
        Ok(())
    }

    // ----- roots -----

    /// Roots with in-memory state.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.inner.roots.lock().keys().cloned().collect()
    }

    fn slots(&self) -> Vec<Arc<RootSlot>> {
        self.inner.roots.lock().values().cloned().collect()
    }

    fn slot(&self, root: &Path) -> Arc<RootSlot> {
        Arc::clone(
            self.inner
                .roots
                .lock()
                .entry(root.to_path_buf())
                .or_insert_with(|| {
                    Arc::new(RootSlot {
                        root: root.to_path_buf(),
                        op: tokio::sync::Mutex::new(()),
                        state: RwLock::new(RootState::default()),
                    })
                }),
        )
    }

    /// The deepest known root containing `path`.
    #[must_use]
    pub fn root_for(&self, path: &Path) -> Option<PathBuf> {
        self.inner
            .roots
            .lock()
            .keys()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
    }

    /// `monitor.index_roots`, expanded and made absolute (missing directories are skipped).
    #[must_use]
    pub fn index_roots(&self) -> Vec<PathBuf> {
        self.settings().index_roots()
    }

    /// The loaded tree for `root`.
    #[must_use]
    pub fn tree(&self, root: &Path) -> Option<Arc<Tree>> {
        let slot = self.inner.roots.lock().get(root).cloned()?;
        let state = slot.state.read();
        state.tree.clone()
    }

    /// Snapshot reference for a loaded root.
    #[must_use]
    pub fn snapshot_ref(&self, root: &Path) -> Option<SnapshotRef> {
        let slot = self.inner.roots.lock().get(root).cloned()?;
        let generation = slot.state.read().generation;
        (generation > 0).then(|| SnapshotRef {
            root: root.to_path_buf(),
            path: self.snapshot_path(root),
            generation,
        })
    }

    /// Snapshot reference for a loaded root whose file is current: pending incremental changes
    /// are saved first. Use it whenever the reference is handed to a reader of the file (IPC
    /// clients); in-process users should read [`Engine::tree`] instead.
    ///
    /// # Errors
    ///
    /// * [`EngineError::NotFound`] if `root` has no snapshot
    /// * [`EngineError::Store`] if the snapshot cannot be written
    pub async fn saved_snapshot_ref(&self, root: &Path) -> Result<SnapshotRef> {
        let slot = self.inner.roots.lock().get(root).cloned();
        if let Some(slot) = slot
            && slot.state.read().unsaved_since.is_some()
        {
            let _op = slot.op.lock().await;
            if slot.state.read().unsaved_since.is_some() {
                self.persist(&slot).await?;
            }
        }
        self.require_ref(root)
    }

    fn require_ref(&self, root: &Path) -> Result<SnapshotRef> {
        self.snapshot_ref(root)
            .ok_or_else(|| EngineError::NotFound(format!("no snapshot for {}", root.display())))
    }

    fn scan_options(settings: &Settings, root: &Path) -> (ScanOptions, RulesOracle) {
        let config = settings.config();
        (
            ScanOptions::from_config(root, &config.scan),
            RulesOracle::new(Arc::clone(&settings.rules), config.scan.summarize_ephemeral),
        )
    }

    fn spawn_progress(
        &self,
        root: PathBuf,
        progress: Arc<ScanProgress>,
    ) -> tokio::task::JoinHandle<()> {
        let events = self.inner.events.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let _ = events.send(Event::ScanProgress {
                    root: root.clone(),
                    progress: progress_info(progress.snapshot()),
                });
            }
        })
    }

    /// Scans `root` from scratch, classifies it, persists the snapshot, and publishes
    /// `TREE_UPDATED`.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Scan`] if the scan fails
    /// * [`EngineError::Store`] if the snapshot cannot be written
    pub async fn scan(&self, root: &Path) -> Result<ScanReport> {
        self.scan_with_cursor(root, None).await
    }

    /// Like [`Engine::scan`], recording `cursor` (taken before the scan started) in the
    /// snapshot metadata.
    ///
    /// # Errors
    ///
    /// * Same as [`Engine::scan`]
    pub async fn scan_with_cursor(
        &self,
        root: &Path,
        cursor: Option<Vec<u8>>,
    ) -> Result<ScanReport> {
        let root = absolute(root)?;
        let slot = self.slot(&root);
        let _op = slot.op.lock().await;
        self.full_scan_locked(&slot, cursor).await
    }

    async fn full_scan_locked(
        &self,
        slot: &Arc<RootSlot>,
        cursor: Option<Vec<u8>>,
    ) -> Result<ScanReport> {
        let settings = self.settings();
        let progress = Arc::new(ScanProgress::new());
        slot.state.write().progress = Some(Arc::clone(&progress));
        let ticker = self.spawn_progress(slot.root.clone(), Arc::clone(&progress));
        let (options, oracle) = Self::scan_options(&settings, &slot.root);
        let rules = Arc::clone(&settings.rules);
        let p = Arc::clone(&progress);
        let result = tokio::task::spawn_blocking(move || {
            let outcome = bclean_scan::scan(&options, &oracle, &p)?;
            let classes = refresh::classify_all(&rules, &outcome.tree);
            Ok::<_, ScanError>((outcome, classes))
        })
        .await;
        ticker.abort();
        slot.state.write().progress = None;
        let (outcome, classes) = result??;
        self.emit(Event::ScanProgress {
            root: slot.root.clone(),
            progress: progress_info(outcome.stats),
        });
        let now = unix_now();
        let cursor = cursor.or_else(|| slot.state.read().cursor.clone());
        {
            let mut state = slot.state.write();
            state.tree = Some(Arc::new(outcome.tree));
            state.classes = classes;
            state.generation += 1;
            state.scanned_at = Some(now);
            state.cursor = cursor;
            state.deferred_summaries.clear();
            state.full_scans += 1;
            state.backend = format!("{:?}", outcome.backend).to_uppercase();
        }
        self.persist(slot).await?;
        let snapshot = self.require_ref(&slot.root)?;
        self.emit(Event::TreeUpdated(snapshot.clone()));
        log::info!(
            "scanned {} in {:.2?} ({} files, {} dirs, {} issues)",
            slot.root.display(),
            outcome.elapsed,
            outcome.stats.files,
            outcome.stats.dirs,
            outcome.issues.len()
        );
        Ok(ScanReport {
            snapshot,
            elapsed: outcome.elapsed,
            issues: outcome.issues.len(),
            stats: outcome.stats,
            backend: outcome.backend,
        })
    }

    async fn persist(&self, slot: &Arc<RootSlot>) -> Result<()> {
        let (tree, meta) = {
            let state = slot.state.read();
            let Some(tree) = state.tree.clone() else {
                return Ok(());
            };
            (
                tree,
                SnapshotMeta {
                    scanned_at: state.scanned_at.unwrap_or_default(),
                    backend: state.backend.clone(),
                    cursor: state.cursor.clone(),
                },
            )
        };
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || inner.store.save_snapshot(&tree, &meta)).await??;
        slot.state.write().unsaved_since = None;
        Ok(())
    }

    /// Persists every root with unsaved changes (for example at shutdown).
    ///
    /// # Errors
    ///
    /// * [`EngineError::Store`] if a snapshot cannot be written
    pub async fn persist_all(&self) -> Result<()> {
        self.persist_unsaved(Duration::ZERO).await
    }

    /// Persists every root whose changes have been unsaved for at least `max_age`.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Store`] if a snapshot cannot be written
    pub async fn persist_unsaved(&self, max_age: Duration) -> Result<()> {
        for slot in self.slots() {
            let due = |slot: &RootSlot| {
                slot.state
                    .read()
                    .unsaved_since
                    .is_some_and(|since| since.elapsed() >= max_age)
            };
            if !due(&slot) {
                continue;
            }
            let _op = slot.op.lock().await;
            if due(&slot) {
                self.persist(&slot).await?;
            }
        }
        Ok(())
    }

    /// Runs postponed work that has become due: rescans waiting for the rescan budget, and
    /// summarized directories whose `monitor.summary_refresh_interval` has elapsed.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Scan`] on scan failures
    pub async fn refresh_deferred(&self) -> Result<()> {
        let interval = self
            .settings()
            .config()
            .monitor
            .summary_refresh_interval
            .duration();
        for slot in self.slots() {
            self.run_due_rescans(&slot).await?;
            let due: Vec<PathBuf> = {
                let state = slot.state.read();
                state
                    .deferred_summaries
                    .iter()
                    .filter(|p| {
                        state
                            .summary_refreshed
                            .get(*p)
                            .is_none_or(|t| t.elapsed() >= interval)
                    })
                    .cloned()
                    .collect()
            };
            if due.is_empty() {
                continue;
            }
            let _op = slot.op.lock().await;
            if slot.state.read().tree.is_none() {
                continue;
            }
            self.refresh_locked(&slot, due, RefreshMode::Shallow)
                .await?;
        }
        Ok(())
    }

    /// Loads the persisted snapshot of `root` (instant), publishes `TREE_UPDATED`, then
    /// classifies it. Returns the snapshot metadata, or `None` if there is no usable snapshot.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Store`] on I/O errors other than a missing or invalid snapshot
    pub async fn load_snapshot(&self, root: &Path) -> Result<Option<SnapshotMeta>> {
        let root = absolute(root)?;
        let slot = self.slot(&root);
        let _op = slot.op.lock().await;
        self.load_snapshot_locked(&slot).await
    }

    async fn load_snapshot_locked(&self, slot: &Arc<RootSlot>) -> Result<Option<SnapshotMeta>> {
        let inner = Arc::clone(&self.inner);
        let root = slot.root.clone();
        let loaded = tokio::task::spawn_blocking(move || inner.store.load_snapshot(&root)).await?;
        let snapshot = match loaded {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Ok(None),
            Err(StoreError::Snapshot(e)) if e.is_invalid() => {
                log::warn!(
                    "ignoring unusable snapshot for {}: {e}",
                    slot.root.display()
                );
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let tree = Arc::new(snapshot.tree);
        {
            let mut state = slot.state.write();
            state.tree = Some(Arc::clone(&tree));
            state.generation += 1;
            state.scanned_at = Some(snapshot.meta.scanned_at);
            state.cursor.clone_from(&snapshot.meta.cursor);
            state.backend.clone_from(&snapshot.meta.backend);
        }
        self.emit(Event::TreeUpdated(self.require_ref(&slot.root)?));
        let rules = Arc::clone(&self.settings().rules);
        let classes =
            tokio::task::spawn_blocking(move || refresh::classify_all(&rules, &tree)).await?;
        slot.state.write().classes = classes;
        Ok(Some(snapshot.meta))
    }

    /// Makes sure `path` is covered by a loaded tree and returns its root's snapshot.
    ///
    /// * Inside a loaded root: reuses it (with `force`, rescans `path` and grafts it).
    /// * Otherwise `path` becomes a root: its persisted snapshot is loaded unless `force`,
    ///   else it is scanned.
    ///
    /// # Errors
    ///
    /// * Same as [`Engine::scan`] and [`Engine::load_snapshot`]
    pub async fn ensure_scanned(&self, path: &Path, force: bool) -> Result<SnapshotRef> {
        let path = absolute(path)?;
        let root = self.root_for(&path).unwrap_or_else(|| path.clone());
        let slot = self.slot(&root);
        let has_tree = slot.state.read().tree.is_some();
        if has_tree && force && root != path {
            return Ok(self.rescan(&path, false).await?.0);
        }
        let _op = slot.op.lock().await;
        let has_tree = slot.state.read().tree.is_some();
        if has_tree && !force {
            return self.require_ref(&root);
        }
        if !force && self.load_snapshot_locked(&slot).await?.is_some() {
            return self.require_ref(&root);
        }
        Ok(self.full_scan_locked(&slot, None).await?.snapshot)
    }

    /// Classified entries at or under `path` (from the loaded tree), highest score first.
    #[must_use]
    pub fn classifications_under(&self, path: &Path) -> Vec<ClassifiedPath> {
        let mut out: Vec<ClassifiedPath> = Vec::new();
        for slot in self.slots() {
            if !slot.root.starts_with(path) && !path.starts_with(&slot.root) {
                continue;
            }
            let state = slot.state.read();
            out.extend(
                state
                    .classes
                    .iter()
                    .filter(|(p, _)| p.starts_with(path))
                    .map(|(p, c)| ClassifiedPath {
                        path: p.clone(),
                        classification: c.clone(),
                    }),
            );
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out.dedup_by(|a, b| a.path == b.path);
        sort_items(&mut out, SortOrder::Score);
        out
    }

    /// Ensures `path` is scanned and returns the classified entries under it.
    ///
    /// # Errors
    ///
    /// * Same as [`Engine::ensure_scanned`]
    pub async fn classifications(&self, path: &Path, force: bool) -> Result<Vec<ClassifiedPath>> {
        let path = absolute(path)?;
        self.ensure_scanned(&path, force).await?;
        Ok(self.classifications_under(&path))
    }

    /// Classifies and explains one path (independently of any tree).
    ///
    /// # Errors
    ///
    /// * [`EngineError::Io`] if the path cannot be read
    pub async fn explain(&self, path: &Path) -> Result<Explanation> {
        let path = absolute(path)?;
        let rules = Arc::clone(&self.settings().rules);
        Ok(tokio::task::spawn_blocking(move || {
            let staleness = bclean_staleness::StalenessEngine::with_defaults(unix_now());
            bclean_rules::Classifier::new(&rules, &staleness).explain(&path)
        })
        .await??)
    }

    // ----- incremental updates -----

    /// Rescans one directory and grafts it (with `expand`, a summarized directory is
    /// materialized) into the deepest loaded root containing it, and returns that root's
    /// snapshot and the rescanned subtree. Outer roots containing it (`/` around `~`) are
    /// updated in the background, each publishing `TREE_UPDATED`, so a client browsing them
    /// stays in sync without the caller waiting; those without a tree in memory are skipped.
    ///
    /// # Errors
    ///
    /// * [`EngineError::NotFound`] if `path` is not inside a loaded root
    /// * [`EngineError::Scan`] / [`EngineError::Store`] on scan or persistence failures
    pub async fn rescan(
        &self,
        path: &Path,
        expand: bool,
    ) -> Result<(SnapshotRef, Option<Arc<Tree>>)> {
        let path = absolute(path)?;
        let mut roots: Vec<PathBuf> = self
            .roots()
            .into_iter()
            .filter(|root| path.starts_with(root))
            .collect();
        roots.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
        let Some(deepest) = roots.first().cloned() else {
            return Err(EngineError::NotFound(format!(
                "{} is not indexed",
                path.display()
            )));
        };
        let outer: Vec<PathBuf> = roots.into_iter().skip(1).collect();
        let result = {
            let slot = self.slot(&deepest);
            let _op = slot.op.lock().await;
            self.refresh_locked(&slot, vec![path.clone()], RefreshMode::Deep { expand })
                .await?
        };
        if !outer.is_empty() {
            let engine = self.clone();
            tokio::spawn(async move {
                for root in outer {
                    let slot = engine.slot(&root);
                    let _op = slot.op.lock().await;
                    if slot.state.read().tree.is_none() {
                        continue;
                    }
                    let mode = RefreshMode::Deep { expand };
                    if let Err(e) = engine.refresh_locked(&slot, vec![path.clone()], mode).await {
                        log::warn!(
                            "rescan of {} in {} failed: {e}",
                            path.display(),
                            root.display()
                        );
                    }
                }
            });
        }
        Ok(result)
    }

    /// Reconciles directories whose own entries changed (grouped by root): each is re-listed,
    /// new entries are scanned, removed ones dropped, and affected entries re-classified, then
    /// `TREE_UPDATED` is published. Unchanged subtrees are never read. Paths outside every
    /// loaded root, and bclean's own data, cache, and runtime directories, are ignored.
    ///
    /// The snapshot is saved later (see [`Engine::persist_unsaved`]).
    ///
    /// # Errors
    ///
    /// * [`EngineError::Scan`] on scan failures
    pub async fn apply_dirty(&self, dirs: Vec<PathBuf>) -> Result<()> {
        self.apply_dirs(dirs, RefreshMode::Shallow).await
    }

    async fn apply_dirs(&self, dirs: Vec<PathBuf>, mode: RefreshMode) -> Result<()> {
        for (root, mut dirs) in self.group_by_root(dirs) {
            dirs.sort();
            dirs.dedup();
            let slot = self.slot(&root);
            let _op = slot.op.lock().await;
            if slot.state.read().tree.is_none() {
                continue;
            }
            self.refresh_locked(&slot, dirs, mode).await?;
        }
        Ok(())
    }

    /// Groups `dirs` by loaded root, dropping paths outside every root and bclean's own paths.
    fn group_by_root(&self, dirs: Vec<PathBuf>) -> BTreeMap<PathBuf, Vec<PathBuf>> {
        let mut by_root: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for dir in dirs {
            if self.is_own_path(&dir) {
                continue;
            }
            if let Some(root) = self.root_for(&dir) {
                by_root.entry(root).or_default().push(dir);
            }
        }
        by_root
    }

    /// Queues complete rescans of subtrees whose change history is incomplete, and runs them
    /// as the rescan budget allows: after a rescan that took `T`, the next one in that root
    /// waits `T × (100 / monitor.rescan_budget_percent − 1)`. Requests arriving meanwhile are
    /// merged and run together once due (from here or [`Engine::refresh_deferred`]).
    ///
    /// Event history is lost under heavy filesystem load (dropped events, coalesced
    /// `MustScanSubDirs`), often for a whole root. Without a budget, every loss of a large
    /// root costs a full scan on every core, back to back for as long as the load lasts.
    async fn request_rescans(&self, dirs: Vec<PathBuf>) -> Result<()> {
        for (root, dirs) in self.group_by_root(dirs) {
            let slot = self.slot(&root);
            {
                let mut state = slot.state.write();
                let wait = state
                    .rescan_not_before
                    .and_then(|t| t.checked_duration_since(Instant::now()));
                if let Some(wait) = wait
                    && state.pending_rescans.is_empty()
                {
                    log::info!(
                        "deferring rescan of {} subtree(s) in {} by {:.0?} (rescan budget)",
                        dirs.len(),
                        root.display(),
                        wait
                    );
                }
                state.pending_rescans.extend(dirs);
            }
            self.run_due_rescans(&slot).await?;
        }
        Ok(())
    }

    /// Runs `slot`'s pending rescans if the rescan budget allows.
    async fn run_due_rescans(&self, slot: &Arc<RootSlot>) -> Result<()> {
        let due = |state: &RootState| {
            !state.pending_rescans.is_empty()
                && state.rescan_not_before.is_none_or(|t| Instant::now() >= t)
        };
        if !due(&slot.state.read()) {
            return Ok(());
        }
        let _op = slot.op.lock().await;
        let dirs = {
            let mut state = slot.state.write();
            if !due(&state) || state.tree.is_none() {
                return Ok(());
            }
            std::mem::take(&mut state.pending_rescans)
        };
        // Nested requests are covered by their ancestors.
        let mut outer: Vec<PathBuf> = Vec::with_capacity(dirs.len());
        for dir in dirs {
            if !outer.last().is_some_and(|o| dir.starts_with(o)) {
                outer.push(dir);
            }
        }
        log::info!(
            "change history incomplete; rescanning {} subtree(s) in {}: {:?}",
            outer.len(),
            slot.root.display(),
            outer.iter().take(5).collect::<Vec<_>>()
        );
        let started = Instant::now();
        let result = self
            .refresh_locked(slot, outer, RefreshMode::Deep { expand: false })
            .await;
        let percent = u32::from(
            self.settings()
                .config()
                .monitor
                .rescan_budget_percent
                .clamp(1, 100),
        );
        let cooldown = started.elapsed() * (100 - percent) / percent;
        slot.state.write().rescan_not_before = Some(Instant::now() + cooldown);
        result.map(drop)
    }

    /// `true` for paths inside bclean's own data, cache, or runtime directories, whose
    /// churn (snapshot writes, databases, the socket) must never feed back into the index.
    fn is_own_path(&self, path: &Path) -> bool {
        let dirs = &self.inner.dirs;
        [&self.inner.data_dir, &dirs.data, &dirs.cache, &dirs.runtime]
            .into_iter()
            .any(|own| path.starts_with(own))
    }

    /// Applies a change-source batch: updates the watch cursor, then reconciles the batch's
    /// directories (shallowly, or as complete subtree rescans when
    /// [`ChangeBatch::must_full_rescan`] is set).
    ///
    /// # Errors
    ///
    /// * Same as [`Engine::apply_dirty`]
    pub async fn apply_batch(&self, batch: ChangeBatch) -> Result<()> {
        if let Some(cursor) = &batch.cursor {
            let bytes = cursor.to_bytes();
            for slot in self.slots() {
                slot.state.write().cursor = Some(bytes.clone());
            }
        }
        if batch.must_full_rescan {
            return self.request_rescans(batch.dirty).await;
        }
        log::debug!("applying change batch: {} dirs", batch.dirty.len());
        self.apply_dirs(batch.dirty, RefreshMode::Shallow).await
    }

    async fn refresh_locked(
        &self,
        slot: &Arc<RootSlot>,
        dirs: Vec<PathBuf>,
        mode: RefreshMode,
    ) -> Result<(SnapshotRef, Option<Arc<Tree>>)> {
        let settings = self.settings();
        let Some(tree) = slot.state.write().tree.take() else {
            return Ok((self.full_scan_locked(slot, None).await?.snapshot, None));
        };
        let interval = settings
            .config()
            .monitor
            .summary_refresh_interval
            .duration();
        let (mut classes, last_refreshed) = {
            let state = slot.state.read();
            (state.classes.clone(), state.summary_refreshed.clone())
        };
        let (options, oracle) = Self::scan_options(&settings, &slot.root);
        let rules = Arc::clone(&settings.rules);
        let started = Instant::now();
        let dirs_count = dirs.len();
        let joined = tokio::task::spawn_blocking(move || {
            let mut tree = Arc::unwrap_or_clone(tree);
            let progress = ScanProgress::new();
            let ctx = refresh::ScanCtx {
                options: &options,
                oracle: &oracle,
                progress: &progress,
            };
            let now = Instant::now();
            let mut refreshed = Vec::new();
            let mut allow = |path: &Path| {
                let due = last_refreshed
                    .get(path)
                    .is_none_or(|t| now.duration_since(*t) >= interval);
                if due {
                    refreshed.push(path.to_path_buf());
                }
                due
            };
            let result = refresh::refresh_dirs(&mut tree, &dirs, mode, &ctx, &mut allow);
            if let Ok(Refresh::Partial { affected, .. }) = &result {
                refresh::reclassify(&rules, &tree, &mut classes, affected);
            }
            (tree, classes, result, refreshed, progress.snapshot().dirs)
        })
        .await;
        let (tree, classes, result, refreshed, dirs_read) = match joined {
            Ok(v) => v,
            Err(e) => {
                // The tree was lost with the panicked task: rebuild it from scratch.
                log::error!("refresh of {} failed: {e}", slot.root.display());
                return Ok((self.full_scan_locked(slot, None).await?.snapshot, None));
            }
        };
        slot.state.write().tree = Some(Arc::new(tree));
        let (affected, subtree, deferred) = match result? {
            Refresh::Full => {
                log::info!("{} changed at its root; rescanning it", slot.root.display());
                return Ok((self.full_scan_locked(slot, None).await?.snapshot, None));
            }
            Refresh::Partial {
                affected,
                subtree,
                deferred,
            } => (affected, subtree.map(|t| Arc::new(*t)), deferred),
        };
        log::debug!(
            "refreshed {} dir(s) in {} in {:.2?} ({} dirs read, {} changed, {} deferred)",
            dirs_count,
            slot.root.display(),
            started.elapsed(),
            dirs_read,
            affected.len(),
            deferred.len()
        );
        let finished = Instant::now();
        let changed = !affected.is_empty() || subtree.is_some();
        {
            let mut state = slot.state.write();
            state
                .summary_refreshed
                .retain(|_, t| finished.duration_since(*t) < interval);
            for path in refreshed {
                state.deferred_summaries.remove(&path);
                state.summary_refreshed.insert(path, finished);
            }
            state.deferred_summaries.extend(deferred);
            state.refreshes += 1;
            if changed {
                state.classes = classes;
                state.generation += 1;
                state.unsaved_since.get_or_insert(finished);
            }
        }
        let snapshot = self.require_ref(&slot.root)?;
        if !changed {
            return Ok((snapshot, None));
        }
        self.emit(Event::TreeUpdated(snapshot.clone()));
        Ok((snapshot, subtree))
    }

    /// Watch configuration for the index roots, resuming from the oldest persisted cursor.
    #[must_use]
    pub fn watch_config(&self, roots: &[PathBuf]) -> WatchConfig {
        let mut config = WatchConfig::new(roots.to_vec());
        let mut oldest: Option<(u64, WatchCursor)> = None;
        for root in roots {
            let Some(slot) = self.inner.roots.lock().get(root).cloned() else {
                continue;
            };
            let Some(bytes) = slot.state.read().cursor.clone() else {
                continue;
            };
            let Ok(cursor) = WatchCursor::from_bytes(&bytes) else {
                continue;
            };
            #[allow(clippy::match_wildcard_for_single_variants)] // the enum is non_exhaustive
            let id = match &cursor {
                WatchCursor::FsEvents { event_id, .. } => *event_id,
                _ => u64::MAX,
            };
            if oldest.as_ref().is_none_or(|(o, _)| id < *o) {
                oldest = Some((id, cursor));
            }
        }
        config.since = oldest.map(|(_, c)| c);
        config
    }

    /// Records the names of active change-watching backends (shown in status).
    pub fn set_watchers(&self, names: Vec<String>) {
        *self.inner.watchers.lock() = names;
    }

    // ----- planning & execution -----

    fn find_class(&self, path: &Path) -> Option<(Classification, Option<u64>)> {
        let root = self.root_for(path)?;
        let slot = self.inner.roots.lock().get(&root).cloned()?;
        let state = slot.state.read();
        let size = state.tree.as_ref().and_then(|t| {
            t.find(path)
                .map(|id| t.size(id, size_mode(&self.settings())))
        });
        state.classes.get(path).cloned().map_or_else(
            || size.map(|s| (explicit_class(s), Some(s))),
            |c| Some((c, size)),
        )
    }

    /// Builds a plan for explicit paths: classified paths use their classification; anything
    /// else is planned as an explicit `TRASH` of its measured size.
    #[must_use]
    pub fn plan_for_paths(&self, paths: &[PathBuf]) -> Plan {
        let candidates: Vec<Candidate> = paths
            .iter()
            .filter_map(|p| {
                let path = std::path::absolute(p).ok()?;
                let class = self
                    .find_class(&path)
                    .map(|(c, _)| c)
                    .or_else(|| path.exists().then(|| explicit_class(0)))?;
                let kind = bclean_actions::EntryKind::of(&path).ok();
                Some(Candidate::new(path, kind, class))
            })
            .collect();
        Plan::from_candidates(&candidates)
    }

    /// Every classified entry of every loaded root as a plan candidate.
    #[must_use]
    pub fn all_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for slot in self.slots() {
            let state = slot.state.read();
            out.extend(state.classes.iter().map(|(p, c)| {
                Candidate::new(p.clone(), Some(bclean_actions::EntryKind::Dir), c.clone())
            }));
        }
        for c in &mut out {
            c.kind = bclean_actions::EntryKind::of(&c.path).ok();
        }
        out
    }

    /// Policy-style plan with default selection (auto-safe ephemeral items) over every root.
    #[must_use]
    pub fn plan_auto(&self) -> Plan {
        let candidates = self.all_candidates();
        let chosen = bclean_policy::select(
            &SelectSpec::default(),
            &BudgetSpec::default(),
            &candidates,
            None,
            None,
        );
        Plan::from_candidates(chosen)
    }

    fn executor(&self, dry_run: bool) -> Result<Executor> {
        executor_for(&self.settings(), &self.inner.data_dir, dry_run)
    }

    /// Why each plan item would be skipped right now (`None` = actionable).
    ///
    /// # Errors
    ///
    /// * [`EngineError::Protect`] if a protected-path pattern is invalid
    pub async fn preflight(&self, plan: &Plan) -> Result<Vec<Option<String>>> {
        let executor = self.executor(true)?;
        let plan = plan.clone();
        Ok(tokio::task::spawn_blocking(move || {
            plan.items
                .iter()
                .map(|item| executor.preflight(item).err().map(|r| r.to_string()))
                .collect()
        })
        .await?)
    }

    /// Executes `plan` (dry run with `dry_run` or for shadow plans), journaling each attempted
    /// item as it finishes, publishing `ACTION_PROGRESS` events (and sending them to
    /// `progress`), then rescans the parents of completed items.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Protect`] if a protected-path pattern is invalid
    /// * [`EngineError::Join`] if the executor panics
    pub async fn execute(
        &self,
        mut plan: Plan,
        dry_run: bool,
        cancel: CancelToken,
        progress: Option<mpsc::UnboundedSender<ActionProgress>>,
    ) -> Result<ExecutionReport> {
        for item in &mut plan.items {
            if item.entries.is_none() {
                item.entries = self.indexed_entries(&item.path);
            }
        }
        let exec = self.inner.exec_lock.lock().await;
        let executor = self.executor(dry_run)?;
        let events = self.inner.events.clone();
        let notify = move |p: ActionProgress| {
            if let Ok(value) = serde_json::to_value(&p) {
                let _ = events.send(Event::ActionProgress { value });
            }
            if let Some(tx) = &progress {
                let _ = tx.send(p);
            }
        };
        notify(ActionProgress::Started {
            total: plan.items.len(),
            dry_run: dry_run || plan.shadow,
        });
        let (jtx, mut jrx) = mpsc::unbounded_channel::<JournalEntry>();
        let inner = Arc::clone(&self.inner);
        let writer = tokio::spawn(async move {
            while let Some(entry) = jrx.recv().await {
                if let Err(e) = inner.journal.append(&entry).await {
                    log::error!("failed to journal {}: {e}", entry.path.display());
                }
            }
        });
        let (report, notify) = tokio::task::spawn_blocking(move || {
            let report = executor.execute(&plan, &cancel, |p| match p {
                Progress::Started { index, total, item } => notify(ActionProgress::ItemStarted {
                    index,
                    total,
                    path: item.path.clone(),
                }),
                Progress::Advanced {
                    index,
                    total,
                    removed,
                    entries,
                } => notify(ActionProgress::ItemAdvanced {
                    index,
                    total,
                    removed,
                    entries,
                }),
                Progress::Finished {
                    index,
                    total,
                    result,
                } => {
                    if let Some(entry) = &result.journal_entry {
                        let _ = jtx.send(entry.clone());
                    }
                    notify(ActionProgress::ItemFinished {
                        index,
                        total,
                        result: Box::new(result.clone()),
                    });
                }
            });
            (report, notify)
        })
        .await?;
        writer.await?;
        notify(ActionProgress::Finished {
            dry_run: report.dry_run,
            estimated_freed: report.estimated_freed,
            measured_freed: report.measured_freed,
            cancelled: report.cancelled,
        });
        if !report.dry_run {
            let dirs: Vec<PathBuf> = report
                .results
                .iter()
                .filter(|r| r.outcome == ItemOutcome::Done)
                .filter_map(|r| r.path.parent().map(Path::to_path_buf))
                .collect();
            drop(exec);
            if let Err(e) = self.apply_dirty(dirs).await {
                log::warn!("failed to refresh the index after cleaning: {e}");
            }
        }
        Ok(report)
    }

    /// Reverses a journaled action and refreshes the index around the restored path.
    /// Returns the undo record.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Undo`] if the entry does not exist, is not reversible, was already
    ///   undone, or the restore fails
    pub async fn undo(&self, id: &str) -> Result<JournalEntry> {
        let entry = bclean_actions::prepare_undo(&self.inner.journal, id).await?;
        let record =
            tokio::task::spawn_blocking(move || bclean_actions::perform_undo(&entry)).await??;
        self.inner.journal.append(&record).await?;
        if let Some(parent) = record.path.parent()
            && let Err(e) = self.apply_dirty(vec![parent.to_path_buf()]).await
        {
            log::warn!("failed to refresh the index after undo: {e}");
        }
        Ok(record)
    }

    /// Writes `subtree` to a new file under `<data>/expanded/` for an IPC client to read (and
    /// delete). Leftovers are removed when the engine opens.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Store`] if the file cannot be written
    /// * [`EngineError::Join`] if the writer panics
    pub async fn write_subtree(&self, subtree: Arc<Tree>) -> Result<PathBuf> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.inner.data_dir.join(EXPANDED_DIR);
        let path = dir.join(format!("{}-{n}.snap", std::process::id()));
        let file = path.clone();
        tokio::task::spawn_blocking(move || -> std::result::Result<(), StoreError> {
            std::fs::create_dir_all(&dir)?;
            let meta = SnapshotMeta {
                scanned_at: unix_now(),
                backend: String::new(),
                cursor: None,
            };
            bclean_index::snapshot::write_tree(&file, &subtree, &meta)?;
            Ok(())
        })
        .await??;
        Ok(path)
    }

    /// Entries the index holds at `path` (itself included), if it is indexed.
    fn indexed_entries(&self, path: &Path) -> Option<u64> {
        let root = self.root_for(path)?;
        let tree = self.slot(&root).state.read().tree.clone()?;
        tree.find(path).map(|id| tree.info(id).items)
    }

    /// Most recent journal entries, newest first.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Journal`] if the journal cannot be read
    pub async fn journal(&self, limit: usize) -> Result<Vec<JournalEntry>> {
        let mut entries = self.inner.journal.entries().await?;
        entries.reverse();
        entries.truncate(limit);
        Ok(entries)
    }

    // ----- monitoring -----

    /// Current usage of the monitored volumes (`monitor.volumes`).
    ///
    /// # Errors
    ///
    /// * [`EngineError::Volume`] if volumes cannot be listed
    pub async fn volumes(&self) -> Result<Vec<VolumeStats>> {
        let settings = self.settings();
        let specs = settings.config().monitor.volumes.clone();
        let expander = settings.expander.clone();
        tokio::task::spawn_blocking(move || monitored_volumes(&specs, &expander)).await?
    }

    /// Samples volumes, records them in history, and publishes a `VOLUMES` event.
    ///
    /// # Errors
    ///
    /// * [`EngineError::Volume`] / [`EngineError::History`] on failures
    pub async fn sample_volumes(&self) -> Result<Vec<VolumeStats>> {
        let volumes = self.volumes().await?;
        self.inner.store.history().record_volumes(&volumes).await?;
        self.emit(Event::Volumes {
            volumes: volumes.clone(),
        });
        Ok(volumes)
    }

    /// Records a history rollup for every loaded root and prunes expired history.
    ///
    /// # Errors
    ///
    /// * [`EngineError::History`] if a write fails
    pub async fn record_rollups(&self) -> Result<()> {
        let settings = self.settings();
        let monitor = &settings.config().monitor;
        let now = unix_now();
        let history = self.inner.store.history();
        for slot in self.slots() {
            let tree = slot.state.read().tree.clone();
            if let Some(tree) = tree {
                history
                    .record_rollup(&tree, now, monitor.history_depth)
                    .await?;
            }
        }
        let pruned = history
            .prune_retention(monitor.history_retention.duration(), now)
            .await?;
        log::debug!("history rollup recorded; pruned {pruned:?}");
        Ok(())
    }

    /// Volume samples of `mount_point` since `since`.
    ///
    /// # Errors
    ///
    /// * [`EngineError::History`] if the query fails
    pub async fn volume_history(&self, mount_point: &str, since: i64) -> Result<Vec<VolumeStats>> {
        Ok(self
            .inner
            .store
            .history()
            .volume_series(mount_point, since)
            .await?)
    }

    /// Directories under `root` that grew the most since `since`.
    ///
    /// # Errors
    ///
    /// * [`EngineError::History`] if the query fails
    pub async fn top_growth(
        &self,
        root: &Path,
        since: i64,
        limit: usize,
    ) -> Result<Vec<GrowthEntry>> {
        let root = absolute(root)?;
        Ok(self
            .inner
            .store
            .history()
            .top_growth(&root, since, limit)
            .await?
            .into_iter()
            .map(|g| GrowthEntry {
                path: g.path,
                from: g.before_allocated,
                to: g.after_allocated,
            })
            .collect())
    }

    async fn prefetch_history(&self, volumes: &[VolumeStats], now: i64) -> Result<MemoryHistory> {
        let history = self.inner.store.history();
        let mut memory = MemoryHistory::default();
        for volume in volumes {
            for sample in history
                .volume_series(&volume.mount_point, now - VOLUME_LOOKBACK_SECS)
                .await?
            {
                memory.record_volume(&volume.mount_point, sample.at, sample.used());
            }
        }
        let settings = self.settings();
        for spec in settings.config().policies.values() {
            if let TriggerSpec::Growth { path, within, .. } = &spec.trigger {
                let key = bclean_actions::protect::expand_tilde(path);
                let since = now.saturating_sub(i64::try_from(within.as_secs()).unwrap_or(0) + 1);
                for point in history.size_series(&key, since).await? {
                    memory.record_dir(key.clone(), point.at, point.allocated);
                }
            }
        }
        Ok(memory)
    }

    /// Evaluates policies against `volumes`, plans and executes (shadow plans as dry runs),
    /// and raises alerts.
    ///
    /// # Errors
    ///
    /// * [`EngineError::History`] if history cannot be read
    /// * Same as [`Engine::execute`]
    pub async fn evaluate_policies(&self, volumes: &[VolumeStats]) -> Result<Vec<PolicyOutcome>> {
        let now = unix_now();
        let memory = self.prefetch_history(volumes, now).await?;
        let firings = self
            .inner
            .policy
            .lock()
            .await
            .evaluate(now, volumes, &memory, &memory);
        if firings.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self.all_candidates();
        let mut out = Vec::new();
        for firing in firings {
            let planned = self
                .inner
                .policy
                .lock()
                .await
                .plan(&firing, &candidates, None);
            log::info!("policy {} fired: {}", firing.policy_id, planned.event.body);
            self.raise(planned.event, &firing.notify).await;
            let report = if let Some(plan) = planned.plan.as_ref().filter(|p| !p.is_empty()) {
                let result = self
                    .execute(plan.clone(), false, CancelToken::new(), None)
                    .await?;
                if !result.dry_run {
                    self.raise(
                        bclean_policy::result_event(&firing, &result),
                        &firing.notify,
                    )
                    .await;
                }
                Some(result)
            } else {
                None
            };
            out.push(PolicyOutcome {
                firing,
                plan: planned.plan,
                report,
            });
        }
        Ok(out)
    }

    /// Publishes an alert, records it in the BMUX buffer, and delivers it to `sinks`.
    pub async fn raise(&self, event: AlertEvent, sinks: &[String]) {
        self.emit(Event::Alert(event.clone()));
        let settings = self.settings();
        let has_bmux_sink = sinks.iter().any(|s| {
            settings
                .config()
                .alerts
                .get(s)
                .is_some_and(|a| a.sink == AlertSink::Bmux)
        });
        if !has_bmux_sink {
            self.inner.bmux.push("engine", event.clone(), unix_now());
        }
        let router = Arc::clone(&self.inner.router.read());
        let sinks = sinks.to_vec();
        let delivered = tokio::task::spawn_blocking(move || router.dispatch(&event, &sinks)).await;
        if let Err(e) = delivered {
            log::warn!("alert delivery failed: {e}");
        }
    }

    /// Active (unacknowledged) alerts.
    #[must_use]
    pub fn alerts(&self) -> Vec<AlertEvent> {
        self.inner
            .bmux
            .active()
            .into_iter()
            .map(|a| a.event)
            .collect()
    }

    /// Acknowledges alerts with `key`; returns how many were acknowledged.
    #[must_use]
    pub fn ack_alert(&self, key: &str) -> usize {
        self.inner.bmux.acknowledge_key(key)
    }

    /// Status of every loaded root.
    #[must_use]
    pub fn root_statuses(&self) -> Vec<RootStatus> {
        self.slots()
            .iter()
            .map(|slot| {
                let state = slot.state.read();
                RootStatus {
                    root: slot.root.clone(),
                    scanning: state.progress.is_some(),
                    progress: state.progress.as_ref().map(|p| progress_info(p.snapshot())),
                    generation: state.generation,
                    scanned_at: state.scanned_at,
                    nodes: state.tree.as_ref().map_or(0, |t| t.len() as u64),
                    total_allocated: state
                        .tree
                        .as_ref()
                        .map_or(0, |t| t.size(NodeId::ROOT, SizeMode::Allocated)),
                    full_scans: state.full_scans,
                    refreshes: state.refreshes,
                }
            })
            .collect()
    }

    /// Daemon status for this engine.
    #[must_use]
    pub fn status(&self, version: &str) -> DaemonStatus {
        let settings = self.settings();
        DaemonStatus {
            pid: std::process::id(),
            version: version.to_owned(),
            started_at: self.inner.started_at,
            roots: self.root_statuses(),
            policies: settings
                .config()
                .policies
                .values()
                .filter(|p| p.enabled)
                .count(),
            active_alerts: self.inner.bmux.active_count(),
            watchers: self.inner.watchers.lock().clone(),
        }
    }
}

const fn size_mode(settings: &Settings) -> SizeMode {
    match settings.config().scan.size_mode {
        bclean_model::SizeModeSpec::Allocated => SizeMode::Allocated,
        bclean_model::SizeModeSpec::Apparent => SizeMode::Apparent,
    }
}

/// Builds the actions executor for `settings`, protecting the data directory (which holds the
/// journal).
///
/// # Errors
///
/// * [`EngineError::Protect`] if a protected-path pattern is invalid
pub fn executor_for(settings: &Settings, data_dir: &Path, dry_run: bool) -> Result<Executor> {
    let protector =
        Protector::from_config(&settings.config().protect)?.with_protected_dir(data_dir);
    Ok(Executor::new(
        protector,
        ExecutorOptions {
            dry_run,
            journal_path: Some(data_dir.join("journal.sqlite3")),
            ..ExecutorOptions::default()
        },
    ))
}

/// Why each item of `plan` would be skipped right now (`None` = actionable). Blocking.
///
/// # Errors
///
/// * [`EngineError::Protect`] if a protected-path pattern is invalid
pub fn preflight_plan(
    settings: &Settings,
    data_dir: &Path,
    plan: &Plan,
) -> Result<Vec<Option<String>>> {
    let executor = executor_for(settings, data_dir, true)?;
    Ok(plan
        .items
        .iter()
        .map(|item| executor.preflight(item).err().map(|r| r.to_string()))
        .collect())
}

/// Classification for an explicitly selected, unclassified path.
fn explicit_class(size: u64) -> Classification {
    Classification {
        rule_id: None,
        category: None,
        ephemeral: None,
        confidence: Confidence::High,
        staleness: Staleness::default(),
        size,
        score: 0.0,
        action: ActionSpec::Trash,
        auto: false,
        reasons: vec![Reason::new("EXPLICIT", "explicitly selected")],
    }
}

/// Converts scanner counters to protocol counters.
#[must_use]
pub const fn progress_info(p: ProgressSnapshot) -> ScanProgressInfo {
    ScanProgressInfo {
        dirs: p.dirs,
        files: p.files,
        bytes: p.bytes,
        errors: p.errors,
    }
}

fn monitored_volumes(specs: &[String], expander: &Expander) -> Result<Vec<VolumeStats>> {
    if specs.is_empty() || specs.iter().any(|s| s == "*") {
        return Ok(bclean_watch::list_volumes()?);
    }
    let mut out: BTreeMap<String, VolumeStats> = BTreeMap::new();
    for spec in specs {
        let path = expander.expand_path(spec)?;
        match bclean_watch::volume_for(&path) {
            Ok(v) => {
                out.insert(v.mount_point.clone(), v);
            }
            Err(e) => log::warn!("cannot stat volume {spec}: {e}"),
        }
    }
    Ok(out.into_values().collect())
}

/// Picks the newest completed, reversible, not-yet-undone entry from `entries` (any order).
#[must_use]
pub fn latest_undoable(entries: &[JournalEntry]) -> Option<&JournalEntry> {
    let undone: Vec<&str> = entries
        .iter()
        .filter(|e| e.outcome == bclean_actions::JournalOutcome::Done)
        .filter_map(|e| e.undoes.as_deref())
        .collect();
    entries
        .iter()
        .filter(|e| {
            e.outcome == bclean_actions::JournalOutcome::Done
                && e.undo.is_some()
                && e.undoes.is_none()
                && !undone.contains(&e.id.as_str())
        })
        .max_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)))
}
