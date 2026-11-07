//! An in-memory [`Backend`] for demos, tests, and UI development.
//!
//! It serves a fixed tree plus canned classifications and dashboard data. Cleaning is
//! **simulated**: nothing on disk is ever touched; executed items are only removed from the
//! backend's own copy of the tree.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bclean_model::{ActionSpec, AlertEvent, Classification, DashboardSummary, VolumeStats};
use bclean_tree::{NodeId, SizeMode, Tree};

use crate::backend::{
    ActionOverride, Backend, BackendEvent, BackendMode, BackendStatus, CleanPlan, CleanReport,
    EventSink, ExecutionEvent, ItemOutcome, PlanItem, PlanRequest, ScanProgress,
};

/// In-memory backend; see the module docs.
#[derive(Debug, Default)]
pub struct InMemoryBackend {
    tree: Option<Arc<Tree>>,
    classifications: BTreeMap<PathBuf, Classification>,
    subtrees: BTreeMap<PathBuf, Arc<Tree>>,
    /// Other roots (drives) served by [`Backend::open_root`], keyed by root path.
    roots: BTreeMap<PathBuf, Arc<Tree>>,
    volumes: Vec<VolumeStats>,
    alerts: Vec<AlertEvent>,
    dashboard: Option<DashboardSummary>,
    status: BackendStatus,
    item_delay: Duration,
    sink: Option<EventSink>,
    next_plan_id: u64,
    /// Every request received, for tests.
    pub log: Vec<String>,
}

impl InMemoryBackend {
    /// Creates a backend serving `tree`.
    #[must_use]
    pub fn new(tree: Tree) -> Self {
        Self {
            tree: Some(Arc::new(tree)),
            status: BackendStatus {
                mode: BackendMode::Local,
                connected: true,
                version: None,
                detail: Some("in-memory demo backend (cleaning is simulated)".to_owned()),
            },
            next_plan_id: 1,
            ..Self::default()
        }
    }

    /// Adds canned classifications (sent on start).
    #[must_use]
    pub fn with_classifications(
        mut self,
        items: impl IntoIterator<Item = (PathBuf, Classification)>,
    ) -> Self {
        self.classifications.extend(items);
        self
    }

    /// Registers the materialized subtree served when `path` (a summarized dir) is expanded.
    #[must_use]
    pub fn with_subtree(mut self, path: PathBuf, subtree: Tree) -> Self {
        self.subtrees.insert(path, Arc::new(subtree));
        self
    }

    /// Registers another root (a drive mounted inside the main tree) for
    /// [`Backend::open_root`].
    #[must_use]
    pub fn with_root(mut self, tree: Tree) -> Self {
        self.roots
            .insert(tree.root_path().to_path_buf(), Arc::new(tree));
        self
    }

    /// Sets dashboard volumes.
    #[must_use]
    pub fn with_volumes(mut self, volumes: Vec<VolumeStats>) -> Self {
        self.volumes = volumes;
        self
    }

    /// Sets the dashboard summary.
    #[must_use]
    pub fn with_dashboard(mut self, summary: DashboardSummary) -> Self {
        self.dashboard = Some(summary);
        self
    }

    /// Sets dashboard alerts.
    #[must_use]
    pub fn with_alerts(mut self, alerts: Vec<AlertEvent>) -> Self {
        self.alerts = alerts;
        self
    }

    /// Delay per executed item (runs execution on a thread when non-zero), for demos.
    #[must_use]
    pub const fn with_item_delay(mut self, delay: Duration) -> Self {
        self.item_delay = delay;
        self
    }

    fn send(&self, event: BackendEvent) {
        if let Some(sink) = &self.sink {
            sink.send(event);
        }
    }

    fn send_tree(&self) {
        let Some(tree) = self.tree.clone() else {
            return;
        };
        let info = tree.info(NodeId::ROOT);
        self.send(BackendEvent::Progress(ScanProgress {
            root: tree.root_path().to_path_buf(),
            running: false,
            files: info.items,
            dirs: 0,
            apparent_bytes: info.total_apparent,
            allocated_bytes: info.total_allocated,
            ..ScanProgress::default()
        }));
        self.send(BackendEvent::Tree(tree));
    }
}

impl Backend for InMemoryBackend {
    fn start(&mut self, sink: EventSink) {
        self.log.push("start".to_owned());
        self.sink = Some(sink);
        self.send_tree();
        let classes: Vec<_> = self
            .classifications
            .iter()
            .map(|(p, c)| (p.clone(), c.clone()))
            .collect();
        if !classes.is_empty() {
            self.send(BackendEvent::Classifications(classes));
        }
    }

    fn expand(&mut self, path: PathBuf) {
        self.log.push(format!("expand {}", path.display()));
        match self.subtrees.get(&path) {
            Some(subtree) => {
                if let Some(tree) = self.tree.as_mut()
                    && let Some(id) = tree.find(&path)
                    && id != NodeId::ROOT
                {
                    let tree = Arc::make_mut(tree);
                    let parent = tree.parent(id).unwrap_or(NodeId::ROOT);
                    tree.remove_subtree(id);
                    tree.graft(parent, subtree);
                }
                self.send(BackendEvent::Expanded {
                    path,
                    subtree: Arc::clone(subtree),
                });
            }
            None => self.send(BackendEvent::Error(format!(
                "cannot expand {}: no data",
                path.display()
            ))),
        }
    }

    fn rescan(&mut self, path: PathBuf) {
        self.log.push(format!("rescan {}", path.display()));
        self.send_tree();
    }

    fn open_root(&mut self, path: PathBuf) {
        self.log.push(format!("open root {}", path.display()));
        if self.tree.as_ref().is_some_and(|t| t.root_path() == path) {
            self.send_tree();
            return;
        }
        let Some(next) = self.roots.remove(&path) else {
            self.send(BackendEvent::Error(format!(
                "cannot open {}: no data",
                path.display()
            )));
            return;
        };
        if let Some(previous) = self.tree.replace(next) {
            self.roots
                .insert(previous.root_path().to_path_buf(), previous);
        }
        self.send_tree();
    }

    fn request_classifications(&mut self, paths: Vec<PathBuf>) {
        self.log.push(format!("classify {}", paths.len()));
    }

    fn build_plan(&mut self, request: PlanRequest) {
        self.log.push(format!("plan {}", request.paths.len()));
        let tree = self.tree.clone();
        let items = request
            .paths
            .iter()
            .map(|path| {
                let class = self.classifications.get(path);
                let action = match &request.action {
                    ActionOverride::Force(spec) => spec.clone(),
                    ActionOverride::RuleDefault => {
                        class.map_or(ActionSpec::Trash, |c| c.action.clone())
                    }
                };
                let node = tree.as_deref().and_then(|t| t.find(path).map(|id| (t, id)));
                let estimated_bytes = node.map_or(0, |(t, id)| t.size(id, SizeMode::Allocated));
                let blocked = match node {
                    None => Some("not found".to_owned()),
                    Some((_, NodeId::ROOT)) => Some("scan root is protected".to_owned()),
                    Some(_) if matches!(action, ActionSpec::Report) => {
                        Some("report-only rule".to_owned())
                    }
                    Some(_) => None,
                };
                PlanItem {
                    path: path.clone(),
                    action,
                    estimated_bytes,
                    blocked,
                    note: class.and_then(|c| c.rule_id.clone()),
                }
            })
            .collect();
        let id = self.next_plan_id;
        self.next_plan_id += 1;
        self.send(BackendEvent::Plan(CleanPlan {
            id,
            request_id: request.request_id,
            items,
        }));
    }

    fn execute_plan(&mut self, plan: CleanPlan, dry_run: bool) {
        self.log
            .push(format!("execute {} dry_run={dry_run}", plan.id));
        if !dry_run && let Some(tree) = self.tree.as_mut() {
            let tree = Arc::make_mut(tree);
            for item in plan.items.iter().filter(|i| i.blocked.is_none()) {
                if let Some(id) = tree.find(&item.path)
                    && id != NodeId::ROOT
                {
                    tree.remove_subtree(id);
                }
                self.classifications
                    .retain(|p, _| !p.starts_with(&item.path));
            }
        }
        let Some(sink) = self.sink.clone() else {
            return;
        };
        let delay = self.item_delay;
        let job = move || simulate_execution(&sink, &plan, dry_run, delay);
        if delay.is_zero() {
            job();
        } else {
            std::thread::spawn(job);
        }
    }

    fn refresh_dashboard(&mut self) {
        self.log.push("refresh_dashboard".to_owned());
        if let Some(summary) = &self.dashboard {
            self.send(BackendEvent::Dashboard(Box::new(summary.clone())));
        }
        self.send(BackendEvent::Status(self.status.clone()));
        self.send(BackendEvent::Volumes(self.volumes.clone()));
        self.send(BackendEvent::Alerts(self.alerts.clone()));
    }
}

fn simulate_execution(sink: &EventSink, plan: &CleanPlan, dry_run: bool, delay: Duration) {
    let started = Instant::now();
    sink.send(BackendEvent::Execution(ExecutionEvent::Started {
        plan_id: plan.id,
    }));
    let mut report = CleanReport {
        plan_id: plan.id,
        dry_run,
        ..CleanReport::default()
    };
    let mut freed = 0_u64;
    for (index, item) in plan.items.iter().enumerate() {
        sink.send(BackendEvent::Execution(ExecutionEvent::ItemStarted {
            index,
        }));
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        let outcome = if let Some(reason) = &item.blocked {
            report.skipped += 1;
            ItemOutcome::Skipped {
                reason: reason.clone(),
            }
        } else if dry_run {
            report.done += 1;
            report.estimated_bytes += item.estimated_bytes;
            ItemOutcome::Done { freed_bytes: None }
        } else {
            report.done += 1;
            report.estimated_bytes += item.estimated_bytes;
            freed += item.estimated_bytes;
            ItemOutcome::Done {
                freed_bytes: Some(item.estimated_bytes),
            }
        };
        if !sink.send(BackendEvent::Execution(ExecutionEvent::ItemFinished {
            index,
            outcome,
        })) {
            return;
        }
    }
    report.measured_freed_bytes = (!dry_run).then_some(freed);
    report.elapsed = started.elapsed();
    sink.send(BackendEvent::Execution(ExecutionEvent::Finished(report)));
}
