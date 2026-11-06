//! Drawing. Pure functions of [`App`] state onto a bmux [`PaintCx`].
//!
//! Only the visible rows of the current listing are ever touched; listings themselves come from
//! the [`crate::listing::ListingCache`].

use std::cell::{Cell, RefCell};

use bclean_model::Severity;
use bclean_tree::{EntryKind, NodeFlags, NodeId, Tree};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui::text_width::{display_width, truncate_to_display_width};
use bmux_tui_components::breadcrumbs::{
    BreadcrumbItem, BreadcrumbsComponent, BreadcrumbsPolicy, BreadcrumbsState,
};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBarComponent};
use bmux_tui_components::labeled_details::{DetailItem, LabeledDetailsComponent};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent, ModalSizing, ModalTheme};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState};
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarStyles, ProgressBarValue,
};
use bmux_tui_components::status_bar::{StatusBarComponent, StatusSegment, StatusSeverity};
use bmux_tui_components::tab_bar::{TabBarComponent, TabBarPolicy, TabBarState, TabItem};

use crate::app::{App, ExecState, MessageLevel, Modal, View};
use crate::backend::{ActionOverride, Backend, BackendMode, CleanPlan, ItemOutcome};
use crate::format;
use crate::listing::{Lens, SortBy};
use crate::widgets::{UsageGauge, paint_component, paint_fixed_rows};

const ACCENT: Color = Color::Cyan;
const MUTED: Style = Style::new().fg(Color::BrightBlack);
const SELECTED: Style = Style::new().bg(Color::Blue).fg(Color::BrightWhite);
/// Minimum terminal width at which the explain panel docks to the right instead of a modal.
const DOCK_EXPLAIN_WIDTH: u16 = 120;

const fn rect_local(rect: Rect) -> LocalRect {
    LocalRect::terminal(rect)
}

fn write(cx: &mut PaintCx<'_, '_>, rect: Rect, line: &Line) {
    cx.write_line(rect_local(rect), line);
}

fn styled(text: impl Into<String>, style: Style) -> Line {
    Line::from_spans(vec![Span::styled(text.into(), style)])
}

const fn row(area: Rect, y: u16) -> Rect {
    Rect::new(area.x, area.y.saturating_add(y), area.width, 1)
}

/// Renders the whole UI.
pub fn render<B: Backend>(app: &mut App<B>, cx: &mut PaintCx<'_, '_>) {
    let full = cx.clip();
    cx.fill(rect_local(full), " ", Style::new());
    if full.width < 20 || full.height < 6 {
        write(cx, row(full, 0), &Line::raw("terminal too small"));
        return;
    }
    let body = Rect::new(full.x, full.y + 2, full.width, full.height - 4);
    render_header(app, cx, row(full, 0), row(full, 1));
    match app.view {
        View::Dashboard => render_dashboard(app, cx, body),
        View::Browser if app.tree.is_none() => render_progress(app, cx, body),
        View::Browser => render_browser(app, cx, body),
    }
    render_status(app, cx, row(full, full.height - 2));
    render_hints(app, cx, row(full, full.height - 1));
    if app.explain_open && app.view == View::Browser && full.width < DOCK_EXPLAIN_WIDTH {
        render_explain_modal(app, cx, full);
    }
    if let Some(modal) = app.modal.clone() {
        render_modal(app, &modal, cx, full);
    }
}

fn render_header<B: Backend>(app: &App<B>, cx: &mut PaintCx<'_, '_>, top: Rect, tabs: Rect) {
    let title = format!(" {} ", app.options.title);
    let title_width = u16::try_from(display_width(&title)).unwrap_or(0);
    write(
        cx,
        top,
        &Line::from_spans(vec![Span::styled(
            title,
            Style::new()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )]),
    );
    let crumbs_area = Rect::new(
        top.x + title_width + 1,
        top.y,
        top.width.saturating_sub(title_width + 1),
        1,
    );
    match (app.view, app.tree.clone()) {
        (View::Dashboard, _) => write(cx, crumbs_area, &Line::raw("Dashboard")),
        (View::Browser, Some(tree)) if !app.lens.is_flat() => {
            render_breadcrumbs(&tree, app.cwd, cx, crumbs_area);
        }
        (View::Browser, Some(tree)) => write(
            cx,
            crumbs_area,
            &Line::from_spans(vec![
                Span::styled(format!("{} ", app.lens.label()), Style::new().fg(ACCENT)),
                Span::styled(format!("across {}", tree.root_path().display()), MUTED),
            ]),
        ),
        (View::Browser, None) => write(cx, crumbs_area, &Line::raw("waiting for scan data…")),
    }

    if app.view != View::Browser {
        return;
    }
    let items: Vec<TabItem<'_>> = Lens::ALL
        .iter()
        .map(|l| TabItem::new(l.id(), l.label()).disabled(!l.available()))
        .collect();
    let selected = Lens::ALL.iter().position(|l| *l == app.lens);
    let state = RefCell::new(TabBarState::new(selected));
    let info = format!(
        "sort:{}{}  {}  {}{}",
        app.sort.by.label(),
        if app.sort.reverse { "↑" } else { "↓" },
        if app.apparent { "apparent" } else { "disk" },
        if app.show_hidden { "" } else { "no-hidden  " },
        if app.filter.is_empty() {
            String::new()
        } else {
            format!("filter:{}", app.filter)
        },
    );
    let info_width = u16::try_from(display_width(&info))
        .unwrap_or(0)
        .min(tabs.width / 2);
    let tabs_area = Rect::new(tabs.x, tabs.y, tabs.width - info_width - 1, 1);
    paint_component(
        &TabBarComponent::new("lenses", &items, &state).policy(TabBarPolicy::bare()),
        tabs_area,
        cx,
    );
    let info_area = Rect::new(tabs.right() - info_width, tabs.y, info_width, 1);
    write(
        cx,
        info_area,
        &styled(
            truncate_to_display_width(&info, usize::from(info_width)),
            MUTED,
        ),
    );
}

fn render_breadcrumbs(tree: &Tree, cwd: NodeId, cx: &mut PaintCx<'_, '_>, area: Rect) {
    let mut chain = tree.ancestors(cwd);
    chain.reverse();
    chain.push(cwd);
    let labels: Vec<(String, String)> = chain
        .iter()
        .map(|id| {
            let label = if *id == NodeId::ROOT {
                tree.root_path().display().to_string()
            } else {
                tree.name(*id).to_string_lossy().into_owned()
            };
            (id.raw().to_string(), label)
        })
        .collect();
    // Keep the deepest crumbs when the path is too long.
    let mut start = 0;
    let total = |s: usize| -> usize {
        labels[s..]
            .iter()
            .map(|(_, l)| display_width(l) + 3)
            .sum::<usize>()
    };
    while start + 1 < labels.len() && total(start) > usize::from(area.width) {
        start += 1;
    }
    let items: Vec<BreadcrumbItem<'_>> = labels[start..]
        .iter()
        .map(|(id, label)| BreadcrumbItem::new(id, label))
        .collect();
    let state = Cell::new(BreadcrumbsState::new(Some(items.len().saturating_sub(1))));
    paint_component(
        &BreadcrumbsComponent::new("path", &items, &state).policy(BreadcrumbsPolicy::bare()),
        area,
        cx,
    );
}

/// Which optional columns fit.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // one flag per optional column
struct Columns {
    bar: bool,
    pct: bool,
    items: bool,
    mtime: bool,
    class: bool,
}

impl Columns {
    const MARK: u16 = 2;
    const SIZE: u16 = 11;
    const BAR: u16 = 12;
    const PCT: u16 = 7;
    const ITEMS: u16 = 9;
    const MTIME: u16 = 11;
    const CLASS: u16 = 23;

    const fn for_width(width: u16) -> Self {
        let mut cols = Self {
            bar: true,
            pct: true,
            items: true,
            mtime: true,
            class: true,
        };
        let name_min = 16;
        if cols.width() + name_min > width {
            cols.mtime = false;
        }
        if cols.width() + name_min > width {
            cols.items = false;
        }
        if cols.width() + name_min > width {
            cols.pct = false;
        }
        if cols.width() + name_min > width {
            cols.class = false;
        }
        if cols.width() + name_min > width {
            cols.bar = false;
        }
        cols
    }

    const fn width(self) -> u16 {
        let mut w = Self::MARK + Self::SIZE;
        if self.bar {
            w += Self::BAR;
        }
        if self.pct {
            w += Self::PCT;
        }
        if self.items {
            w += Self::ITEMS;
        }
        if self.mtime {
            w += Self::MTIME;
        }
        if self.class {
            w += Self::CLASS;
        }
        w
    }
}

fn render_browser<B: Backend>(app: &mut App<B>, cx: &mut PaintCx<'_, '_>, body: Rect) {
    let docked = app.explain_open && body.width >= DOCK_EXPLAIN_WIDTH;
    let (list_area, explain_area) = if docked {
        let panel = (body.width * 2 / 5).min(60);
        (
            Rect::new(body.x, body.y, body.width - panel, body.height),
            Some(Rect::new(body.right() - panel, body.y, panel, body.height)),
        )
    } else {
        (body, None)
    };
    let header = row(list_area, 0);
    let rows_area = Rect::new(
        list_area.x,
        list_area.y + 1,
        list_area.width,
        list_area.height.saturating_sub(1),
    );
    app.viewport_rows = usize::from(rows_area.height.max(1));
    let listing = app.listing();
    app.rows.clamp(listing.len(), app.viewport_rows);
    let Some(tree) = app.tree.clone() else {
        return;
    };
    let cols = Columns::for_width(rows_area.width.saturating_sub(1));
    render_column_header(app, cx, header, cols);

    if listing.is_empty() {
        let text = if app.lens.is_flat() {
            if app.classes.is_empty() {
                "no classifications yet"
            } else {
                "nothing matches this lens"
            }
        } else if app.filter.is_empty() {
            "empty directory"
        } else {
            "no entries match the filter"
        };
        write(cx, row(rows_area, 1), &styled(format!("  {text}"), MUTED));
    } else {
        let mode = app.size_mode();
        let parent_total = if app.lens.is_flat() {
            tree.size(NodeId::ROOT, mode)
        } else {
            tree.size(app.cwd, mode)
        };
        let visible = app.rows.visible(listing.len(), app.viewport_rows);
        let max = listing[visible]
            .iter()
            .map(|id| tree.size(*id, mode))
            .max()
            .unwrap_or(0)
            .max(1);
        let ctx = RowCtx {
            tree: &tree,
            app,
            cols,
            parent_total,
            max,
        };
        paint_fixed_rows(
            cx,
            rows_area,
            listing.len(),
            app.rows,
            |index, area, selected, cx| {
                ctx.paint(listing[index], area, selected, cx);
            },
        );
    }

    if let Some(area) = explain_area {
        let lines = explain_lines(app, area.width.saturating_sub(4));
        let state = Cell::new(PaneState::new(area));
        let pane = PaneComponent::new(
            "explain",
            Pane::new().title("Explain (i)"),
            &state,
            TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None),
        );
        paint_component(&pane, area, cx);
    }
}

fn render_column_header<B: Backend>(
    app: &App<B>,
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
    cols: Columns,
) {
    let arrow = |by: SortBy| {
        if app.sort.by == by {
            if app.sort.reverse { "↑" } else { "↓" }
        } else {
            ""
        }
    };
    let style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
    let mut parts = vec![format!("  {:>10} ", format!("{}Size", arrow(SortBy::Size)))];
    if cols.bar {
        parts.push(" ".repeat(12));
    }
    if cols.pct {
        parts.push(format!("{:>6} ", "%"));
    }
    if cols.items {
        parts.push(format!("{:>8} ", format!("{}Items", arrow(SortBy::Items))));
    }
    if cols.mtime {
        parts.push(format!(
            "{:<10} ",
            format!("{}Modified", arrow(SortBy::Mtime))
        ));
    }
    if cols.class {
        parts.push(format!(
            "{:<16}{:>6} ",
            "Class",
            format!("{}Score", arrow(SortBy::Score))
        ));
    }
    parts.push(format!("{}Name", arrow(SortBy::Name)));
    let text = parts.concat();
    write(cx, area, &styled(text, style));
}

struct RowCtx<'a, B: Backend> {
    tree: &'a Tree,
    app: &'a App<B>,
    cols: Columns,
    parent_total: u64,
    max: u64,
}

impl<B: Backend> RowCtx<'_, B> {
    #[allow(clippy::too_many_lines)] // one row, column by column
    fn paint(&self, id: NodeId, area: Rect, selected: bool, cx: &mut PaintCx<'_, '_>) {
        let tree = self.tree;
        let app = self.app;
        let mode = app.size_mode();
        let info = tree.info(id);
        let size = tree.size(id, mode);
        let base = if selected { SELECTED } else { Style::new() };
        cx.fill(rect_local(area), " ", base);

        let path = tree.path(id);
        let marked = app.marked.contains(&path);
        let inherited = !marked && app.marked.iter().any(|m| path.starts_with(m));
        let mut x = area.x;
        let put = |cx: &mut PaintCx<'_, '_>, x: &mut u16, width: u16, spans: Vec<Span>| {
            let w = width.min(area.right().saturating_sub(*x));
            write(cx, Rect::new(*x, area.y, w, 1), &Line::from_spans(spans));
            *x = x.saturating_add(width);
        };
        let mark = if marked {
            Span::styled(
                "● ",
                Style::new()
                    .fg(Color::BrightYellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else if inherited {
            Span::styled("◦ ", Style::new().fg(Color::Yellow))
        } else {
            Span::raw("  ")
        };
        put(cx, &mut x, Columns::MARK, vec![mark]);
        put(
            cx,
            &mut x,
            Columns::SIZE,
            vec![Span::styled(
                format!("{:>10} ", format::size(size)),
                Style::new().add_modifier(Modifier::BOLD),
            )],
        );
        if self.cols.bar {
            let bar_area = Rect::new(x + 1, area.y, Columns::BAR - 2, 1);
            let styles = ProgressBarStyles {
                filled: Style::new().fg(if selected {
                    Color::BrightWhite
                } else {
                    Color::Green
                }),
                empty: Style::new().fg(Color::BrightBlack),
                complete: Style::new().fg(if selected {
                    Color::BrightWhite
                } else {
                    Color::Green
                }),
                ..ProgressBarStyles::default()
            };
            paint_component(
                &ProgressBarComponent::new("size-bar", ProgressBarValue::ratio(size, self.max))
                    .policy(ProgressBarPolicy::bare().symbols("█", " ", "▌"))
                    .styles(styles),
                bar_area,
                cx,
            );
            x = x.saturating_add(Columns::BAR);
        }
        if self.cols.pct {
            put(
                cx,
                &mut x,
                Columns::PCT,
                vec![Span::styled(
                    format!("{} ", format::percent(size, self.parent_total)),
                    MUTED,
                )],
            );
        }
        if self.cols.items {
            put(
                cx,
                &mut x,
                Columns::ITEMS,
                vec![Span::raw(format!("{:>8} ", format::count(info.items)))],
            );
        }
        if self.cols.mtime {
            put(
                cx,
                &mut x,
                Columns::MTIME,
                vec![Span::styled(
                    format!("{:<10} ", format::date(info.max_mtime)),
                    MUTED,
                )],
            );
        }
        if self.cols.class {
            put(cx, &mut x, Columns::CLASS, self.class_spans(id));
        }
        let name_width = area.right().saturating_sub(x);
        put(
            cx,
            &mut x,
            name_width,
            self.name_spans(id, usize::from(name_width)),
        );
    }

    fn class_spans(&self, id: NodeId) -> Vec<Span> {
        if let Some(class) = self.app.classes.get(id) {
            let badge = truncate_to_display_width(&format::badge(class), 15);
            return vec![
                Span::styled(
                    format!("{badge:<16}"),
                    format::quadrant_style(class.quadrant()),
                ),
                Span::styled(
                    format!("{:>6} ", format::score(class.score)),
                    Style::new().fg(Color::BrightCyan),
                ),
            ];
        }
        let inside = self.app.classes.reclaimable_inside(id);
        if inside > 0 {
            vec![Span::styled(
                format!("{:<22} ", format!("◦ {} inside", format::size(inside))),
                Style::new().fg(Color::Green).add_modifier(Modifier::DIM),
            )]
        } else {
            vec![Span::raw(" ".repeat(23))]
        }
    }

    fn name_spans(&self, id: NodeId, width: usize) -> Vec<Span> {
        let tree = self.tree;
        let flags = tree.flags(id);
        let mut spans = Vec::new();
        if flags.contains(NodeFlags::ERROR) {
            spans.push(Span::styled(
                "! ",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        let is_dir = tree.kind(id) == EntryKind::Dir;
        let mut name = if self.app.lens.is_flat() {
            tree.relative_path(id).display().to_string()
        } else {
            tree.name(id).to_string_lossy().into_owned()
        };
        if is_dir {
            name.push('/');
        }
        let mut suffix = String::new();
        if flags.contains(NodeFlags::SUMMARIZED) && !tree.has_children(id) {
            suffix.push_str(if self.app.expanding.contains(&tree.path(id)) {
                " (expanding…)"
            } else {
                " (summarized)"
            });
        }
        if flags.contains(NodeFlags::OTHER_FS) {
            suffix.push_str(" (other fs)");
        }
        if tree.kind(id) == EntryKind::Symlink {
            suffix.push_str(" →");
        }
        let avail = width.saturating_sub(display_width(&suffix) + spans.len() * 2);
        let name = if self.app.lens.is_flat() {
            format::truncate_left(&name, avail)
        } else {
            truncate_to_display_width(&name, avail)
        };
        let style = if is_dir {
            Style::new()
                .fg(Color::BrightBlue)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        spans.push(Span::styled(name, style));
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix, MUTED));
        }
        spans
    }
}

#[allow(clippy::too_many_lines)] // a flat list of detail rows
fn explain_lines<B: Backend>(app: &mut App<B>, width: u16) -> Vec<Line> {
    let Some(id) = app.selected() else {
        return vec![styled("nothing selected", MUTED)];
    };
    let Some(tree) = app.tree.clone() else {
        return Vec::new();
    };
    let info = tree.info(id);
    let mut items = vec![
        DetailItem::new("Path", tree.path(id).display().to_string()),
        DetailItem::new(
            "Size",
            format!(
                "{} on disk · {} apparent · {} items",
                format::size(info.total_allocated),
                format::size(info.total_apparent),
                format::count(info.items)
            ),
        ),
        DetailItem::new("Modified", format::datetime(info.max_mtime)),
    ];
    let class = app.classes.get(id).cloned();
    let mut extra = Vec::new();
    match &class {
        None => {
            let inside = app.classes.reclaimable_inside(id);
            items.push(DetailItem::new(
                "Classification",
                if inside > 0 {
                    format!("none (contains {} reclaimable)", format::size(inside))
                } else {
                    "none".to_owned()
                },
            ));
        }
        Some(class) => {
            items.push(DetailItem::new(
                "Rule",
                format!(
                    "{}{}",
                    class.rule_id.as_deref().unwrap_or("-"),
                    class
                        .category
                        .as_ref()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default()
                ),
            ));
            items.push(DetailItem::new(
                "Quadrant",
                format::quadrant_label(class.quadrant()),
            ));
            if let Some(eph) = &class.ephemeral {
                items.push(DetailItem::new(
                    "Ephemeral",
                    format!(
                        "{} · regen cost {:?}",
                        format::kind_label(eph.kind),
                        eph.regen_cost
                    ),
                ));
            }
            let st = &class.staleness;
            items.push(DetailItem::new(
                "Staleness",
                format!(
                    "{} · idle {} · threshold {}",
                    if st.stale { "stale" } else { "fresh" },
                    st.idle_secs.map_or_else(|| "?".to_owned(), format::age),
                    format::age(st.threshold_secs)
                ),
            ));
            items.push(DetailItem::new(
                "Score",
                format!(
                    "{} · confidence {:?} · reclaim {}",
                    format::score(class.score),
                    class.confidence,
                    format::size(class.size)
                ),
            ));
            items.push(DetailItem::new(
                "Action",
                format!(
                    "{}{}",
                    format::action(&class.action),
                    if class.auto { " (auto)" } else { "" }
                ),
            ));
            if !st.signals.is_empty() {
                extra.push(styled(
                    "Signals",
                    Style::new()
                        .fg(Color::BrightBlack)
                        .add_modifier(Modifier::BOLD),
                ));
                for s in &st.signals {
                    extra.push(Line::from_spans(vec![
                        Span::styled(format!("  {:?}", s.kind), Style::new().fg(ACCENT)),
                        Span::raw(format!(
                            " {}{} {}",
                            s.last_activity.map_or_else(|| "-".to_owned(), format::date),
                            if s.forces_stale {
                                " (forces stale)"
                            } else {
                                ""
                            },
                            s.detail
                        )),
                    ]));
                }
            }
            if !class.reasons.is_empty() {
                extra.push(styled(
                    "Reasons",
                    Style::new()
                        .fg(Color::BrightBlack)
                        .add_modifier(Modifier::BOLD),
                ));
                for r in &class.reasons {
                    extra.push(Line::from_spans(vec![
                        Span::styled(format!("  {} ", r.code), Style::new().fg(ACCENT)),
                        Span::raw(r.message.clone()),
                    ]));
                }
            }
        }
    }
    let mut lines = LabeledDetailsComponent::new("explain.details", &items)
        .item_spacing(false)
        .lines(width.max(10));
    lines.extend(extra);
    lines
}

fn modal_frame(title: &str, max: Size) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(30, 6), max, Insets::all(1)),
        ModalTheme::dark(ACCENT),
    )
    .title(format!(" {title} "))
}

fn paint_text_modal(
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
    frame: ModalFrame,
    lines: Vec<Line>,
    scroll: usize,
) {
    let block = TextBlock::new(Text::from_lines(lines))
        .wrap(TextWrap::None)
        .vertical_scroll(scroll);
    paint_component(&ModalFrameComponent::new("modal", frame, block), area, cx);
}

fn render_explain_modal<B: Backend>(app: &mut App<B>, cx: &mut PaintCx<'_, '_>, full: Rect) {
    let width = full.width.saturating_sub(4).min(90);
    let lines = explain_lines(app, width.saturating_sub(6));
    let frame = modal_frame(
        "Explain — i/esc to close",
        Size::new(width, full.height.saturating_sub(2)),
    );
    paint_text_modal(cx, full, frame, lines, 0);
}

fn render_modal<B: Backend>(app: &App<B>, modal: &Modal, cx: &mut PaintCx<'_, '_>, full: Rect) {
    match modal {
        Modal::Help { scroll } => {
            let frame = modal_frame(
                "Keys — j/k scroll, esc to close",
                Size::new(76, full.height.saturating_sub(2)),
            );
            let lines = help_lines();
            let scroll = (*scroll).min(lines.len().saturating_sub(1));
            paint_text_modal(cx, full, frame, lines, scroll);
        }
        Modal::PlanPending { paths, .. } => {
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            #[allow(clippy::cast_possible_truncation)] // modulo keeps it tiny
            let frame_char = spinner[(app.tick % spinner.len() as u64) as usize];
            let lines = vec![
                Line::raw(format!(
                    "{frame_char} building clean plan for {} item(s)…",
                    paths.len()
                )),
                Line::default(),
                styled("esc to cancel", MUTED),
            ];
            paint_text_modal(cx, full, modal_frame("Plan", Size::new(60, 7)), lines, 0);
        }
        Modal::PlanPreview {
            plan,
            action,
            scroll,
            ..
        } => {
            let frame = modal_frame(
                &format!(
                    "Clean plan — {} item(s), est. {}",
                    plan.actionable(),
                    format::size(plan.estimated_bytes())
                ),
                Size::new(
                    full.width.saturating_sub(4).min(110),
                    full.height.saturating_sub(2),
                ),
            );
            let width = usize::from(full.width.saturating_sub(10).min(104));
            paint_text_modal(cx, full, frame, plan_lines(plan, action, width), *scroll);
        }
        Modal::Execution(exec) => render_execution(exec, cx, full),
    }
}

fn plan_lines(plan: &CleanPlan, action: &ActionOverride, width: usize) -> Vec<Line> {
    let mut lines = vec![
        Line::from_spans(vec![
            Span::styled("action: ", MUTED),
            Span::styled(
                match action {
                    ActionOverride::RuleDefault => "per-rule default".to_owned(),
                    ActionOverride::Force(spec) => format!("force {}", format::action(spec)),
                },
                Style::new()
                    .fg(Color::BrightYellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("   (a to change)", MUTED),
        ]),
        Line::from_spans(vec![
            Span::styled("enter/y", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" execute   ", MUTED),
            Span::styled("t", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" dry run   ", MUTED),
            Span::styled("esc/n", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", MUTED),
        ]),
        Line::default(),
    ];
    for item in &plan.items {
        let path_width = width.saturating_sub(36);
        let (style, status) = item.blocked.as_ref().map_or_else(
            || (Style::new(), String::new()),
            |reason| {
                (
                    MUTED.add_modifier(Modifier::CROSSED_OUT),
                    format!("  skip: {reason}"),
                )
            },
        );
        let mut spans = vec![
            Span::styled(
                format!("{:>10}  ", format::size(item.estimated_bytes)),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{:<12} ",
                    truncate_to_display_width(&format::action(&item.action), 12)
                ),
                Style::new().fg(if matches!(item.action, bclean_model::ActionSpec::Delete) {
                    Color::Red
                } else {
                    Color::Yellow
                }),
            ),
            Span::styled(
                format::truncate_left(&item.path.display().to_string(), path_width),
                style,
            ),
        ];
        if !status.is_empty() {
            spans.push(Span::styled(status, Style::new().fg(Color::Yellow)));
        } else if let Some(note) = &item.note {
            spans.push(Span::styled(format!("  {note}"), MUTED));
        }
        lines.push(Line::from_spans(spans));
    }
    if plan.items.is_empty() {
        lines.push(styled("(empty plan)", MUTED));
    }
    lines
}

#[allow(clippy::too_many_lines)] // progress, report, and item log in one modal
fn render_execution(exec: &ExecState, cx: &mut PaintCx<'_, '_>, full: Rect) {
    let title = match (&exec.report, exec.dry_run) {
        (Some(_), true) => "Dry run complete — enter to close",
        (Some(_), false) => "Clean complete — enter to close",
        (None, true) => "Dry run…",
        (None, false) => "Cleaning… (esc to cancel)",
    };
    let frame = modal_frame(
        title,
        Size::new(
            full.width.saturating_sub(4).min(100),
            full.height.saturating_sub(2),
        ),
    );
    let content = frame.content_area(full);
    let width = usize::from(content.width);
    let total = exec.plan.items.len();
    let finished = exec.finished();
    let mut lines = vec![Line::default(), Line::default()];
    if let Some(report) = &exec.report {
        lines.push(Line::from_spans(vec![
            Span::styled("estimated ", MUTED),
            Span::styled(
                format::size(report.estimated_bytes),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled("   measured freed ", MUTED),
            Span::styled(
                report
                    .measured_freed_bytes
                    .map_or_else(|| "unknown".to_owned(), format::size),
                Style::new()
                    .fg(Color::BrightGreen)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::raw(format!(
            "{} done · {} skipped · {} failed{} · {:.1}s",
            report.done,
            report.skipped,
            report.failed,
            if report.cancelled {
                " · cancelled"
            } else {
                ""
            },
            report.elapsed.as_secs_f64()
        )));
        if let Some(journal) = &report.journal {
            lines.push(styled(format!("journal: {journal}"), MUTED));
        }
    } else {
        let current = exec
            .current
            .and_then(|i| exec.plan.items.get(i))
            .map(|item| {
                format::truncate_left(&item.path.display().to_string(), width.saturating_sub(4))
            })
            .unwrap_or_default();
        lines.push(Line::raw(format!(
            "{} of est. {} freed{}",
            format::size(exec.done_estimated_bytes()),
            format::size(exec.plan.estimated_bytes()),
            if exec.cancel_requested {
                " · cancelling…"
            } else {
                ""
            }
        )));
        lines.push(styled(format!("→ {current}"), MUTED));
    }
    lines.push(Line::default());
    // Item log, most recent first.
    for (item, outcome) in exec.plan.items.iter().zip(&exec.outcomes).rev() {
        let Some(outcome) = outcome else { continue };
        let (mark, style, detail) = match outcome {
            ItemOutcome::Done { freed_bytes } => (
                "✓",
                Style::new().fg(Color::Green),
                freed_bytes.map(format::size).unwrap_or_default(),
            ),
            ItemOutcome::Skipped { reason } => {
                ("–", Style::new().fg(Color::Yellow), reason.clone())
            }
            ItemOutcome::Failed { error } => ("✗", Style::new().fg(Color::Red), error.clone()),
        };
        lines.push(Line::from_spans(vec![
            Span::styled(format!("{mark} "), style),
            Span::raw(format::truncate_left(
                &item.path.display().to_string(),
                width.saturating_sub(30),
            )),
            Span::styled(format!("  {detail}"), MUTED),
        ]));
    }
    paint_text_modal(cx, full, frame, lines, 0);
    let gauge_area = Rect::new(content.x, content.y, content.width, 1);
    let label = format!("{finished}/{total} items");
    paint_component(
        &ProgressBarComponent::new(
            "exec",
            ProgressBarValue::determinate(finished as u64, total as u64),
        )
        .label(&label)
        .policy(ProgressBarPolicy::compact()),
        gauge_area,
        cx,
    );
}

fn help_lines() -> Vec<Line> {
    let sections: [(&str, &[(&str, &str)]); 5] = [
        (
            "Navigate",
            &[
                ("↑ ↓ / k j", "move"),
                ("PgUp PgDn / ^u ^d", "page"),
                ("Home End / g G", "top / bottom"),
                ("→ enter / l", "open directory (expands summarized dirs)"),
                ("← backspace / h", "parent directory"),
                ("/", "filter by name (enter keeps, esc clears)"),
            ],
        ),
        (
            "View",
            &[
                (
                    "s n c m o",
                    "sort by size / name / items / mtime / score (again: reverse)",
                ),
                ("a", "toggle disk (allocated) / apparent size"),
                (".", "toggle hidden files"),
                ("tab / 1-4", "lens: all, ephemeral, stale, reclaimable"),
                ("i", "explain selected item"),
                ("D", "dashboard (volumes, alerts, daemon)"),
                ("r", "rescan current directory"),
            ],
        ),
        (
            "Clean",
            &[
                ("space", "mark / unmark (moves down)"),
                ("x", "clear all marks"),
                ("p", "plan for marked items (or the selected one)"),
                ("d", "delete selected (plan preview first)"),
            ],
        ),
        (
            "Plan preview",
            &[
                ("enter / y", "execute"),
                ("t", "dry run"),
                ("a", "cycle action: rule default / trash / delete"),
            ],
        ),
        ("General", &[("?", "this help"), ("q / ^c", "quit")]),
    ];
    let mut lines = Vec::new();
    for (title, keys) in sections {
        lines.push(styled(
            title,
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        for (key, desc) in keys {
            lines.push(Line::from_spans(vec![
                Span::styled(
                    format!("  {key:<20}"),
                    Style::new()
                        .fg(Color::BrightWhite)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled((*desc).to_owned(), Style::new().fg(Color::White)),
            ]));
        }
        lines.push(Line::default());
    }
    lines
}

fn render_progress<B: Backend>(app: &App<B>, cx: &mut PaintCx<'_, '_>, body: Rect) {
    let width = body.width.saturating_sub(4).min(90);
    let area = Rect::new(
        body.x + (body.width - width) / 2,
        body.y + body.height.saturating_sub(12) / 2,
        width,
        body.height.min(12),
    );
    let Some(p) = app.progress.as_ref() else {
        write(cx, row(area, 0), &styled("waiting for the backend…", MUTED));
        return;
    };
    write(
        cx,
        row(area, 0),
        &Line::from_spans(vec![
            Span::styled(
                if p.running { "Scanning " } else { "Scanned " },
                Style::new().add_modifier(Modifier::BOLD),
            ),
            Span::styled(p.root.display().to_string(), Style::new().fg(ACCENT)),
        ]),
    );
    #[allow(clippy::cast_possible_truncation)] // pulse offset wraps
    let value = p.expected_entries.map_or_else(
        || ProgressBarValue::indeterminate((app.tick % u64::from(u16::MAX)) as u16),
        |expected| ProgressBarValue::determinate(p.entries().min(expected), expected),
    );
    paint_component(
        &ProgressBarComponent::new("scan", value).policy(ProgressBarPolicy::compact()),
        row(area, 2),
        cx,
    );
    let secs = p.elapsed.as_secs_f64().max(0.001);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rate = (p.entries() as f64 / secs) as u64;
    let stats = [
        ("files", format::count(p.files)),
        ("directories", format::count(p.dirs)),
        ("disk usage", format::size(p.allocated_bytes)),
        ("apparent size", format::size(p.apparent_bytes)),
        ("errors", format::count(p.errors)),
        (
            "elapsed",
            format!(
                "{:.1}s ({} items/s)",
                p.elapsed.as_secs_f64(),
                format::count(rate)
            ),
        ),
    ];
    for (i, (label, value)) in stats.iter().enumerate() {
        let y = 4 + u16::try_from(i).unwrap_or(0);
        write(
            cx,
            row(area, y),
            &Line::from_spans(vec![
                Span::styled(format!("{label:>14}  "), MUTED),
                Span::styled(value.clone(), Style::new().add_modifier(Modifier::BOLD)),
            ]),
        );
    }
    if let Some(current) = &p.current {
        write(
            cx,
            row(area, 11),
            &styled(
                format::truncate_left(&current.display().to_string(), usize::from(width)),
                MUTED,
            ),
        );
    }
}

#[allow(clippy::too_many_lines)] // three sequential sections
fn render_dashboard<B: Backend>(app: &App<B>, cx: &mut PaintCx<'_, '_>, body: Rect) {
    let mut y = 0;
    let heading = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
    write(cx, row(body, y), &styled("Volumes", heading));
    y += 1;
    if app.volumes.is_empty() {
        write(cx, row(body, y), &styled("  no volume data", MUTED));
        y += 1;
    }
    let label_width = 28u16.min(body.width / 3);
    for volume in &app.volumes {
        if y + 2 >= body.height {
            break;
        }
        let label = format!(
            "  {} ({})",
            truncate_to_display_width(&volume.mount_point, usize::from(label_width) - 8),
            volume.fs_type
        );
        write(
            cx,
            Rect::new(body.x, body.y + y, label_width, 1),
            &Line::raw(label),
        );
        let gauge = UsageGauge::new(volume.used(), volume.total);
        let text = format!(
            "{} / {} ({:.0}%)",
            format::size(volume.used()),
            format::size(volume.total),
            gauge.fraction() * 100.0
        );
        let gauge_width = body.width.saturating_sub(label_width + 32).max(10);
        gauge.paint(
            &text,
            Rect::new(body.x + label_width, body.y + y, gauge_width, 1),
            cx,
        );
        let extra = format!(
            "  avail {}{}",
            format::size(volume.available),
            volume
                .purgeable
                .map(|p| format!(" · purgeable {}", format::size(p)))
                .unwrap_or_default()
        );
        let ex = body.x + label_width + gauge_width;
        write(
            cx,
            Rect::new(ex, body.y + y, body.right().saturating_sub(ex), 1),
            &styled(extra, MUTED),
        );
        y += 1;
    }
    y += 1;

    if let Some(tree) = app.tree.as_deref() {
        let mode = app.size_mode();
        write(cx, row(body, y), &styled("Scan", heading));
        y += 1;
        let recl = app.classes.reclaimable_inside(NodeId::ROOT)
            + app.classes.get(NodeId::ROOT).map_or(0, |c| c.size);
        write(
            cx,
            row(body, y),
            &Line::raw(format!(
                "  {} · {} · {} items · {} classified · {} reclaimable",
                tree.root_path().display(),
                format::size(tree.size(NodeId::ROOT, mode)),
                format::count(tree.info(NodeId::ROOT).items),
                format::count(app.classes.len() as u64),
                format::size(recl),
            )),
        );
        y += 2;
    }

    write(
        cx,
        row(body, y),
        &styled(format!("Alerts ({})", app.alerts.len()), heading),
    );
    y += 1;
    if app.alerts.is_empty() {
        write(cx, row(body, y), &styled("  no active alerts", MUTED));
        y += 1;
    }
    for alert in &app.alerts {
        if y + 1 >= body.height {
            break;
        }
        let (tag, style) = match alert.severity {
            Severity::Info => ("INFO", Style::new().fg(Color::Cyan)),
            Severity::Warning => (
                "WARN",
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Severity::Critical => (
                "CRIT",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        };
        write(
            cx,
            row(body, y),
            &Line::from_spans(vec![
                Span::styled(format!("  {tag} "), style),
                Span::styled(
                    alert.title.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  {}", alert.body, format::datetime(alert.at)),
                    MUTED,
                ),
            ]),
        );
        y += 1;
    }
    y += 1;
    if y < body.height {
        let line = app.status.as_ref().map_or_else(
            || styled("Backend: unknown", MUTED),
            |s| {
                let (state, style) = if s.connected {
                    ("connected", Style::new().fg(Color::Green))
                } else {
                    (
                        "disconnected",
                        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )
                };
                Line::from_spans(vec![
                    Span::styled("Backend: ", heading),
                    Span::raw(match s.mode {
                        BackendMode::Local => "local ",
                        BackendMode::Daemon => "daemon ",
                    }),
                    Span::styled(state, style),
                    Span::styled(
                        format!(
                            "{}{}",
                            s.version
                                .as_ref()
                                .map(|v| format!(" · v{v}"))
                                .unwrap_or_default(),
                            s.detail
                                .as_ref()
                                .map(|d| format!(" · {d}"))
                                .unwrap_or_default()
                        ),
                        MUTED,
                    ),
                ])
            },
        );
        write(cx, row(body, y), &line);
    }
}

fn render_status<B: Backend>(app: &App<B>, cx: &mut PaintCx<'_, '_>, area: Rect) {
    let mode = app.size_mode();
    let left_text = app.tree.as_deref().map_or_else(String::new, |tree| {
        let dir = if app.lens.is_flat() {
            NodeId::ROOT
        } else {
            app.cwd
        };
        format!(
            "{} in {} items",
            format::size(tree.size(dir, mode)),
            format::count(tree.info(dir).items)
        )
    });
    let marked_text = (!app.marked.is_empty()).then(|| {
        format!(
            "{} marked ({})",
            app.marked.len(),
            format::size(app.marked_bytes())
        )
    });
    let scan_text = app
        .progress
        .as_ref()
        .filter(|p| p.running)
        .map(|p| format!("scanning… {} items", format::count(p.entries())));
    let center_text = if app.filter_editing {
        Some((format!("filter: {}▏", app.filter), StatusSeverity::Info))
    } else {
        app.message.as_ref().map(|(text, level)| {
            (
                text.clone(),
                match level {
                    MessageLevel::Info => StatusSeverity::Info,
                    MessageLevel::Success => StatusSeverity::Success,
                    MessageLevel::Warning => StatusSeverity::Warning,
                    MessageLevel::Error => StatusSeverity::Error,
                },
            )
        })
    };
    let mut left = vec![StatusSegment::new(&left_text)];
    if let Some(text) = &marked_text {
        left.push(StatusSegment::new(text).severity(StatusSeverity::Warning));
    }
    let center: Vec<StatusSegment<'_>> = center_text
        .as_ref()
        .map(|(text, sev)| StatusSegment::new(text).severity(*sev))
        .into_iter()
        .collect();
    let mut right = Vec::new();
    if let Some(text) = &scan_text {
        right.push(StatusSegment::new(text).severity(StatusSeverity::Info));
    }
    let alerts_text = format!(
        "{} alert{}",
        app.alerts.len(),
        if app.alerts.len() == 1 { "" } else { "s" }
    );
    if !app.alerts.is_empty() {
        right.push(StatusSegment::new(&alerts_text).severity(StatusSeverity::Warning));
    }
    let mode_text = app
        .status
        .as_ref()
        .map_or("", |s| match (s.mode, s.connected) {
            (BackendMode::Local, _) => "local",
            (BackendMode::Daemon, true) => "daemon",
            (BackendMode::Daemon, false) => "daemon (offline)",
        });
    if !mode_text.is_empty() {
        right.push(StatusSegment::new(mode_text).severity(StatusSeverity::Muted));
    }
    cx.fill(rect_local(area), " ", Style::new().bg(Color::Indexed(236)));
    paint_component(
        &StatusBarComponent::new("status")
            .left(&left)
            .center(&center)
            .right(&right),
        area,
        cx,
    );
}

fn render_hints<B: Backend>(app: &App<B>, cx: &mut PaintCx<'_, '_>, area: Rect) {
    const BROWSER: &[KeyHint<'static>] = &[
        KeyHint::new("↑↓", "move"),
        KeyHint::new("→", "open"),
        KeyHint::new("←", "up"),
        KeyHint::new("space", "mark"),
        KeyHint::new("p", "plan"),
        KeyHint::new("d", "delete"),
        KeyHint::new("i", "explain"),
        KeyHint::new("tab", "lens"),
        KeyHint::new("s/n/c/m/o", "sort"),
        KeyHint::new("/", "filter"),
        KeyHint::new("D", "dashboard"),
        KeyHint::new("?", "help"),
        KeyHint::new("q", "quit"),
    ];
    const DASHBOARD: &[KeyHint<'static>] = &[
        KeyHint::new("r", "refresh"),
        KeyHint::new("esc/D", "browser"),
        KeyHint::new("?", "help"),
        KeyHint::new("q", "quit"),
    ];
    const FILTER: &[KeyHint<'static>] = &[
        KeyHint::new("type", "filter"),
        KeyHint::new("enter", "keep"),
        KeyHint::new("esc", "clear"),
        KeyHint::new("↑↓", "move"),
    ];
    let hints = if app.filter_editing {
        FILTER
    } else if app.view == View::Dashboard {
        DASHBOARD
    } else {
        BROWSER
    };
    paint_component(&KeyHintBarComponent::new("hints", hints), area, cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_degrade_with_width() {
        let wide = Columns::for_width(200);
        assert!(wide.mtime && wide.items && wide.class && wide.bar);
        let narrow = Columns::for_width(40);
        assert!(!narrow.mtime && !narrow.items);
        assert!(Columns::for_width(10).width() >= Columns::MARK + Columns::SIZE);
    }
}
