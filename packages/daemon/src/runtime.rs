//! The bclean daemon proper: an [`Engine`]-backed [`Handler`] for every protocol request and
//! the background loops (initial indexing, change watching, volume polling + policies,
//! history rollups).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bclean_actions::{CancelToken, Plan};
use bclean_engine::{Engine, EngineError, Settings};
use bclean_protocol::{ClassifiedPath, Endpoint, RemoteError, Request, Response, SnapshotRef};
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
        Request::Scan { root, force, wait } => {
            if wait {
                Response::Snapshot(engine.ensure_scanned(&root, force).await?)
            } else {
                let current = engine
                    .root_for(&root)
                    .and_then(|r| engine.snapshot_ref(&r))
                    .unwrap_or_else(|| SnapshotRef {
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
        Request::Snapshot { root } => Response::Snapshot(
            engine
                .root_for(&root)
                .and_then(|r| engine.snapshot_ref(&r))
                .ok_or_else(|| {
                    EngineError::NotFound(format!("{} is not indexed", root.display()))
                })?,
        ),
        Request::Rescan { path, expand } => {
            Response::Snapshot(engine.rescan(&path, expand).await?.0)
        }
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
    let server = Arc::new(Server::new(handler, version));
    let sender = server.events();
    let mut events = engine.subscribe();
    let forward = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let _ = sender.send(event);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
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
/// 2. apply change-source batches incrementally
/// 3. sample volumes every `monitor.poll_interval` and evaluate policies
/// 4. record history rollups every `monitor.history_interval` (with retention)
///
/// Returns (after persisting every root) once `shutdown` becomes `true`.
pub async fn run_monitor(engine: Engine, mut shutdown: watch::Receiver<bool>) {
    let (mut batch_rx, initial) = start_indexing(&engine).await;

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
    let mut watching = true;

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            batch = batch_rx.recv(), if watching => match batch {
                Some(batch) => {
                    if let Err(e) = engine.apply_batch(batch).await {
                        log::warn!("failed to apply change batch: {e}");
                    }
                }
                None => watching = false,
            },
            _ = poll.tick() => match engine.sample_volumes().await {
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
            },
            _ = history.tick() => {
                if let Err(e) = engine.record_rollups().await {
                    log::warn!("history rollup failed: {e}");
                }
            }
        }
    }
    initial.abort();
    if let Err(e) = engine.persist_all().await {
        log::warn!("failed to persist snapshots at shutdown: {e}");
    }
}

/// Loads snapshots, starts the change source, and spawns the initial reconciliation scans.
async fn start_indexing(
    engine: &Engine,
) -> (
    mpsc::UnboundedReceiver<ChangeBatch>,
    tokio::task::JoinHandle<()>,
) {
    let roots = engine.index_roots();
    let mut needs_scan: Vec<PathBuf> = Vec::new();
    for root in &roots {
        match engine.load_snapshot(root).await {
            Ok(Some(meta)) if meta.cursor.is_some() => {}
            Ok(_) => needs_scan.push(root.clone()),
            Err(e) => {
                log::warn!("cannot load snapshot for {}: {e}", root.display());
                needs_scan.push(root.clone());
            }
        }
    }

    let (batch_tx, batch_rx) = mpsc::unbounded_channel::<ChangeBatch>();
    let mut scan_cursor = None;
    if !roots.is_empty() {
        let config = engine.watch_config(&roots);
        match bclean_watch::open_change_source(&config) {
            Ok(source) => {
                let name = format!("{:?}", source.backend()).to_uppercase();
                log::info!("watching {} root(s) with {name}", roots.len());
                engine.set_watchers(vec![name]);
                if !source.supports_replay() || config.since.is_none() {
                    needs_scan.clone_from(&roots);
                }
                scan_cursor = source.current_cursor().map(|c| c.to_bytes());
                let tx = batch_tx.clone();
                std::thread::Builder::new()
                    .name("bclean-watch".to_owned())
                    .spawn(move || forward_batches(&*source, &tx))
                    .map_or_else(|e| log::warn!("cannot start watch thread: {e}"), drop);
            }
            Err(e) => {
                log::warn!("filesystem watching unavailable: {e}");
                needs_scan.clone_from(&roots);
            }
        }
    }
    drop(batch_tx);

    let scanner = engine.clone();
    let initial = tokio::spawn(async move {
        for root in needs_scan {
            if let Err(e) = scanner.scan_with_cursor(&root, scan_cursor.clone()).await {
                log::warn!("initial scan of {} failed: {e}", root.display());
            }
        }
    });

    (batch_rx, initial)
}

fn forward_batches(
    source: &dyn bclean_watch::ChangeSource,
    tx: &mpsc::UnboundedSender<ChangeBatch>,
) {
    loop {
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
