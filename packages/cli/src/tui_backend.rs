//! [`bclean_tui::Backend`] implementations: [`LocalBackend`] (in-process engine) and
//! [`DaemonBackend`] (IPC client; trees are loaded from snapshot files, never sent over the
//! socket).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bclean_actions::{CancelToken, ExecutionReport, ItemResult, Plan};
use bclean_client::{Client, ClientOptions};
use bclean_engine::{ActionProgress, Engine, Settings};
use bclean_protocol::{Event, Request, Response, SnapshotRef, Topic};
use bclean_tree::{EntryMeta, NodeId, Tree};
use bclean_tui::{
    ActionOverride, Backend, BackendEvent, BackendMode, BackendStatus, CleanPlan, CleanReport,
    EventSink, ExecutionEvent, PlanRequest, ScanProgress,
};
use parking_lot::Mutex;
use tokio::runtime::Handle;

use crate::conn::VERSION;

/// Shared plan bookkeeping.
#[derive(Debug, Default)]
struct Plans {
    next_id: u64,
    by_id: BTreeMap<u64, Plan>,
    cancel: Option<CancelToken>,
}

fn apply_override(plan: &mut Plan, action: &ActionOverride) {
    if let ActionOverride::Force(spec) = action {
        for item in &mut plan.items {
            item.action = spec.clone();
        }
    }
}

fn to_clean_plan(id: u64, request_id: u64, plan: &Plan, blocked: &[Option<String>]) -> CleanPlan {
    CleanPlan {
        id,
        request_id,
        items: plan
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| bclean_tui::PlanItem {
                path: item.path.clone(),
                action: item.action.clone(),
                estimated_bytes: item.size,
                blocked: blocked.get(i).cloned().flatten(),
                note: item.rule_id.clone(),
            })
            .collect(),
    }
}

fn item_outcome(result: &ItemResult) -> bclean_tui::ItemOutcome {
    use bclean_actions::ItemOutcome as A;
    match &result.outcome {
        A::Done => bclean_tui::ItemOutcome::Done {
            freed_bytes: Some(
                result
                    .measured_freed
                    .map_or(result.estimated_freed, |m| u64::try_from(m).unwrap_or(0)),
            ),
        },
        A::DryRun => bclean_tui::ItemOutcome::Done { freed_bytes: None },
        A::Skipped(reason) => bclean_tui::ItemOutcome::Skipped {
            reason: reason.to_string(),
        },
        A::Failed(error) => bclean_tui::ItemOutcome::Failed {
            error: error.clone(),
        },
        A::Cancelled => bclean_tui::ItemOutcome::Skipped {
            reason: "cancelled".to_owned(),
        },
    }
}

fn clean_report(
    plan_id: u64,
    report: &ExecutionReport,
    elapsed: Duration,
    journal: Option<String>,
) -> CleanReport {
    use bclean_actions::ItemOutcome as A;
    let count = |f: fn(&A) -> bool| report.results.iter().filter(|r| f(&r.outcome)).count();
    CleanReport {
        plan_id,
        dry_run: report.dry_run,
        estimated_bytes: report.estimated_freed,
        measured_freed_bytes: report.measured_freed.map(|m| u64::try_from(m).unwrap_or(0)),
        done: count(|o| matches!(o, A::Done | A::DryRun)),
        skipped: count(|o| matches!(o, A::Skipped(_) | A::Cancelled)),
        failed: count(|o| matches!(o, A::Failed(_))),
        cancelled: report.cancelled,
        elapsed,
        journal,
    }
}

/// Maps an execution progress message to a TUI event (`Finished` is reported separately).
fn execution_event(progress: &ActionProgress) -> Option<ExecutionEvent> {
    match progress {
        ActionProgress::ItemStarted { index, .. } => {
            Some(ExecutionEvent::ItemStarted { index: *index })
        }
        ActionProgress::ItemAdvanced {
            index,
            removed,
            entries,
            ..
        } => Some(ExecutionEvent::ItemProgress {
            index: *index,
            removed: *removed,
            entries: *entries,
        }),
        ActionProgress::ItemFinished { index, result, .. } => Some(ExecutionEvent::ItemFinished {
            index: *index,
            outcome: item_outcome(result),
        }),
        ActionProgress::Started { .. } | ActionProgress::Finished { .. } => None,
    }
}

fn scan_progress(
    root: &Path,
    p: bclean_protocol::ScanProgressInfo,
    started: Instant,
) -> ScanProgress {
    ScanProgress {
        root: root.to_path_buf(),
        running: true,
        files: p.files,
        dirs: p.dirs,
        apparent_bytes: p.bytes,
        allocated_bytes: p.bytes,
        errors: p.errors,
        current: None,
        elapsed: started.elapsed(),
        expected_entries: None,
    }
}

/// Copies the subtree at `node` into a standalone tree rooted at its path.
#[must_use]
pub fn extract_subtree(tree: &Tree, node: NodeId) -> Tree {
    if node == NodeId::ROOT {
        return tree.clone();
    }
    let meta = |id: NodeId| {
        let info = tree.info(id);
        let child_items: u64 = tree.children(id).map(|c| tree.info(c).items).sum();
        let mut meta = EntryMeta::new(info.kind, info.own_apparent, info.own_allocated, info.mtime)
            .with_flags(info.flags);
        meta.items = info.items.saturating_sub(child_items);
        meta
    };
    let mut out = Tree::new(tree.path(node), meta(node));
    let mut stack = vec![(node, NodeId::ROOT)];
    while let Some((src, dst)) = stack.pop() {
        for child in tree.children(src) {
            let id = out.add_child_raw(dst, tree.name(child), meta(child));
            stack.push((child, id));
        }
    }
    out.recompute_aggregates();
    out
}

// ----- local -----

/// Backend running the engine in-process (`--no-daemon`).
pub struct LocalBackend {
    engine: Engine,
    handle: Handle,
    root: PathBuf,
    sink: Option<EventSink>,
    plans: Arc<Mutex<Plans>>,
}

impl LocalBackend {
    /// Creates a backend browsing `root`; work runs on `handle`.
    #[must_use]
    pub fn new(engine: Engine, handle: Handle, root: PathBuf) -> Self {
        Self {
            engine,
            handle,
            root,
            sink: None,
            plans: Arc::new(Mutex::new(Plans::default())),
        }
    }

    fn sink(&self) -> EventSink {
        self.sink.clone().expect("Backend::start is called first")
    }
}

fn send_tree(engine: &Engine, sink: &EventSink, root: &Path) {
    if let Some(tree) = engine.tree(root) {
        sink.send(BackendEvent::Tree(tree));
        let classes = engine
            .classifications_under(root)
            .into_iter()
            .map(|c| (c.path, c.classification))
            .collect();
        sink.send(BackendEvent::ClearClassifications);
        sink.send(BackendEvent::Classifications(classes));
    }
}

impl Backend for LocalBackend {
    fn start(&mut self, sink: EventSink) {
        self.sink = Some(sink.clone());
        let engine = self.engine.clone();
        let root = self.root.clone();
        let mut events = engine.subscribe();
        let started = Instant::now();
        let listener_sink = sink.clone();
        let listener_engine = engine.clone();
        let listener_root = root.clone();
        self.handle.spawn(async move {
            while let Ok(event) = events.recv().await {
                if listener_sink.is_closed() {
                    break;
                }
                match event {
                    Event::ScanProgress { root, progress } if root == listener_root => {
                        listener_sink.send(BackendEvent::Progress(scan_progress(
                            &root, progress, started,
                        )));
                    }
                    Event::TreeUpdated(s) if s.root == listener_root => {
                        send_tree(&listener_engine, &listener_sink, &listener_root);
                    }
                    _ => {}
                }
            }
        });
        self.handle.spawn(async move {
            match engine.scan(&root).await {
                Ok(report) => {
                    let mut done =
                        scan_progress(&root, bclean_engine::progress_info(report.stats), started);
                    done.running = false;
                    done.elapsed = report.elapsed;
                    sink.send(BackendEvent::Progress(done));
                }
                Err(e) => {
                    sink.send(BackendEvent::Error(format!("scan failed: {e}")));
                }
            }
        });
    }

    fn expand(&mut self, path: PathBuf) {
        let engine = self.engine.clone();
        let sink = self.sink();
        self.handle.spawn(async move {
            if let Err(e) = engine.rescan(&path, true).await {
                sink.send(BackendEvent::Error(format!(
                    "cannot expand {}: {e}",
                    path.display()
                )));
            }
        });
    }

    fn rescan(&mut self, path: PathBuf) {
        let engine = self.engine.clone();
        let sink = self.sink();
        let root = self.root.clone();
        self.handle.spawn(async move {
            let result = if path == root {
                engine.scan(&root).await.map(drop)
            } else {
                engine.rescan(&path, false).await.map(drop)
            };
            if let Err(e) = result {
                sink.send(BackendEvent::Error(format!(
                    "rescan of {} failed: {e}",
                    path.display()
                )));
            }
        });
    }

    fn build_plan(&mut self, request: PlanRequest) {
        let engine = self.engine.clone();
        let sink = self.sink();
        let plans = Arc::clone(&self.plans);
        self.handle.spawn(async move {
            let mut plan = engine.plan_for_paths(&request.paths);
            apply_override(&mut plan, &request.action);
            let blocked = match engine.preflight(&plan).await {
                Ok(b) => b,
                Err(e) => {
                    sink.send(BackendEvent::Error(format!("cannot plan: {e}")));
                    return;
                }
            };
            let id = {
                let mut p = plans.lock();
                p.next_id += 1;
                let id = p.next_id;
                p.by_id.insert(id, plan.clone());
                id
            };
            sink.send(BackendEvent::Plan(to_clean_plan(
                id,
                request.request_id,
                &plan,
                &blocked,
            )));
        });
    }

    fn execute_plan(&mut self, plan: CleanPlan, dry_run: bool) {
        let engine = self.engine.clone();
        let sink = self.sink();
        let cancel = CancelToken::new();
        let Some(mut actions_plan) = ({
            let mut p = self.plans.lock();
            p.cancel = Some(cancel.clone());
            p.by_id.get(&plan.id).cloned()
        }) else {
            sink.send(BackendEvent::Error(format!("unknown plan {}", plan.id)));
            return;
        };
        // Drop items the user saw as blocked.
        let blocked: Vec<PathBuf> = plan
            .items
            .iter()
            .filter(|i| i.blocked.is_some())
            .map(|i| i.path.clone())
            .collect();
        actions_plan.items.retain(|i| !blocked.contains(&i.path));
        let index_of: Vec<usize> = actions_plan
            .items
            .iter()
            .filter_map(|i| plan.items.iter().position(|p| p.path == i.path))
            .collect();
        let journal = engine.journal_path().display().to_string();
        self.handle.spawn(async move {
            let started = Instant::now();
            sink.send(BackendEvent::Execution(ExecutionEvent::Started {
                plan_id: plan.id,
            }));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let forward_sink = sink.clone();
            let forward = tokio::spawn(async move {
                while let Some(p) = rx.recv().await {
                    if let Some(mut ev) = execution_event(&p) {
                        remap_index(&mut ev, &index_of);
                        forward_sink.send(BackendEvent::Execution(ev));
                    }
                }
            });
            let result = engine
                .execute(actions_plan, dry_run, cancel, Some(tx))
                .await;
            let _ = forward.await;
            match result {
                Ok(report) => sink.send(BackendEvent::Execution(ExecutionEvent::Finished(
                    clean_report(plan.id, &report, started.elapsed(), Some(journal)),
                ))),
                Err(e) => sink.send(BackendEvent::Error(format!("clean failed: {e}"))),
            };
        });
    }

    fn cancel_execution(&mut self) {
        if let Some(cancel) = &self.plans.lock().cancel {
            cancel.cancel();
        }
    }

    fn refresh_dashboard(&mut self) {
        let engine = self.engine.clone();
        let Some(sink) = self.sink.clone() else {
            return;
        };
        self.handle.spawn(async move {
            match engine.volumes().await {
                Ok(v) => {
                    sink.send(BackendEvent::Volumes(v));
                }
                Err(e) => {
                    sink.send(BackendEvent::Error(format!("volumes: {e}")));
                }
            }
            sink.send(BackendEvent::Alerts(engine.alerts()));
            sink.send(BackendEvent::Status(BackendStatus {
                mode: BackendMode::Local,
                connected: true,
                version: Some(VERSION.to_owned()),
                detail: Some(format!(
                    "in-process engine (data {})",
                    engine.data_dir().display()
                )),
            }));
        });
    }
}

fn remap_index(event: &mut ExecutionEvent, index_of: &[usize]) {
    match event {
        ExecutionEvent::ItemStarted { index }
        | ExecutionEvent::ItemProgress { index, .. }
        | ExecutionEvent::ItemFinished { index, .. } => {
            if let Some(&i) = index_of.get(*index) {
                *index = i;
            }
        }
        ExecutionEvent::Started { .. } | ExecutionEvent::Finished(_) => {}
    }
}

// ----- daemon -----

#[derive(Debug, Default)]
struct DaemonShared {
    /// Root of the daemon tree containing the browse path.
    tree_root: Option<PathBuf>,
    /// Index remapping of the running execution (plan index -> UI index), if any.
    executing: Option<Vec<usize>>,
}

/// Backend talking to the daemon.
pub struct DaemonBackend {
    options: ClientOptions,
    settings: Arc<Settings>,
    data_dir: PathBuf,
    handle: Handle,
    path: PathBuf,
    sink: Option<EventSink>,
    plans: Arc<Mutex<Plans>>,
    shared: Arc<Mutex<DaemonShared>>,
}

impl DaemonBackend {
    /// Creates a backend browsing `path` through the daemon reachable with `options`.
    #[must_use]
    pub fn new(
        options: ClientOptions,
        settings: Arc<Settings>,
        data_dir: PathBuf,
        handle: Handle,
        path: PathBuf,
    ) -> Self {
        Self {
            options,
            settings,
            data_dir,
            handle,
            path,
            sink: None,
            plans: Arc::new(Mutex::new(Plans::default())),
            shared: Arc::new(Mutex::new(DaemonShared::default())),
        }
    }

    fn sink(&self) -> EventSink {
        self.sink.clone().expect("Backend::start is called first")
    }

    /// Runs `f` with a fresh client connection on the runtime, reporting errors to the UI.
    fn with_client<F, Fut>(&self, what: &'static str, f: F)
    where
        F: FnOnce(Client, EventSink) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send,
    {
        let options = self.options.clone();
        let sink = self.sink();
        self.handle.spawn(async move {
            let result = match Client::connect(&options).await {
                Ok(client) => f(client, sink.clone()).await,
                Err(e) => Err(e.into()),
            };
            if let Err(e) = result {
                sink.send(BackendEvent::Error(format!("{what}: {e:#}")));
            }
        });
    }
}

async fn load_snapshot(snapshot: SnapshotRef, path: PathBuf) -> anyhow::Result<Arc<Tree>> {
    tokio::task::spawn_blocking(move || {
        let tree = bclean_index::snapshot::read(&snapshot.path, Some(&snapshot.root))?.tree;
        let node = tree
            .find(&path)
            .ok_or_else(|| anyhow::anyhow!("{} is not in the daemon's tree", path.display()))?;
        Ok(Arc::new(if node == NodeId::ROOT {
            tree
        } else {
            extract_subtree(&tree, node)
        }))
    })
    .await?
}

async fn push_tree_and_classes(
    client: &mut Client,
    sink: &EventSink,
    snapshot: SnapshotRef,
    path: &Path,
) -> anyhow::Result<()> {
    let tree = load_snapshot(snapshot, path.to_path_buf()).await?;
    sink.send(BackendEvent::Tree(tree));
    if let Response::Classifications { items } = client
        .request(Request::Classifications {
            root: path.to_path_buf(),
        })
        .await?
    {
        sink.send(BackendEvent::ClearClassifications);
        sink.send(BackendEvent::Classifications(
            items
                .into_iter()
                .map(|c| (c.path, c.classification))
                .collect(),
        ));
    }
    Ok(())
}

impl Backend for DaemonBackend {
    fn start(&mut self, sink: EventSink) {
        self.sink = Some(sink.clone());
        let path = self.path.clone();
        let shared = Arc::clone(&self.shared);
        let options = self.options.clone();
        // Event subscription.
        let event_sink = sink;
        let event_path = path.clone();
        self.handle.spawn(async move {
            let started = Instant::now();
            let mut client = match Client::connect(&options).await {
                Ok(c) => c,
                Err(e) => {
                    event_sink.send(BackendEvent::Error(format!("daemon: {e}")));
                    return;
                }
            };
            let topics = vec![Topic::Scans, Topic::Volumes, Topic::Alerts, Topic::Actions];
            if client.request(Request::Subscribe { topics }).await.is_err() {
                return;
            }
            while let Ok(Some(event)) = client.next_event().await {
                if event_sink.is_closed() {
                    break;
                }
                let tree_root = shared.lock().tree_root.clone();
                match event {
                    Event::ScanProgress { root, progress }
                        if tree_root.as_ref() == Some(&root) || root == event_path =>
                    {
                        event_sink.send(BackendEvent::Progress(scan_progress(
                            &root, progress, started,
                        )));
                    }
                    Event::TreeUpdated(snapshot) if tree_root.as_ref() == Some(&snapshot.root) => {
                        let Ok(mut requests) = Client::connect(&options).await else {
                            continue;
                        };
                        if let Err(e) =
                            push_tree_and_classes(&mut requests, &event_sink, snapshot, &event_path)
                                .await
                        {
                            event_sink.send(BackendEvent::Error(format!("reload: {e:#}")));
                        }
                    }
                    Event::Volumes { volumes } => {
                        event_sink.send(BackendEvent::Volumes(volumes));
                    }
                    Event::ActionProgress { value } => {
                        let index_of = shared.lock().executing.clone();
                        if let (Some(index_of), Ok(p)) =
                            (index_of, serde_json::from_value::<ActionProgress>(value))
                            && let Some(mut ev) = execution_event(&p)
                        {
                            remap_index(&mut ev, &index_of);
                            event_sink.send(BackendEvent::Execution(ev));
                        }
                    }
                    Event::Alert(_) | Event::ScanProgress { .. } | Event::TreeUpdated(_) => {}
                }
            }
            event_sink.send(BackendEvent::Status(BackendStatus {
                mode: BackendMode::Daemon,
                connected: false,
                version: None,
                detail: Some("daemon connection closed".to_owned()),
            }));
        });
        // Initial tree.
        let shared = Arc::clone(&self.shared);
        self.with_client("initial scan", move |mut client, sink| async move {
            let snapshot = match client
                .request(Request::Scan {
                    root: path.clone(),
                    force: false,
                    wait: true,
                })
                .await?
            {
                Response::Snapshot(s) => s,
                other => anyhow::bail!("unexpected response {other:?}"),
            };
            shared.lock().tree_root = Some(snapshot.root.clone());
            push_tree_and_classes(&mut client, &sink, snapshot, &path).await
        });
    }

    fn expand(&mut self, path: PathBuf) {
        self.with_client("expand", move |mut client, _sink| async move {
            client
                .request(Request::Rescan { path, expand: true })
                .await?;
            Ok(())
        });
    }

    fn rescan(&mut self, path: PathBuf) {
        let root = self.path.clone();
        self.with_client("rescan", move |mut client, _sink| async move {
            let request = if path == root {
                Request::Scan {
                    root,
                    force: true,
                    wait: true,
                }
            } else {
                Request::Rescan {
                    path,
                    expand: false,
                }
            };
            client.request(request).await?;
            Ok(())
        });
    }

    fn build_plan(&mut self, request: PlanRequest) {
        let plans = Arc::clone(&self.plans);
        let settings = Arc::clone(&self.settings);
        let data_dir = self.data_dir.clone();
        self.with_client("plan", move |mut client, sink| async move {
            let mut plan: Plan = match client
                .request(Request::Plan {
                    paths: request.paths.clone(),
                })
                .await?
            {
                Response::Plan { plan } => serde_json::from_value(plan)?,
                other => anyhow::bail!("unexpected response {other:?}"),
            };
            apply_override(&mut plan, &request.action);
            let checked = plan.clone();
            let blocked = tokio::task::spawn_blocking(move || {
                bclean_engine::preflight_plan(&settings, &data_dir, &checked)
            })
            .await??;
            let id = {
                let mut p = plans.lock();
                p.next_id += 1;
                let id = p.next_id;
                p.by_id.insert(id, plan.clone());
                id
            };
            sink.send(BackendEvent::Plan(to_clean_plan(
                id,
                request.request_id,
                &plan,
                &blocked,
            )));
            Ok(())
        });
    }

    fn execute_plan(&mut self, plan: CleanPlan, dry_run: bool) {
        let Some(mut actions_plan) = self.plans.lock().by_id.get(&plan.id).cloned() else {
            self.sink()
                .send(BackendEvent::Error(format!("unknown plan {}", plan.id)));
            return;
        };
        let blocked: Vec<PathBuf> = plan
            .items
            .iter()
            .filter(|i| i.blocked.is_some())
            .map(|i| i.path.clone())
            .collect();
        actions_plan.items.retain(|i| !blocked.contains(&i.path));
        let index_of: Vec<usize> = actions_plan
            .items
            .iter()
            .filter_map(|i| plan.items.iter().position(|p| p.path == i.path))
            .collect();
        let shared = Arc::clone(&self.shared);
        self.with_client("clean", move |mut client, sink| async move {
            let started = Instant::now();
            shared.lock().executing = Some(index_of);
            sink.send(BackendEvent::Execution(ExecutionEvent::Started {
                plan_id: plan.id,
            }));
            let response = client
                .request(Request::Execute {
                    plan: serde_json::to_value(&actions_plan)?,
                    dry_run,
                })
                .await;
            // Give in-flight progress events a moment to arrive before finishing.
            tokio::time::sleep(Duration::from_millis(100)).await;
            shared.lock().executing = None;
            let report: ExecutionReport = match response? {
                Response::Actions { value } => serde_json::from_value(value)?,
                other => anyhow::bail!("unexpected response {other:?}"),
            };
            sink.send(BackendEvent::Execution(ExecutionEvent::Finished(
                clean_report(
                    plan.id,
                    &report,
                    started.elapsed(),
                    Some("daemon journal".to_owned()),
                ),
            )));
            Ok(())
        });
    }

    fn refresh_dashboard(&mut self) {
        self.with_client("dashboard", |mut client, sink| async move {
            if let Response::Volumes { volumes } = client.request(Request::Volumes).await? {
                sink.send(BackendEvent::Volumes(volumes));
            }
            if let Response::Alerts { alerts } = client.request(Request::Alerts).await? {
                sink.send(BackendEvent::Alerts(alerts));
            }
            if let Response::Status(status) = client.request(Request::Status).await? {
                sink.send(BackendEvent::Status(BackendStatus {
                    mode: BackendMode::Daemon,
                    connected: true,
                    version: Some(status.version),
                    detail: Some(format!(
                        "daemon pid {}, {} root(s), {} active alert(s)",
                        status.pid,
                        status.roots.len(),
                        status.active_alerts
                    )),
                }));
            }
            Ok(())
        });
    }
}
