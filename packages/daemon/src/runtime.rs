//! The bclean daemon proper: an [`Engine`]-backed [`Handler`] for every protocol request and
//! the background loops (initial indexing, change watching, volume polling + policies,
//! history rollups).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bclean_actions::{CancelToken, Plan};
use bclean_engine::{Engine, EngineError, Settings};
use bclean_protocol::{
    ClassifiedPath, Endpoint, Event, RemoteError, Request, Response, SnapshotRef,
};
use bclean_watch::ChangeBatch;
use tokio::sync::{mpsc, watch};

use crate::server::{Handler, Server};

/// The daemon IPC endpoint for `settings`: `daemon.socket` if configured (with `~`
/// expansion), else the per-user default under the runtime directory.
#[must_use]
pub fn endpoint_for(settings: &Settings, runtime_dir: &std::path::Path) -> Endpoint {
    settings.config().daemon.socket.as_ref().map_or_else(
        || Endpoint::default_for(runtime_dir),
        |socket| {
            let expanded = settings
                .expander
                .expand(socket)
                .unwrap_or_else(|_| socket.clone());
            Endpoint::from_config(&expanded)
        },
    )
}

/// Request handler backed by an [`Engine`].
#[derive(Debug, Clone)]
pub struct DaemonHandler {
    engine: Engine,
    version: String,
}

impl DaemonHandler {
    /// Creates a handler.
    #[must_use]
    pub fn new(engine: Engine, version: impl Into<String>) -> Self {
        Self {
            engine,
            version: version.into(),
        }
    }
}

impl Handler for DaemonHandler {
    fn handle(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, RemoteError>> + Send {
        let engine = self.engine.clone();
        let version = self.version.clone();
        async move {
            handle(&engine, &version, request)
                .await
                .map_err(|e| RemoteError::new(e.code(), e.to_string()))
        }
    }
}

fn json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, EngineError> {
    serde_json::to_value(value).map_err(|e| EngineError::Invalid(e.to_string()))
}

/// Rescans `path`. With `expand`, replies with the expanded directory in its own file as soon
/// as it is grafted, without waiting for the root's snapshot to be saved.
async fn rescan_reply(
    engine: &Engine,
    path: PathBuf,
    expand: bool,
) -> Result<Response, EngineError> {
    let (rescanned, subtree) = engine.rescan(&path, expand).await?;
    Ok(match subtree {
        Some(subtree) if expand => {
            let root = std::path::absolute(&path).unwrap_or(path);
            let file = engine.write_subtree(subtree).await?;
            Response::Expanded {
                subtree: SnapshotRef {
                    root,
                    path: file,
                    generation: rescanned.generation,
                },
            }
        }
        _ => Response::Snapshot(engine.saved_snapshot_ref(&rescanned.root).await?),
    })
}

/// Maps one request onto the engine.
///
/// # Errors
///
/// * Any [`EngineError`] from the engine call
pub async fn handle(
    engine: &Engine,
    version: &str,
    request: Request,
) -> Result<Response, EngineError> {
    Ok(match request {
        Request::Status => Response::Status(engine.status(version)),
        Request::Dashboard {
            roots,
            one_filesystem,
        } => Response::Dashboard(Box::new(engine.dashboard(&roots, one_filesystem).await?)),
        Request::Volumes => Response::Volumes {
            volumes: engine.volumes().await?,
        },
        Request::Alerts => Response::Alerts {
            alerts: engine.alerts(),
        },
        Request::AckAlert { key } => {
            if engine.ack_alert(&key) == 0 {
                return Err(EngineError::NotFound(format!("no active alert {key}")));
            }
            Response::Ok
        }
        // Clients read the snapshot file a reference points to, so every reference handed out
        // here is saved first.
        Request::Scan { root, force, wait } => {
            if wait {
                let scanned = engine.ensure_scanned(&root, force).await?;
                Response::Snapshot(engine.saved_snapshot_ref(&scanned.root).await?)
            } else {
                let current = match engine.root_for(&root) {
                    Some(r) => engine.saved_snapshot_ref(&r).await.ok(),
                    None => None,
                };
                let current = current.unwrap_or_else(|| SnapshotRef {
                    path: engine.snapshot_path(&root),
                    root: root.clone(),
                    generation: 0,
                });
                let background = engine.clone();
                tokio::spawn(async move {
                    if let Err(e) = background.ensure_scanned(&root, force).await {
                        log::warn!("background scan of {} failed: {e}", root.display());
                    }
                });
                Response::Snapshot(current)
            }
        }
        Request::Snapshot { root } => {
            let root = engine.root_for(&root).ok_or_else(|| {
                EngineError::NotFound(format!("{} is not indexed", root.display()))
            })?;
            Response::Snapshot(engine.saved_snapshot_ref(&root).await?)
        }
        Request::Rescan { path, expand } => rescan_reply(engine, path, expand).await?,
        Request::Classifications { root } => Response::Classifications {
            items: engine.classifications(&root, false).await?,
        },
        Request::Explain { path } => {
            let explanation = engine.explain(&path).await?;
            Response::Explain(ClassifiedPath {
                path: explanation.path,
                classification: explanation.classification,
            })
        }
        Request::Plan { paths } => {
            let plan = if paths.is_empty() {
                engine.plan_auto()
            } else {
                engine.plan_for_paths(&paths)
            };
            Response::Plan { plan: json(&plan)? }
        }
        Request::Execute { plan, dry_run } => {
            let plan: Plan =
                serde_json::from_value(plan).map_err(|e| EngineError::Invalid(e.to_string()))?;
            let report = engine
                .execute(plan, dry_run, CancelToken::new(), None)
                .await?;
            Response::Actions {
                value: json(&report)?,
            }
        }
        Request::Undo { id } => Response::Actions {
            value: json(&engine.undo(&id).await?)?,
        },
        Request::Journal { limit } => Response::Actions {
            value: json(&engine.journal(limit).await?)?,
        },
        Request::VolumeHistory { mount_point, since } => Response::Volumes {
            volumes: engine.volume_history(&mount_point, since).await?,
        },
        Request::TopGrowth { root, since, limit } => Response::Growth {
            entries: engine.top_growth(&root, since, limit).await?,
        },
        Request::ReloadConfig => {
            engine.reload().await?;
            Response::Ok
        }
        // Handled by the server itself.
        Request::Subscribe { .. } | Request::Shutdown => Response::Ok,
    })
}

/// Runs the daemon until it is asked to shut down (IPC `SHUTDOWN` or a termination signal).
///
/// # Errors
///
/// * If the endpoint cannot be bound
pub async fn run_daemon(engine: Engine, endpoint: Endpoint, version: &str) -> std::io::Result<()> {
    let handler = Arc::new(DaemonHandler::new(engine.clone(), version));
    let server =
        Arc::new(Server::new(handler, version).with_build(bclean_protocol::executable_build_id()));
    let forward = tokio::spawn(forward_events(engine.clone(), server.events()));
    let shutdown = server.shutdown_handle();
    let monitor = tokio::spawn(run_monitor(engine.clone(), shutdown.subscribe()));
    let result = Arc::clone(&server).serve(&endpoint).await;
    let _ = shutdown.send(true);
    if let Err(e) = monitor.await {
        log::warn!("monitor task failed: {e}");
    }
    forward.abort();
    result
}

/// Background loops shared by the daemon and `bclean watch`:
///
/// 1. load each index root's snapshot (instant), then reconcile it: replay changes since the
///    persisted watch cursor when the backend supports it, else rescan
/// 2. apply change-source batches incrementally (batches queued while one is applied are
///    merged into a single update)
/// 3. sample volumes every `monitor.poll_interval` and evaluate policies; index drives as
///    they are mounted and unload them when they go away (`monitor.index_volumes`)
/// 4. record history rollups every `monitor.history_interval` (with retention)
/// 5. every [`MAINTENANCE_INTERVAL`], re-measure postponed summarized directories and save
///    snapshots whose changes are older than `monitor.persist_interval`
///
/// Returns (after persisting every root) once `shutdown` becomes `true`.
pub async fn run_monitor(engine: Engine, mut shutdown: watch::Receiver<bool>) {
    if let Err(e) = engine.refresh_volume_roots().await {
        log::warn!("cannot detect drives to index: {e}");
    }
    let mut indexing = Indexing::start(&engine).await;

    let monitor = engine.settings().config().monitor.clone();
    let poll_every = monitor.poll_interval.duration().max(Duration::from_secs(1));
    let history_every = monitor
        .history_interval
        .duration()
        .max(Duration::from_secs(1));
    let mut poll = tokio::time::interval(poll_every);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut history =
        tokio::time::interval_at(tokio::time::Instant::now() + history_every, history_every);
    history.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let persist_after = monitor.persist_interval.duration();
    let mut maintenance = tokio::time::interval_at(
        tokio::time::Instant::now() + MAINTENANCE_INTERVAL,
        MAINTENANCE_INTERVAL,
    );
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            Some(batch) = indexing.batches.recv() => {
                for batch in merge_queued(batch, &mut indexing.batches) {
                    if let Err(e) = engine.apply_batch(batch).await {
                        log::warn!("failed to apply change batch: {e}");
                    }
                }
            },
            _ = maintenance.tick() => {
                if let Err(e) = engine.refresh_deferred().await {
                    log::warn!("failed to refresh postponed summaries: {e}");
                }
                if let Err(e) = engine.persist_unsaved(persist_after).await {
                    log::warn!("failed to persist snapshots: {e}");
                }
            }
            _ = poll.tick() => {
                indexing.refresh_drives(&engine).await;
                match engine.sample_volumes().await {
                    Ok(volumes) => match engine.evaluate_policies(&volumes).await {
                        Ok(outcomes) => {
                            for outcome in outcomes {
                                log::info!(
                                    "policy {}: {} item(s) planned",
                                    outcome.firing.policy_id,
                                    outcome.plan.map_or(0, |p| p.len())
                                );
                            }
                        }
                        Err(e) => log::warn!("policy evaluation failed: {e}"),
                    },
                    Err(e) => log::warn!("volume sampling failed: {e}"),
                }
            },
            _ = history.tick() => {
                if let Err(e) = engine.record_rollups().await {
                    log::warn!("history rollup failed: {e}");
                }
            }
        }
    }
    indexing.stop();
    if let Err(e) = engine.persist_all().await {
        log::warn!("failed to persist snapshots at shutdown: {e}");
    }
}

/// Index roots, the change source watching them, and the queue of full scans.
struct Indexing {
    /// Change batches from the current source (the channel outlives source restarts).
    batches: mpsc::UnboundedReceiver<ChangeBatch>,
    batch_tx: mpsc::UnboundedSender<ChangeBatch>,
    /// Stops the current source's forwarding thread (which owns and drops the source).
    source_stop: Option<Arc<AtomicBool>>,
    /// Cursor of the current source, recorded with full scans.
    cursor: Option<Vec<u8>>,
    /// Roots to scan from scratch, one after another, with the cursor to record.
    scans: mpsc::UnboundedSender<(PathBuf, Option<Vec<u8>>)>,
    scanner: tokio::task::JoinHandle<()>,
}

impl Indexing {
    /// Loads snapshots, starts the change source, and queues the initial reconciliation scans.
    async fn start(engine: &Engine) -> Self {
        let roots = engine.index_roots();
        let mut needs_scan: Vec<PathBuf> = Vec::new();
        for root in &roots {
            if !load_snapshot(engine, root).await {
                needs_scan.push(root.clone());
            }
        }
        let (batch_tx, batches) = mpsc::unbounded_channel::<ChangeBatch>();
        let (scans, mut queue) = mpsc::unbounded_channel::<(PathBuf, Option<Vec<u8>>)>();
        let scanner_engine = engine.clone();
        let scanner = tokio::spawn(async move {
            while let Some((root, cursor)) = queue.recv().await {
                // A drive unmounted while its scan was queued would scan as an empty
                // directory and overwrite its snapshot.
                if !scanner_engine.index_roots().contains(&root) {
                    continue;
                }
                if let Err(e) = scanner_engine.scan_with_cursor(&root, cursor).await {
                    log::warn!("initial scan of {} failed: {e}", root.display());
                }
            }
        });
        let mut indexing = Self {
            batches,
            batch_tx,
            source_stop: None,
            cursor: None,
            scans,
            scanner,
        };
        if indexing.watch(engine, &roots) != Replay::Replays {
            needs_scan.clone_from(&roots);
        }
        // Configured roots (usually the home directory) first; whole drives can take minutes.
        let configured = engine.settings().index_roots();
        needs_scan.sort_by_key(|root| !configured.contains(root));
        for root in needs_scan {
            indexing.queue_scan(root);
        }
        indexing
    }

    fn queue_scan(&self, root: PathBuf) {
        let _ = self.scans.send((root, self.cursor.clone()));
    }

    /// (Re)starts the change source on `roots`, replacing the current one.
    fn watch(&mut self, engine: &Engine, roots: &[PathBuf]) -> Replay {
        if let Some(stop) = self.source_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.cursor = None;
        engine.set_watchers(Vec::new());
        if roots.is_empty() {
            return Replay::Unavailable;
        }
        let config = engine.watch_config(roots);
        match bclean_watch::open_change_source(&config) {
            Ok(source) => {
                let name = format!("{:?}", source.backend()).to_uppercase();
                log::info!("watching {} root(s) with {name}", roots.len());
                engine.set_watchers(vec![name]);
                let replay = if source.supports_replay() && config.since.is_some() {
                    Replay::Replays
                } else {
                    Replay::Unavailable
                };
                self.cursor = source.current_cursor().map(|c| c.to_bytes());
                let stop = Arc::new(AtomicBool::new(false));
                let flag = Arc::clone(&stop);
                let tx = self.batch_tx.clone();
                match std::thread::Builder::new()
                    .name("bclean-watch".to_owned())
                    .spawn(move || forward_batches(&*source, &tx, &flag))
                {
                    Ok(_) => self.source_stop = Some(stop),
                    Err(e) => log::warn!("cannot start watch thread: {e}"),
                }
                replay
            }
            Err(e) => {
                log::warn!("filesystem watching unavailable: {e}");
                Replay::Unavailable
            }
        }
    }

    /// Picks up mounted and unmounted drives: unloads drives that went away, indexes new ones,
    /// and restarts the change source on the new set of roots.
    async fn refresh_drives(&mut self, engine: &Engine) {
        let changes = match engine.refresh_volume_roots().await {
            Ok(changes) if !changes.is_empty() => changes,
            Ok(_) => return,
            Err(e) => {
                log::warn!("cannot detect drives to index: {e}");
                return;
            }
        };
        for root in &changes.removed {
            log::info!("drive {} went away; unloading its index", root.display());
            if let Err(e) = engine.unload_root(root).await {
                log::warn!("cannot save the index of {}: {e}", root.display());
            }
        }
        for root in &changes.added {
            log::info!("drive {} appeared; indexing it", root.display());
            load_snapshot(engine, root).await;
        }
        self.watch(engine, &engine.index_roots());
        // Without replay the snapshot of a returning drive may be out of date.
        for root in changes.added {
            self.queue_scan(root);
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = self.source_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.scanner.abort();
    }
}

/// Whether a change source replays what happened while the daemon was down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replay {
    Replays,
    Unavailable,
}

/// Loads `root`'s snapshot; `true` if one with a replayable watch cursor was found.
async fn load_snapshot(engine: &Engine, root: &std::path::Path) -> bool {
    match engine.load_snapshot(root).await {
        Ok(Some(meta)) => meta.cursor.is_some(),
        Ok(None) => false,
        Err(e) => {
            log::warn!("cannot load snapshot for {}: {e}", root.display());
            false
        }
    }
}

/// Minimum time between two `TREE_UPDATED` events for one root sent to clients.
pub const TREE_EVENT_INTERVAL: Duration = Duration::from_secs(1);

/// Forwards engine events to connected clients. A `TREE_UPDATED` reference is only useful
/// once its snapshot file is current, so tree updates are dropped while nobody is subscribed
/// and otherwise saved and sent at most once per [`TREE_EVENT_INTERVAL`] per root.
async fn forward_events(engine: Engine, sender: tokio::sync::broadcast::Sender<Event>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut events = engine.subscribe();
    let mut pending: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut flush = tokio::time::interval(TREE_EVENT_INTERVAL);
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(Event::TreeUpdated(snapshot)) => {
                    if sender.receiver_count() > 0 {
                        pending.insert(snapshot.root);
                    }
                }
                Ok(event) => {
                    let _ = sender.send(event);
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            _ = flush.tick(), if !pending.is_empty() => {
                for root in std::mem::take(&mut pending) {
                    match engine.saved_snapshot_ref(&root).await {
                        Ok(snapshot) => {
                            let _ = sender.send(Event::TreeUpdated(snapshot));
                        }
                        Err(e) => log::warn!("cannot save snapshot of {}: {e}", root.display()),
                    }
                }
            }
        }
    }
}

/// How often postponed work (summary re-measurements, snapshot saves) is checked.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(15);

/// Drains batches queued behind `first` and merges them: at most one full-rescan batch, then
/// one shallow batch carrying the newest cursor. Shallow paths inside a full-rescan subtree
/// are dropped.
fn merge_queued(
    first: ChangeBatch,
    rx: &mut mpsc::UnboundedReceiver<ChangeBatch>,
) -> Vec<ChangeBatch> {
    let mut deep: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut shallow: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut cursor = None;
    let mut next = Some(first);
    while let Some(batch) = next.take().or_else(|| rx.try_recv().ok()) {
        if batch.must_full_rescan {
            deep.extend(batch.dirty);
        } else {
            shallow.extend(batch.dirty);
        }
        if batch.cursor.is_some() {
            cursor = batch.cursor;
        }
    }
    let shallow: Vec<PathBuf> = shallow
        .into_iter()
        .filter(|p| !deep.iter().any(|d| p.starts_with(d)))
        .collect();
    let mut out = Vec::with_capacity(2);
    if !deep.is_empty() {
        out.push(ChangeBatch {
            dirty: deep.into_iter().collect(),
            must_full_rescan: true,
            cursor: None,
        });
    }
    if !shallow.is_empty() || cursor.is_some() {
        out.push(ChangeBatch {
            dirty: shallow,
            must_full_rescan: false,
            cursor,
        });
    }
    out
}

fn forward_batches(
    source: &dyn bclean_watch::ChangeSource,
    tx: &mpsc::UnboundedSender<ChangeBatch>,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::Relaxed) {
        match source.batches().recv_timeout(Duration::from_secs(1)) {
            Ok(batch) => {
                if tx.send(batch).is_err() {
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}
