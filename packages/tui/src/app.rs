//! Application state machine: everything except drawing.
//!
//! [`App`] owns the [`Backend`], applies [`BackendEvent`]s, and handles input. It is fully
//! testable without a terminal; the runtime glue in `program.rs` only forwards events to it and
//! asks `render.rs` to draw it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bclean_model::{ActionSpec, AlertEvent, Classification, DashboardSummary, VolumeStats};
use bclean_tree::{EntryKind, NodeFlags, NodeId, SizeMode, Tree};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui_components::uniform_list::{UniformList, UniformListOutcome, UniformListState};

use crate::backend::{
    ActionOverride, Backend, BackendEvent, BackendStatus, CleanPlan, CleanReport, EventSink,
    ExecutionEvent, ItemOutcome, PlanRequest, ScanProgress,
};
use crate::dashboard::{DashGroup, DashPanel, DashState};
use crate::listing::{
    ClassStore, Lens, LensFilter, ListingCache, ListingKey, SortBy, SortSpec, top_level_paths,
};

/// Where to land in a tree requested with [`Backend::open_root`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPosition {
    root: PathBuf,
    cwd: PathBuf,
    selected: Option<PathBuf>,
}

/// Start-up options for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent start-up switches
pub struct TuiOptions {
    /// Title shown in the header.
    pub title: String,
    /// Start in apparent-size mode instead of allocated.
    pub apparent: bool,
    /// Show dotfiles.
    pub show_hidden: bool,
    /// Initial lens.
    pub lens: Lens,
    /// Initial sort.
    pub sort: SortSpec,
    /// Start on the dashboard instead of the browser.
    pub start_on_dashboard: bool,
    /// Directory to open once the first tree arrives (inside the scan root), if not the root.
    pub initial_path: Option<PathBuf>,
    /// Treat mount points as boundaries instead of opening their drives (`-x`).
    pub one_filesystem: bool,
    /// Minimum interval between frames (`None` = unthrottled).
    pub frame_interval: Option<Duration>,
}

impl Default for TuiOptions {
    fn default() -> Self {
        Self {
            title: "bclean".to_owned(),
            apparent: false,
            show_hidden: true,
            lens: Lens::All,
            sort: SortSpec::default(),
            start_on_dashboard: false,
            initial_path: None,
            one_filesystem: false,
            frame_interval: Some(Duration::from_millis(16)),
        }
    }
}

/// Top-level screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// Directory browser / lenses.
    Browser,
    /// Volumes, alerts, daemon status.
    Dashboard,
}

/// Severity of the transient status message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLevel {
    /// Informational.
    Info,
    /// Something went well.
    Success,
    /// Something needs attention.
    Warning,
    /// Something failed.
    Error,
}

/// Live state of a plan execution (and its final report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecState {
    /// The plan being executed.
    pub plan: CleanPlan,
    /// Dry run?
    pub dry_run: bool,
    /// Outcome per plan item, once finished.
    pub outcomes: Vec<Option<ItemOutcome>>,
    /// Item in progress.
    pub current: Option<usize>,
    /// Entries removed so far by the item in progress, and how many it holds (if known).
    pub current_progress: Option<(u64, Option<u64>)>,
    /// Final report.
    pub report: Option<CleanReport>,
    /// Cancellation requested.
    pub cancel_requested: bool,
}

impl ExecState {
    fn new(plan: CleanPlan, dry_run: bool) -> Self {
        let outcomes = vec![None; plan.items.len()];
        Self {
            plan,
            dry_run,
            outcomes,
            current: None,
            current_progress: None,
            report: None,
            cancel_requested: false,
        }
    }

    /// Number of finished items.
    #[must_use]
    pub fn finished(&self) -> usize {
        self.outcomes.iter().filter(|o| o.is_some()).count()
    }

    /// Estimated bytes of the items completed so far, plus the share of the item in progress
    /// already removed (by entry count).
    #[must_use]
    pub fn done_estimated_bytes(&self) -> u64 {
        let finished = self
            .outcomes
            .iter()
            .zip(&self.plan.items)
            .filter(|(o, _)| matches!(o, Some(ItemOutcome::Done { .. })))
            .map(|(_, item)| item.estimated_bytes)
            .fold(0, u64::saturating_add);
        finished.saturating_add(self.current_estimated_bytes())
    }

    /// Estimated bytes already removed by the item in progress.
    fn current_estimated_bytes(&self) -> u64 {
        let (Some(index), Some((removed, Some(entries)))) = (self.current, self.current_progress)
        else {
            return 0;
        };
        let Some(item) = self.plan.items.get(index) else {
            return 0;
        };
        let removed = removed.min(entries);
        u64::try_from(
            u128::from(item.estimated_bytes) * u128::from(removed) / u128::from(entries.max(1)),
        )
        .unwrap_or(u64::MAX)
    }
}

/// Modal overlays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// Key bindings.
    Help {
        /// Scroll offset.
        scroll: usize,
    },
    /// Waiting for the backend to build a plan.
    PlanPending {
        /// Request id.
        request_id: u64,
        /// Requested paths.
        paths: Vec<PathBuf>,
        /// Requested action override.
        action: ActionOverride,
        /// Items left out of the plan being rebuilt, carried over to it.
        excluded: BTreeSet<PathBuf>,
    },
    /// Plan review: a list of the items, the highlighted one's details, and confirmation.
    PlanPreview {
        /// The plan.
        plan: CleanPlan,
        /// Paths it was built from (to rebuild with another action).
        paths: Vec<PathBuf>,
        /// Action override used.
        action: ActionOverride,
        /// Highlighted item.
        cursor: usize,
        /// Items left out (space toggles); only the others run.
        excluded: BTreeSet<PathBuf>,
    },
    /// Execution progress, then the report.
    Execution(ExecState),
}

/// Outcome of handling one input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// Nothing changed.
    Ignored,
    /// State changed; redraw.
    Redraw,
    /// Leave the UI.
    Quit,
}

/// The whole UI state.
#[allow(clippy::struct_excessive_bools)] // independent view toggles, not a state machine
pub struct App<B: Backend> {
    pub(crate) backend: B,
    pub(crate) options: TuiOptions,
    pub(crate) tree: Option<Arc<Tree>>,
    pub(crate) tree_gen: u64,
    pub(crate) pending_tree: Option<Arc<Tree>>,
    pub(crate) progress: Option<ScanProgress>,
    pub(crate) classes: ClassStore,
    pub(crate) cache: ListingCache,
    pub(crate) view: View,
    pub(crate) lens: Lens,
    pub(crate) sort: SortSpec,
    pub(crate) apparent: bool,
    pub(crate) show_hidden: bool,
    pub(crate) filter: String,
    pub(crate) filter_editing: bool,
    pub(crate) cwd: NodeId,
    pub(crate) rows: UniformListState,
    lens_rows: BTreeMap<Lens, UniformListState>,
    pub(crate) marked: BTreeSet<PathBuf>,
    pub(crate) explain_open: bool,
    pub(crate) modal: Option<Modal>,
    pub(crate) volumes: Vec<VolumeStats>,
    pub(crate) alerts: Vec<AlertEvent>,
    pub(crate) dashboard: Option<Box<DashboardSummary>>,
    pub(crate) dash: DashState,
    /// Kind/category restriction of flat lenses (from a dashboard drill-down).
    pub(crate) lens_filter: Option<LensFilter>,
    initial_path: Option<PathBuf>,
    /// Roots browsed before the current one, each with the mount point that was opened from it
    /// (go up from a drive's top returns there).
    root_stack: Vec<(PathBuf, PathBuf)>,
    /// Position to take once the tree rooted at `.root` arrives (after [`Backend::open_root`]).
    pending_position: Option<PendingPosition>,
    pub(crate) status: Option<BackendStatus>,
    pub(crate) message: Option<(String, MessageLevel)>,
    pub(crate) expanding: BTreeSet<PathBuf>,
    enter_after_expand: Option<PathBuf>,
    requested: BTreeSet<PathBuf>,
    next_request_id: u64,
    /// Rows available to the list in the last frame (for paging).
    pub(crate) viewport_rows: usize,
    /// Where the browser rows were painted, in terminal coordinates (set by the renderer).
    pub(crate) list_area: Rect,
    /// Animation counter.
    pub(crate) tick: u64,
}

impl<B: Backend> App<B> {
    /// Creates the app and starts the backend with `sink`.
    pub fn new(mut backend: B, options: TuiOptions, sink: EventSink) -> Self {
        backend.start(sink);
        backend.refresh_dashboard();
        let initial_path = options.initial_path.clone();
        Self {
            backend,
            view: if options.start_on_dashboard {
                View::Dashboard
            } else {
                View::Browser
            },
            lens: options.lens,
            sort: options.sort,
            apparent: options.apparent,
            show_hidden: options.show_hidden,
            options,
            tree: None,
            tree_gen: 0,
            pending_tree: None,
            progress: None,
            classes: ClassStore::default(),
            cache: ListingCache::default(),
            filter: String::new(),
            filter_editing: false,
            cwd: NodeId::ROOT,
            rows: UniformListState::new(),
            lens_rows: BTreeMap::new(),
            marked: BTreeSet::new(),
            explain_open: false,
            modal: None,
            volumes: Vec::new(),
            alerts: Vec::new(),
            dashboard: None,
            dash: DashState::default(),
            lens_filter: None,
            initial_path,
            root_stack: Vec::new(),
            pending_position: None,
            status: None,
            message: None,
            expanding: BTreeSet::new(),
            enter_after_expand: None,
            requested: BTreeSet::new(),
            next_request_id: 1,
            viewport_rows: 20,
            list_area: Rect::new(0, 0, 0, 0),
            tick: 0,
        }
    }

    /// The backend (for tests and integration).
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Mutable backend access.
    pub const fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Current tree, if any has arrived.
    #[must_use]
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_deref()
    }

    /// Current directory.
    #[must_use]
    pub const fn cwd(&self) -> NodeId {
        self.cwd
    }

    /// Current lens.
    #[must_use]
    pub const fn lens(&self) -> Lens {
        self.lens
    }

    /// Current view.
    #[must_use]
    pub const fn view(&self) -> View {
        self.view
    }

    /// Open modal, if any.
    #[must_use]
    pub const fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    /// Marked paths.
    #[must_use]
    pub const fn marked(&self) -> &BTreeSet<PathBuf> {
        &self.marked
    }

    /// Size mode in effect.
    #[must_use]
    pub const fn size_mode(&self) -> SizeMode {
        if self.apparent {
            SizeMode::Apparent
        } else {
            SizeMode::Allocated
        }
    }

    /// Classification of a node, if known.
    #[must_use]
    pub fn classification(&self, id: NodeId) -> Option<&Classification> {
        self.classes.get(id).map(AsRef::as_ref)
    }

    fn listing_key(&self) -> ListingKey {
        let class_sensitive = self.lens.is_flat() || self.sort.by == SortBy::Score;
        ListingKey {
            dir: self.cwd,
            lens: self.lens,
            sort: self.sort,
            apparent: self.apparent,
            show_hidden: self.show_hidden,
            filter: self.filter.clone(),
            class_filter: if self.lens.is_flat() {
                self.lens_filter.clone()
            } else {
                None
            },
            tree_gen: self.tree_gen,
            class_gen: if class_sensitive {
                self.classes.generation()
            } else {
                0
            },
        }
    }

    /// Rows of the current list (cached).
    pub fn listing(&mut self) -> Arc<Vec<NodeId>> {
        let Some(tree) = self.tree.clone() else {
            return Arc::new(Vec::new());
        };
        let key = self.listing_key();
        self.cache.get(&key, &tree, &self.classes)
    }

    /// Selected node of the current list.
    pub fn selected(&mut self) -> Option<NodeId> {
        let listing = self.listing();
        listing.get(self.rows.selected).copied()
    }

    fn clamp_rows(&mut self) {
        let len = self.listing().len();
        self.rows.clamp(len, self.viewport_rows);
    }

    fn select_node(&mut self, id: NodeId) {
        let listing = self.listing();
        if let Some(index) = listing.iter().position(|n| *n == id) {
            self.rows.selected = index;
        }
        self.clamp_rows();
    }

    fn info(&mut self, text: impl Into<String>, level: MessageLevel) {
        self.message = Some((text.into(), level));
    }

    // ----- backend events ---------------------------------------------------------------------

    /// Applies one backend event.
    pub fn apply(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Tree(tree) => {
                if self.defer_tree_updates() {
                    self.pending_tree = Some(tree);
                } else {
                    self.replace_tree(tree);
                }
            }
            BackendEvent::Progress(progress) => {
                if !progress.running {
                    self.expanding.clear();
                }
                self.progress = Some(progress);
            }
            BackendEvent::Expanded { path, subtree } => self.apply_expanded(&path, &subtree),
            BackendEvent::Classifications(items) => {
                self.classes.insert(self.tree.as_deref(), items);
            }
            BackendEvent::ClearClassifications => {
                self.classes.clear();
                self.requested.clear();
            }
            BackendEvent::Plan(plan) => self.apply_plan(plan),
            BackendEvent::Execution(event) => self.apply_execution(event),
            BackendEvent::Dashboard(summary) => {
                self.dash.clamp(&summary);
                self.dashboard = Some(summary);
            }
            BackendEvent::Volumes(volumes) => self.volumes = volumes,
            BackendEvent::Alerts(alerts) => self.alerts = alerts,
            BackendEvent::Status(status) => self.status = Some(status),
            BackendEvent::Error(error) => {
                if let Some(Modal::PlanPending { .. }) = self.modal {
                    self.modal = None;
                }
                self.info(error, MessageLevel::Error);
            }
        }
        self.clamp_rows();
        self.request_visible_classifications();
    }

    const fn defer_tree_updates(&self) -> bool {
        matches!(
            self.modal,
            Some(Modal::PlanPreview { .. } | Modal::Execution(_) | Modal::PlanPending { .. })
        )
    }

    fn replace_tree(&mut self, tree: Arc<Tree>) {
        let (cwd_path, selected_path) = match self.pending_position.take() {
            Some(pending) if pending.root == tree.root_path() => {
                if self
                    .message
                    .as_ref()
                    .is_some_and(|(text, _)| text.starts_with("opening "))
                {
                    self.message = None;
                }
                self.lens_rows.clear();
                (Some(pending.cwd), pending.selected)
            }
            pending => {
                // A late update of the previous root: keep waiting for the requested one.
                self.pending_position = pending;
                self.capture_position()
            }
        };
        let current = Arc::clone(&tree);
        self.tree = Some(tree);
        self.on_tree_changed(cwd_path, selected_path);
        let tree = current;
        if let Some(path) = self.initial_path.take()
            && let Some((id, _)) = tree.find_nearest(&path)
        {
            let id = if tree.kind(id) == EntryKind::Dir {
                id
            } else {
                tree.parent(id).unwrap_or(NodeId::ROOT)
            };
            self.enter_dir(id);
        }
        self.resolve_expansions();
    }

    /// Settles expansions that a new tree completed. The daemon backend reports an expansion
    /// only by publishing the updated tree (no [`BackendEvent::Expanded`]), so without this the
    /// directory stayed "expanding…" forever and was never entered.
    fn resolve_expansions(&mut self) {
        let Some(tree) = self.tree.clone() else {
            return;
        };
        let before = self.expanding.len();
        self.expanding.retain(|path| {
            tree.find(path).is_some_and(|id| {
                tree.flags(id).contains(NodeFlags::SUMMARIZED) && !tree.has_children(id)
            })
        });
        if self.expanding.len() == before {
            return;
        }
        if self
            .message
            .as_ref()
            .is_some_and(|(text, _)| text.starts_with("expanding "))
        {
            self.message = None;
        }
        if let Some(path) = self.enter_after_expand.take() {
            if self.expanding.contains(&path) {
                self.enter_after_expand = Some(path);
            } else if let Some(id) = tree.find(&path)
                && tree.kind(id) == EntryKind::Dir
            {
                self.filter.clear();
                self.enter_dir(id);
            }
        }
    }

    fn capture_position(&mut self) -> (Option<PathBuf>, Option<PathBuf>) {
        let selected = self.selected();
        let Some(tree) = self.tree.as_deref() else {
            return (None, None);
        };
        let cwd = tree.contains(self.cwd).then(|| tree.path(self.cwd));
        let selected = selected
            .filter(|id| tree.contains(*id))
            .map(|id| tree.path(id));
        (cwd, selected)
    }

    fn on_tree_changed(&mut self, cwd_path: Option<PathBuf>, selected_path: Option<PathBuf>) {
        self.tree_gen += 1;
        self.cache.clear();
        let Some(tree) = self.tree.clone() else {
            return;
        };
        self.classes.reindex(&tree);
        self.cwd = cwd_path
            .and_then(|p| tree.find_nearest(&p))
            .map_or(NodeId::ROOT, |(id, _)| id);
        if tree.kind(self.cwd) != EntryKind::Dir {
            self.cwd = tree.parent(self.cwd).unwrap_or(NodeId::ROOT);
        }
        if let Some(id) = selected_path.and_then(|p| tree.find(&p)) {
            self.select_node(id);
        } else {
            self.clamp_rows();
        }
    }

    fn apply_expanded(&mut self, path: &Path, subtree: &Arc<Tree>) {
        self.expanding.remove(path);
        let (cwd_path, selected_path) = self.capture_position();
        let Some(tree_arc) = self.tree.as_mut() else {
            return;
        };
        let tree = Arc::make_mut(tree_arc);
        match tree.find(path) {
            Some(NodeId::ROOT) => {
                *tree_arc = Arc::clone(subtree);
            }
            Some(id) => {
                let parent = tree.parent(id).unwrap_or(NodeId::ROOT);
                tree.remove_subtree(id);
                tree.graft(parent, subtree);
            }
            None => {
                self.info(
                    format!("expanded path not in tree: {}", path.display()),
                    MessageLevel::Warning,
                );
                return;
            }
        }
        self.on_tree_changed(cwd_path, selected_path);
        if self.enter_after_expand.as_deref() == Some(path) {
            self.enter_after_expand = None;
            if let Some(id) = self.tree.as_deref().and_then(|t| t.find(path)) {
                self.enter_dir(id);
            }
        }
    }

    fn apply_plan(&mut self, plan: CleanPlan) {
        let Some(Modal::PlanPending {
            request_id,
            paths,
            action,
            excluded,
        }) = self.modal.take()
        else {
            return; // cancelled or stale
        };
        if plan.request_id != request_id {
            self.modal = Some(Modal::PlanPending {
                request_id,
                paths,
                action,
                excluded,
            });
            return;
        }
        self.modal = Some(Modal::PlanPreview {
            plan,
            paths,
            action,
            cursor: 0,
            excluded,
        });
    }

    fn apply_execution(&mut self, event: ExecutionEvent) {
        let Some(Modal::Execution(exec)) = self.modal.as_mut() else {
            return;
        };
        match event {
            ExecutionEvent::Started { .. } => {}
            ExecutionEvent::ItemStarted { index } => {
                exec.current = Some(index);
                exec.current_progress = None;
            }
            ExecutionEvent::ItemProgress {
                index,
                removed,
                entries,
            } => {
                if exec.current == Some(index) {
                    exec.current_progress = Some((removed, entries));
                }
            }
            ExecutionEvent::ItemFinished { index, outcome } => {
                let done = matches!(outcome, ItemOutcome::Done { .. }) && !exec.dry_run;
                let path = exec.plan.items.get(index).map(|item| item.path.clone());
                if let Some(slot) = exec.outcomes.get_mut(index) {
                    *slot = Some(outcome);
                }
                if exec.current == Some(index) {
                    exec.current = None;
                    exec.current_progress = None;
                }
                if done && let Some(path) = path {
                    self.remove_path_locally(&path);
                }
            }
            ExecutionEvent::Finished(report) => {
                exec.current = None;
                exec.current_progress = None;
                exec.report = Some(report);
                self.backend.refresh_dashboard();
            }
        }
    }

    /// Removes a cleaned path from the local tree so the UI reflects it immediately.
    fn remove_path_locally(&mut self, path: &Path) {
        self.marked.retain(|m| !m.starts_with(path));
        let (cwd_path, selected_path) = self.capture_position();
        let Some(tree_arc) = self.tree.as_mut() else {
            return;
        };
        let Some(id) = tree_arc.find(path) else {
            return;
        };
        if id == NodeId::ROOT {
            return;
        }
        Arc::make_mut(tree_arc).remove_subtree(id);
        self.on_tree_changed(cwd_path, selected_path);
    }

    // ----- input ------------------------------------------------------------------------------

    /// Handles one terminal event.
    pub fn handle_event(&mut self, event: &Event) -> Handled {
        match event {
            Event::Key(stroke) => {
                let handled = self.handle_key(*stroke);
                self.request_visible_classifications();
                handled
            }
            Event::Mouse(mouse) => {
                if self.modal.is_none() && self.view == View::Browser {
                    let list = UniformList::new("rows", self.listing().len());
                    if list.handle_event(self.list_area, &mut self.rows, event)
                        == UniformListOutcome::Ignored
                    {
                        return Handled::Ignored;
                    }
                    self.request_visible_classifications();
                    return Handled::Redraw;
                }
                let delta = match mouse.kind {
                    MouseEventKind::ScrollUp => -3,
                    MouseEventKind::ScrollDown => 3,
                    _ => return Handled::Ignored,
                };
                if let Some(Modal::PlanPreview { plan, cursor, .. }) = self.modal.as_mut() {
                    *cursor = cursor
                        .saturating_add_signed(delta)
                        .min(plan.items.len().saturating_sub(1));
                    return Handled::Redraw;
                }
                Handled::Ignored
            }
            Event::Resize(_) => {
                self.clamp_rows();
                Handled::Redraw
            }
            Event::Paste(text) if self.filter_editing => {
                self.filter.push_str(text.trim());
                self.on_filter_changed();
                Handled::Redraw
            }
            _ => Handled::Ignored,
        }
    }

    /// Advances animations; returns `true` if a redraw is useful.
    pub fn on_tick(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);
        let scanning = self.progress.as_ref().is_some_and(|p| p.running);
        let executing = matches!(&self.modal, Some(Modal::Execution(e)) if e.report.is_none());
        scanning || executing || matches!(self.modal, Some(Modal::PlanPending { .. }))
    }

    /// Handles one key stroke.
    pub fn handle_key(&mut self, stroke: KeyStroke) -> Handled {
        let key = Key::from(stroke);
        if key.ctrl && key.code == KeyCode::Char('c') {
            return Handled::Quit;
        }
        // Any key dismisses the transient message; handlers may set a new one.
        self.message = None;
        if self.filter_editing
            && let Some(handled) = self.handle_filter_key(&key)
        {
            return handled;
        }
        if self.modal.is_some() {
            return self.handle_modal_key(&key);
        }
        match self.view {
            View::Dashboard => self.handle_dashboard_key(&key),
            View::Browser => self.handle_browser_key(&key),
        }
    }

    fn handle_filter_key(&mut self, key: &Key) -> Option<Handled> {
        match key.code {
            KeyCode::Enter => self.filter_editing = false,
            KeyCode::Escape => {
                self.filter_editing = false;
                self.filter.clear();
                self.on_filter_changed();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.on_filter_changed();
            }
            KeyCode::Char(_) | KeyCode::Space if !key.ctrl && !key.alt => {
                self.filter.push(key.ch().unwrap_or(' '));
                self.on_filter_changed();
            }
            // Navigation keys keep working while typing.
            _ => return None,
        }
        Some(Handled::Redraw)
    }

    fn on_filter_changed(&mut self) {
        self.rows = UniformListState::new();
        self.clamp_rows();
    }

    fn handle_dashboard_key(&mut self, key: &Key) -> Handled {
        let len = self.dashboard.as_deref().map_or(0, |summary| {
            crate::dashboard::rows(summary, &self.dash, self.dash.focus)
        });
        match key.code {
            KeyCode::Tab => self.dash.focus = self.dash.focus.cycle(key.shift),
            KeyCode::Up => self.dash.move_by(-1, len),
            KeyCode::Down => self.dash.move_by(1, len),
            KeyCode::PageUp => self.dash.move_by(-10, len),
            KeyCode::PageDown => self.dash.move_by(10, len),
            KeyCode::Home => self.dash.jump(false, len),
            KeyCode::End => self.dash.jump(true, len),
            KeyCode::Enter | KeyCode::Right => self.dash_open(),
            KeyCode::Space => self.dash_toggle_mark(),
            KeyCode::Escape => self.view = View::Browser,
            KeyCode::Char(_) if !key.ctrl => match key.ch() {
                Some('q') => return Handled::Quit,
                Some('k') => self.dash.move_by(-1, len),
                Some('j') => self.dash.move_by(1, len),
                Some('g') => self.dash.jump(false, len),
                Some('G') => self.dash.jump(true, len),
                Some('l') => self.dash_open(),
                Some('D' | 'b') => self.view = View::Browser,
                Some('c') => self.dash_clean_safe(),
                Some('p') => self.dash_plan(),
                Some('x') => {
                    let count = self.marked.len();
                    self.marked.clear();
                    self.info(format!("cleared {count} marks"), MessageLevel::Info);
                }
                Some('v') => {
                    self.dash.group = match self.dash.group {
                        DashGroup::Kind => DashGroup::Category,
                        DashGroup::Category => DashGroup::Kind,
                    };
                    self.clamp_dash();
                }
                Some('w') => {
                    self.dash.growth_week = !self.dash.growth_week;
                    self.clamp_dash();
                }
                Some('r') => {
                    self.backend.refresh_dashboard();
                    self.info("refreshing dashboard…", MessageLevel::Info);
                }
                Some('?') => self.modal = Some(Modal::Help { scroll: 0 }),
                _ => return Handled::Ignored,
            },
            _ => return Handled::Ignored,
        }
        Handled::Redraw
    }

    fn clamp_dash(&mut self) {
        if let Some(summary) = self.dashboard.as_deref() {
            self.dash.clamp(summary);
        }
    }

    /// Paths behind the focused dashboard row: a candidate, or a project's reclaimable entries
    /// (never the project itself).
    fn dash_row_paths(&self) -> Vec<PathBuf> {
        let Some(summary) = self.dashboard.as_deref() else {
            return Vec::new();
        };
        let index = self.dash.selected(self.dash.focus);
        match self.dash.focus {
            DashPanel::Wins => summary
                .top_items
                .get(index)
                .map(|item| vec![item.path.clone()])
                .unwrap_or_default(),
            DashPanel::Projects => summary
                .projects
                .get(index)
                .map(|p| p.paths.clone())
                .unwrap_or_default(),
            DashPanel::Kinds | DashPanel::Growth => Vec::new(),
        }
    }

    fn dash_toggle_mark(&mut self) {
        let paths = self.dash_row_paths();
        if paths.is_empty() {
            self.info(
                "open this row (enter) to pick items to mark",
                MessageLevel::Info,
            );
            return;
        }
        if paths.iter().all(|p| self.marked.contains(p)) {
            for path in &paths {
                self.marked.remove(path);
            }
        } else {
            self.marked.extend(paths);
        }
        if self.dash.focus == DashPanel::Wins {
            let len = self.dashboard.as_deref().map_or(0, |s| s.top_items.len());
            self.dash.move_by(1, len);
        }
    }

    fn dash_plan(&mut self) {
        let paths = if self.marked.is_empty() {
            self.dash_row_paths()
        } else {
            top_level_paths(self.marked.iter().cloned())
        };
        if paths.is_empty() {
            self.info(
                "mark items (space) or select a candidate or project first",
                MessageLevel::Warning,
            );
            return;
        }
        self.request_plan(paths, ActionOverride::RuleDefault);
    }

    fn dash_clean_safe(&mut self) {
        let paths = self
            .dashboard
            .as_deref()
            .map(|s| s.reclaimable.safe_paths.clone())
            .unwrap_or_default();
        if paths.is_empty() {
            self.info("nothing is safe to clean automatically", MessageLevel::Info);
            return;
        }
        self.request_plan(paths, ActionOverride::RuleDefault);
    }

    /// Drills into the focused dashboard row: a kind or ecosystem opens the Reclaimable lens
    /// filtered to it; a candidate, project, or growing directory opens it in the browser.
    fn dash_open(&mut self) {
        let Some(summary) = self.dashboard.clone() else {
            return;
        };
        let index = self.dash.selected(self.dash.focus);
        match self.dash.focus {
            DashPanel::Kinds => {
                let filter = match self.dash.group {
                    DashGroup::Kind => summary.by_kind.get(index).map(|b| LensFilter::Kind(b.kind)),
                    DashGroup::Category => summary
                        .by_category
                        .get(index)
                        .map(|b| LensFilter::Category(b.category.clone())),
                };
                let Some(filter) = filter else {
                    return;
                };
                self.view = View::Browser;
                self.set_lens(Lens::Reclaimable);
                self.lens_filter = Some(filter);
                self.filter.clear();
                self.rows = UniformListState::new();
                self.clamp_rows();
            }
            DashPanel::Wins => {
                if let Some(item) = summary.top_items.get(index) {
                    self.show_in_browser(&item.path, false);
                }
            }
            DashPanel::Projects => {
                if let Some(project) = summary.projects.get(index) {
                    self.show_in_browser(&project.path, true);
                }
            }
            DashPanel::Growth => {
                if let Some(item) = crate::dashboard::growth(&summary, &self.dash).get(index) {
                    self.show_in_browser(&item.path, true);
                }
            }
        }
    }

    /// Opens the browser at `path`: inside it (`enter`), or at its parent with it selected.
    fn show_in_browser(&mut self, path: &Path, enter: bool) {
        let Some(tree) = self.tree.clone() else {
            self.info("the index is still loading", MessageLevel::Info);
            return;
        };
        let Some(id) = tree.find(path) else {
            self.info(
                format!(
                    "{} is outside {} (run bclean {} to browse it)",
                    crate::dashboard::tilde(path),
                    crate::dashboard::tilde(tree.root_path()),
                    crate::dashboard::tilde(path)
                ),
                MessageLevel::Warning,
            );
            return;
        };
        self.view = View::Browser;
        self.lens_rows.insert(self.lens, self.rows);
        self.lens = Lens::All;
        self.lens_filter = None;
        self.filter.clear();
        if enter && tree.kind(id) == EntryKind::Dir {
            if tree.flags(id).contains(NodeFlags::SUMMARIZED) && !tree.has_children(id) {
                // Summarized (`target/`): select it so enter expands it.
                self.cwd = tree.parent(id).unwrap_or(NodeId::ROOT);
                self.rows = UniformListState::new();
                self.select_node(id);
            } else {
                self.enter_dir(id);
            }
        } else {
            self.cwd = tree.parent(id).unwrap_or(NodeId::ROOT);
            self.rows = UniformListState::new();
            self.select_node(id);
        }
    }

    #[allow(clippy::too_many_lines)] // one flat key map reads best
    fn handle_browser_key(&mut self, key: &Key) -> Handled {
        let len = self.listing().len();
        let viewport = self.viewport_rows;
        match key.code {
            KeyCode::Up => self.rows.move_by(-1, len, viewport),
            KeyCode::Down => self.rows.move_by(1, len, viewport),
            KeyCode::PageUp => self.rows.page(false, len, viewport),
            KeyCode::PageDown => self.rows.page(true, len, viewport),
            KeyCode::Home => self.rows.top(len, viewport),
            KeyCode::End => self.rows.bottom(len, viewport),
            KeyCode::Enter | KeyCode::Right => self.open_selected(),
            KeyCode::Left | KeyCode::Backspace => self.go_up(),
            KeyCode::Tab => self.set_lens(self.lens.cycle(key.shift)),
            KeyCode::Space => self.toggle_mark(),
            KeyCode::Escape => {
                if self.explain_open {
                    self.explain_open = false;
                } else if self.lens_filter.is_some() {
                    self.lens_filter = None;
                    self.rows = UniformListState::new();
                    self.clamp_rows();
                } else if !self.filter.is_empty() {
                    self.filter.clear();
                    self.on_filter_changed();
                } else {
                    return Handled::Ignored;
                }
            }
            KeyCode::Char(_) => {
                if key.ctrl {
                    match key.code {
                        KeyCode::Char('d') => self.rows.page(true, len, viewport),
                        KeyCode::Char('u') => self.rows.page(false, len, viewport),
                        _ => return Handled::Ignored,
                    }
                    return Handled::Redraw;
                }
                match key.ch() {
                    Some('q') => return Handled::Quit,
                    Some('k') => self.rows.move_by(-1, len, viewport),
                    Some('j') => self.rows.move_by(1, len, viewport),
                    Some('g') => self.rows.top(len, viewport),
                    Some('G') => self.rows.bottom(len, viewport),
                    Some('l') => self.open_selected(),
                    Some('h') => self.go_up(),
                    Some('s') => self.set_sort(SortBy::Size),
                    Some('n') => self.set_sort(SortBy::Name),
                    Some('c') => self.set_sort(SortBy::Items),
                    Some('m') => self.set_sort(SortBy::Mtime),
                    Some('o') => self.set_sort(SortBy::Score),
                    Some('a') => {
                        self.apparent = !self.apparent;
                        self.clamp_rows();
                    }
                    Some('.') => {
                        self.show_hidden = !self.show_hidden;
                        self.clamp_rows();
                    }
                    Some('/') => self.filter_editing = true,
                    Some('i') => self.explain_open = !self.explain_open,
                    Some('?') => self.modal = Some(Modal::Help { scroll: 0 }),
                    Some('D') => {
                        self.view = View::Dashboard;
                        self.backend.refresh_dashboard();
                    }
                    Some('x') => {
                        let count = self.marked.len();
                        self.marked.clear();
                        self.info(format!("cleared {count} marks"), MessageLevel::Info);
                    }
                    Some('p') => self.request_plan_for_marked(),
                    Some('d') => self.request_delete_selected(),
                    Some('r') => self.rescan_current(),
                    Some(c @ '1'..='5') => {
                        let index = c as usize - '1' as usize;
                        self.set_lens(Lens::ALL[index]);
                    }
                    _ => return Handled::Ignored,
                }
            }
            _ => return Handled::Ignored,
        }
        Handled::Redraw
    }

    fn set_sort(&mut self, by: SortBy) {
        let selected = self.selected();
        self.sort = self.sort.toggled(by);
        if let Some(id) = selected {
            self.select_node(id);
        }
    }

    fn set_lens(&mut self, lens: Lens) {
        if !lens.available() {
            self.info(
                "\"grew since…\" needs index history (not available yet)",
                MessageLevel::Warning,
            );
            return;
        }
        if lens == self.lens {
            return;
        }
        self.lens_filter = None;
        self.lens_rows.insert(self.lens, self.rows);
        self.lens = lens;
        self.rows = self.lens_rows.get(&lens).copied().unwrap_or_default();
        if lens.is_flat() && self.sort.by == SortBy::Size && !self.sort.reverse {
            // Flattened lenses are most useful by score; keep explicit user choices otherwise.
            self.sort = SortSpec {
                by: SortBy::Score,
                reverse: false,
            };
        }
        self.clamp_rows();
    }

    fn enter_dir(&mut self, id: NodeId) {
        self.cwd = id;
        self.rows = UniformListState::new();
        self.clamp_rows();
    }

    fn open_selected(&mut self) {
        let Some(id) = self.selected() else {
            return;
        };
        let Some(tree) = self.tree.clone() else {
            return;
        };
        if self.lens.is_flat() {
            // Jump to the item's location in the browser.
            self.lens_rows.insert(self.lens, self.rows);
            self.lens = Lens::All;
            self.filter.clear();
            self.cwd = tree.parent(id).unwrap_or(NodeId::ROOT);
            self.rows = UniformListState::new();
            self.select_node(id);
            return;
        }
        if tree.kind(id) != EntryKind::Dir {
            return;
        }
        if tree.flags(id).contains(NodeFlags::OTHER_FS) {
            self.open_drive(&tree, id);
            return;
        }
        if tree.flags(id).contains(NodeFlags::SUMMARIZED) && !tree.has_children(id) {
            let path = tree.path(id);
            if self.expanding.insert(path.clone()) {
                self.backend.expand(path.clone());
            }
            self.info(
                format!("expanding {}…", tree.name(id).to_string_lossy()),
                MessageLevel::Info,
            );
            self.enter_after_expand = Some(path);
            return;
        }
        self.filter.clear();
        self.enter_dir(id);
    }

    /// Opens the drive mounted at `id` (another filesystem) as the browsed root.
    fn open_drive(&mut self, tree: &Tree, id: NodeId) {
        let mount = tree.path(id);
        if self.options.one_filesystem {
            self.info(
                format!(
                    "{} is another filesystem (-x)",
                    crate::dashboard::tilde(&mount)
                ),
                MessageLevel::Info,
            );
            return;
        }
        self.root_stack
            .push((tree.root_path().to_path_buf(), mount.clone()));
        self.open_root(mount.clone(), mount, None);
    }

    /// Asks the backend for the tree rooted at `root`, to be shown at `cwd` with `selected`.
    fn open_root(&mut self, root: PathBuf, cwd: PathBuf, selected: Option<PathBuf>) {
        self.info(
            format!("opening {}…", crate::dashboard::tilde(&root)),
            MessageLevel::Info,
        );
        self.pending_position = Some(PendingPosition {
            root: root.clone(),
            cwd,
            selected,
        });
        self.filter.clear();
        self.backend.open_root(root);
    }

    fn go_up(&mut self) {
        if self.lens.is_flat() {
            return;
        }
        let Some(tree) = self.tree.clone() else {
            return;
        };
        let Some(parent) = tree.parent(self.cwd) else {
            // At the top of a drive opened from another root: back to that root.
            if let Some((previous, mount)) = self.root_stack.pop() {
                let cwd = mount
                    .parent()
                    .map_or_else(|| previous.clone(), Path::to_path_buf);
                self.open_root(previous, cwd, Some(mount));
            }
            return;
        };
        let child = self.cwd;
        self.filter.clear();
        self.cwd = parent;
        self.rows = UniformListState::new();
        self.select_node(child);
    }

    fn toggle_mark(&mut self) {
        let Some(id) = self.selected() else {
            return;
        };
        let Some(tree) = self.tree.as_deref() else {
            return;
        };
        if id == NodeId::ROOT {
            return;
        }
        let path = tree.path(id);
        if !self.marked.remove(&path) {
            self.marked.insert(path);
        }
        let len = self.listing().len();
        self.rows.move_by(1, len, self.viewport_rows);
    }

    /// Total estimated bytes of the marked items (top-level only).
    #[must_use]
    pub fn marked_bytes(&self) -> u64 {
        let Some(tree) = self.tree.as_deref() else {
            return 0;
        };
        top_level_paths(self.marked.iter().cloned())
            .iter()
            .filter_map(|p| tree.find(p))
            .map(|id| tree.size(id, self.size_mode()))
            .fold(0, u64::saturating_add)
    }

    fn request_plan(&mut self, paths: Vec<PathBuf>, action: ActionOverride) {
        self.request_plan_excluding(paths, action, BTreeSet::new());
    }

    fn request_plan_excluding(
        &mut self,
        paths: Vec<PathBuf>,
        action: ActionOverride,
        excluded: BTreeSet<PathBuf>,
    ) {
        if paths.is_empty() {
            self.info("nothing selected", MessageLevel::Warning);
            return;
        }
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.modal = Some(Modal::PlanPending {
            request_id,
            paths: paths.clone(),
            action: action.clone(),
            excluded,
        });
        self.backend.build_plan(PlanRequest {
            request_id,
            paths,
            action,
        });
    }

    fn selected_path(&mut self) -> Option<PathBuf> {
        let id = self.selected()?;
        let tree = self.tree.as_deref()?;
        (id != NodeId::ROOT).then(|| tree.path(id))
    }

    fn request_plan_for_marked(&mut self) {
        let paths = if self.marked.is_empty() {
            self.selected_path().into_iter().collect()
        } else {
            top_level_paths(self.marked.iter().cloned())
        };
        self.request_plan(paths, ActionOverride::RuleDefault);
    }

    fn request_delete_selected(&mut self) {
        let paths = self.selected_path().into_iter().collect();
        self.request_plan(paths, ActionOverride::Force(ActionSpec::Delete));
    }

    fn rescan_current(&mut self) {
        let Some(tree) = self.tree.as_deref() else {
            return;
        };
        let path = if self.lens.is_flat() {
            match self.selected() {
                Some(id) => self.tree.as_deref().map(|t| t.path(id)),
                None => None,
            }
        } else {
            Some(tree.path(self.cwd))
        };
        if let Some(path) = path {
            self.info(
                format!("rescanning {}…", path.display()),
                MessageLevel::Info,
            );
            self.expanding.insert(path.clone());
            self.backend.rescan(path);
        }
    }

    fn handle_modal_key(&mut self, key: &Key) -> Handled {
        let Some(modal) = self.modal.take() else {
            return Handled::Ignored;
        };
        let close = matches!(key.code, KeyCode::Escape) || key.ch() == Some('q');
        let (next, handled) = match modal {
            Modal::Help { scroll } => {
                if close || key.ch() == Some('?') || key.code == KeyCode::Enter {
                    (None, Handled::Redraw)
                } else if key.code == KeyCode::Down || key.ch() == Some('j') {
                    (Some(Modal::Help { scroll: scroll + 1 }), Handled::Redraw)
                } else if key.code == KeyCode::Up || key.ch() == Some('k') {
                    (
                        Some(Modal::Help {
                            scroll: scroll.saturating_sub(1),
                        }),
                        Handled::Redraw,
                    )
                } else {
                    (Some(Modal::Help { scroll }), Handled::Ignored)
                }
            }
            pending @ Modal::PlanPending { .. } => {
                if close {
                    (None, Handled::Redraw)
                } else {
                    (Some(pending), Handled::Ignored)
                }
            }
            Modal::PlanPreview {
                plan,
                paths,
                action,
                cursor,
                excluded,
            } => {
                if close || key.ch() == Some('n') {
                    (None, Handled::Redraw)
                } else {
                    return self
                        .handle_plan_preview_key(key, plan, paths, action, cursor, excluded);
                }
            }
            Modal::Execution(mut exec) => {
                if exec.report.is_some() {
                    if close || key.code == KeyCode::Enter {
                        self.modal = None;
                        self.after_modal_closed();
                        return Handled::Redraw;
                    }
                    (Some(Modal::Execution(exec)), Handled::Ignored)
                } else if (close || key.ch() == Some('c')) && !exec.cancel_requested {
                    exec.cancel_requested = true;
                    self.backend.cancel_execution();
                    (Some(Modal::Execution(exec)), Handled::Redraw)
                } else {
                    (Some(Modal::Execution(exec)), Handled::Ignored)
                }
            }
        };
        self.modal = next;
        if self.modal.is_none() {
            self.after_modal_closed();
        }
        handled
    }

    #[allow(clippy::too_many_arguments)] // the modal's state, taken apart and put back
    fn handle_plan_preview_key(
        &mut self,
        key: &Key,
        plan: CleanPlan,
        paths: Vec<PathBuf>,
        action: ActionOverride,
        cursor: usize,
        mut excluded: BTreeSet<PathBuf>,
    ) -> Handled {
        let page = self.viewport_rows.saturating_sub(14).max(1);
        let last = plan.items.len().saturating_sub(1);
        let mut cursor = cursor.min(last);
        match (key.code, key.ch()) {
            (KeyCode::Enter, _) | (_, Some('y')) => {
                self.start_execution(included(plan, &excluded), false);
                return Handled::Redraw;
            }
            (_, Some('t')) => {
                self.start_execution(included(plan, &excluded), true);
                return Handled::Redraw;
            }
            (_, Some('a')) => {
                let next = match action {
                    ActionOverride::RuleDefault => ActionOverride::Force(ActionSpec::Trash),
                    ActionOverride::Force(ActionSpec::Trash) => {
                        ActionOverride::Force(ActionSpec::Delete)
                    }
                    ActionOverride::Force(_) => ActionOverride::RuleDefault,
                };
                self.request_plan_excluding(paths, next, excluded);
                return Handled::Redraw;
            }
            (KeyCode::Space, _) => {
                if let Some(item) = plan.items.get(cursor)
                    && item.blocked.is_none()
                    && !excluded.remove(&item.path)
                {
                    excluded.insert(item.path.clone());
                }
                cursor = (cursor + 1).min(last);
            }
            (_, Some('A')) => {
                let actionable = plan.items.iter().filter(|i| i.blocked.is_none());
                if excluded.is_empty() {
                    excluded = actionable.map(|i| i.path.clone()).collect();
                } else {
                    excluded.clear();
                }
            }
            (KeyCode::Up, _) | (_, Some('k')) => cursor = cursor.saturating_sub(1),
            (KeyCode::Down, _) | (_, Some('j')) => cursor = (cursor + 1).min(last),
            (KeyCode::PageUp, _) => cursor = cursor.saturating_sub(page),
            (KeyCode::PageDown, _) => cursor = (cursor + page).min(last),
            (KeyCode::Home, _) | (_, Some('g')) => cursor = 0,
            (KeyCode::End, _) | (_, Some('G')) => cursor = last,
            _ => {
                self.modal = Some(Modal::PlanPreview {
                    plan,
                    paths,
                    action,
                    cursor,
                    excluded,
                });
                return Handled::Ignored;
            }
        }
        self.modal = Some(Modal::PlanPreview {
            plan,
            paths,
            action,
            cursor,
            excluded,
        });
        Handled::Redraw
    }

    fn start_execution(&mut self, plan: CleanPlan, dry_run: bool) {
        if plan.actionable() == 0 {
            self.modal = None;
            self.info("plan has nothing to do", MessageLevel::Warning);
            self.after_modal_closed();
            return;
        }
        self.modal = Some(Modal::Execution(ExecState::new(plan.clone(), dry_run)));
        self.backend.execute_plan(plan, dry_run);
    }

    fn after_modal_closed(&mut self) {
        if let Some(tree) = self.pending_tree.take() {
            self.replace_tree(tree);
        }
        if self.message.as_ref().map(|m| m.1) == Some(MessageLevel::Info) {
            self.message = None;
        }
    }

    fn request_visible_classifications(&mut self) {
        if self.view != View::Browser || self.lens.is_flat() {
            return;
        }
        let listing = self.listing();
        let Some(tree) = self.tree.clone() else {
            return;
        };
        let range = self.rows.visible(listing.len(), self.viewport_rows);
        let wanted: Vec<PathBuf> = listing[range]
            .iter()
            .filter(|id| self.classes.get(**id).is_none())
            .map(|id| tree.path(*id))
            .filter(|p| !self.requested.contains(p))
            .collect();
        if !wanted.is_empty() {
            self.requested.extend(wanted.iter().cloned());
            self.backend.request_classifications(wanted);
        }
    }
}

/// `plan` without the `excluded` items.
fn included(mut plan: CleanPlan, excluded: &BTreeSet<PathBuf>) -> CleanPlan {
    plan.items.retain(|item| !excluded.contains(&item.path));
    plan
}

/// Normalized key: bmux reports letters lowercase with `shift`; this restores the character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    code: KeyCode,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl From<KeyStroke> for Key {
    fn from(stroke: KeyStroke) -> Self {
        Self {
            code: stroke.key,
            ctrl: stroke.modifiers.ctrl,
            alt: stroke.modifiers.alt,
            shift: stroke.modifiers.shift,
        }
    }
}

impl Key {
    const fn ch(&self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if self.shift && c.is_ascii_lowercase() => {
                Some(c.to_ascii_uppercase())
            }
            KeyCode::Char(c) if !self.ctrl && !self.alt => Some(c),
            KeyCode::Space if !self.ctrl && !self.alt => Some(' '),
            _ => None,
        }
    }
}
