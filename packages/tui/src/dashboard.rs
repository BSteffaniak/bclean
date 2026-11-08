//! The dashboard: disk usage, reclaimable data, and where it lives, from the backend's
//! [`DashboardSummary`].
//!
//! Panels with rows (by kind, biggest wins, projects, growth) take focus and drill down into
//! the browser; the rest are read-only. Wide terminals lay panels out in a grid, narrow ones
//! stack them in priority order and drop the least important when space runs out.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use bclean_model::{DashboardSummary, VolumeSummary, kind_label};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span, Text};
use bmux_tui::text_width::{display_width, truncate_to_display_width};
use bmux_tui_components::pane::{Pane, PaneComponent, PaneState, PaneStyles};
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarStyles, ProgressBarValue,
};
use bmux_tui_components::sparkline::{SparklineComponent, SparklinePolicy, SparklineStyles};

use crate::app::App;
use crate::backend::Backend;
use crate::format;
use crate::render::{ACCENT, MUTED, SELECTED, row, styled, write};
use crate::widgets::UsageGauge;

/// Width at which panels sit side by side.
const WIDE: u16 = 110;
/// Composition colors: reclaimable, other indexed data, outside the index, free.
const RECLAIMABLE: Color = Color::Yellow;
const INDEXED: Color = Color::Blue;
const OUTSIDE: Color = Color::Magenta;
const OTHER_VOLUMES: Color = Color::Cyan;
const ACTIVE: Color = Color::Green;
const FREE: Color = Color::BrightBlack;
const SAFE: Style = Style::new().fg(Color::Green);
const REVIEW: Style = Style::new().fg(Color::Yellow);

/// Dashboard panels that take focus, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashPanel {
    /// Reclaimable bytes by kind or category.
    #[default]
    Kinds,
    /// Best cleanup candidates.
    Wins,
    /// Projects holding the most reclaimable data.
    Projects,
    /// Fastest-growing directories.
    Growth,
}

impl DashPanel {
    /// All focusable panels in tab order.
    pub const ALL: [Self; 4] = [Self::Kinds, Self::Wins, Self::Projects, Self::Growth];

    const fn index(self) -> usize {
        match self {
            Self::Kinds => 0,
            Self::Wins => 1,
            Self::Projects => 2,
            Self::Growth => 3,
        }
    }

    /// Next (or previous) panel.
    #[must_use]
    pub const fn cycle(self, backwards: bool) -> Self {
        let n = Self::ALL.len();
        let i = if backwards {
            (self.index() + n - 1) % n
        } else {
            (self.index() + 1) % n
        };
        Self::ALL[i]
    }
}

/// What the kinds panel groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashGroup {
    /// Ephemeral kind (build output, dependencies, caches, ...).
    #[default]
    Kind,
    /// Rule category / ecosystem (rust, node, xcode, ...).
    Category,
}

/// Dashboard navigation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DashState {
    /// Focused panel.
    pub focus: DashPanel,
    /// Selected row per panel.
    pub selected: [usize; 4],
    /// Grouping of the kinds panel.
    pub group: DashGroup,
    /// Growth over the last week instead of the last day.
    pub growth_week: bool,
}

impl DashState {
    /// Selected row of `panel`.
    #[must_use]
    pub const fn selected(&self, panel: DashPanel) -> usize {
        self.selected[panel.index()]
    }

    /// Moves the focused panel's selection by `delta` within `len` rows.
    pub fn move_by(&mut self, delta: isize, len: usize) {
        let slot = &mut self.selected[self.focus.index()];
        *slot = slot.saturating_add_signed(delta).min(len.saturating_sub(1));
    }

    /// Selects the first (`false`) or last (`true`) row of the focused panel.
    pub const fn jump(&mut self, end: bool, len: usize) {
        self.selected[self.focus.index()] = if end { len.saturating_sub(1) } else { 0 };
    }

    /// Clamps every selection to the summary's current row counts.
    pub fn clamp(&mut self, summary: &DashboardSummary) {
        for panel in DashPanel::ALL {
            let len = rows(summary, self, panel);
            let slot = &mut self.selected[panel.index()];
            *slot = (*slot).min(len.saturating_sub(1));
        }
    }
}

/// Rows in `panel`.
#[must_use]
pub fn rows(summary: &DashboardSummary, state: &DashState, panel: DashPanel) -> usize {
    match panel {
        DashPanel::Kinds => match state.group {
            DashGroup::Kind => summary.by_kind.len(),
            DashGroup::Category => summary.by_category.len(),
        },
        DashPanel::Wins => summary.top_items.len(),
        DashPanel::Projects => summary.projects.len(),
        DashPanel::Growth => growth(summary, state).len(),
    }
}

/// The growth list for the selected window.
#[must_use]
pub fn growth<'a>(
    summary: &'a DashboardSummary,
    state: &DashState,
) -> &'a [bclean_model::GrowthItem] {
    if state.growth_week {
        &summary.growth_week
    } else {
        &summary.growth_day
    }
}

/// `path` with the home directory abbreviated to `~`.
#[must_use]
pub fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Draws the dashboard into `body`.
pub fn render<B: Backend>(app: &App<B>, cx: &mut PaintCx<'_, '_>, body: Rect) {
    let Some(summary) = app.dashboard.as_deref() else {
        let text = if app.tree.is_some() || app.progress.is_some() {
            "  computing the dashboard…"
        } else {
            "  loading the index…"
        };
        write(cx, row(body, 1), &styled(text, MUTED));
        return;
    };
    let state = &app.dash;
    let volumes = summary.volumes.len().clamp(1, 3);
    let disk_height = u16::try_from(volumes * 4).unwrap_or(12) + 2;
    if body.width >= WIDE {
        render_wide(app, summary, state, cx, body, disk_height);
    } else {
        render_narrow(app, summary, state, cx, body, disk_height);
    }
}

fn render_wide<B: Backend>(
    app: &App<B>,
    summary: &DashboardSummary,
    state: &DashState,
    cx: &mut PaintCx<'_, '_>,
    body: Rect,
    disk_height: u16,
) {
    let mut y = body.y;
    let take = |y: &mut u16, height: u16| {
        let height = height.min(body.bottom().saturating_sub(*y));
        let area = Rect::new(body.x, *y, body.width, height);
        *y += height;
        area
    };
    disk_panel(summary, cx, take(&mut y, disk_height));

    let kinds_rows = u16::try_from(rows(summary, state, DashPanel::Kinds)).unwrap_or(0);
    let middle = take(&mut y, kinds_rows.clamp(7, 10) + 2);
    let (left, right) = split(middle, 42);
    reclaim_panel(summary, cx, left);
    kinds_panel(summary, state, cx, right);

    let remaining = body.bottom().saturating_sub(y);
    let bottom_height = if remaining >= 18 { 7 } else { 0 };
    let lists = take(&mut y, remaining - bottom_height);
    let (left, right) = split(lists, lists.width * 3 / 5);
    wins_panel(app, summary, state, cx, left);
    projects_panel(app, summary, state, cx, right);
    if bottom_height > 0 {
        let bottom = take(&mut y, bottom_height);
        let (left, right) = split(bottom, bottom.width * 9 / 20);
        growth_panel(summary, state, cx, left);
        activity_panel(summary, cx, right);
    }
}

fn render_narrow<B: Backend>(
    app: &App<B>,
    summary: &DashboardSummary,
    state: &DashState,
    cx: &mut PaintCx<'_, '_>,
    body: Rect,
    disk_height: u16,
) {
    // Priority order; each panel gets its preferred height while space lasts.
    let kinds_rows = u16::try_from(rows(summary, state, DashPanel::Kinds)).unwrap_or(0);
    let plan: [(u16, u8); 7] = [
        (disk_height, 0),
        (9, 1),
        (kinds_rows.clamp(3, 6) + 2, 2),
        (8, 3),
        (7, 4),
        (6, 5),
        (6, 6),
    ];
    let mut y = body.y;
    for (height, panel) in plan {
        let available = body.bottom().saturating_sub(y);
        if available < 4 {
            break;
        }
        let area = Rect::new(body.x, y, body.width, height.min(available));
        y += area.height;
        match panel {
            0 => disk_panel(summary, cx, area),
            1 => reclaim_panel(summary, cx, area),
            2 => kinds_panel(summary, state, cx, area),
            3 => wins_panel(app, summary, state, cx, area),
            4 => projects_panel(app, summary, state, cx, area),
            5 => growth_panel(summary, state, cx, area),
            _ => activity_panel(summary, cx, area),
        }
    }
}

const fn split(area: Rect, left_width: u16) -> (Rect, Rect) {
    let left_width = if left_width > area.width {
        area.width
    } else {
        left_width
    };
    (
        Rect::new(area.x, area.y, left_width, area.height),
        Rect::new(
            area.x + left_width,
            area.y,
            area.width - left_width,
            area.height,
        ),
    )
}

/// Draws a bordered panel and returns its inner area.
fn panel(cx: &mut PaintCx<'_, '_>, area: Rect, title: &str, focused: bool) -> Rect {
    if area.width < 4 || area.height < 3 {
        return Rect::new(area.x, area.y, 0, 0);
    }
    let state = Cell::new(PaneState::new(area));
    if focused {
        let mut s = state.get();
        s.interaction.focused = true;
        state.set(s);
    }
    let styles = PaneStyles {
        border: MUTED,
        focused_border: Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ..PaneStyles::default()
    };
    let pane = PaneComponent::new(
        title.to_owned(),
        Pane::new().title(format!(" {title} ")).styles(styles),
        &state,
        TextBlock::new(Text::from_lines(Vec::new())),
    );
    cx.paint_component(&pane, area);
    Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2)
}

/// Writes `line` on row `y` of `area` if it fits.
fn line_at(cx: &mut PaintCx<'_, '_>, area: Rect, y: u16, line: &Line) {
    if y < area.height {
        write(cx, row(area, y), line);
    }
}

fn pad(text: &str, width: usize) -> String {
    let text = truncate_to_display_width(text, width);
    let fill = width.saturating_sub(display_width(&text));
    format!("{text}{}", " ".repeat(fill))
}

fn ago(at: i64, now: i64) -> String {
    format::age(u64::try_from(now - at).unwrap_or(0))
}

// ----- panels -------------------------------------------------------------------------------

fn disk_panel(summary: &DashboardSummary, cx: &mut PaintCx<'_, '_>, area: Rect) {
    let freshness = summary
        .roots
        .iter()
        .filter_map(|r| r.scanned_at)
        .min()
        .map_or_else(String::new, |at| {
            format!(" · index {} ago", ago(at, summary.generated_at))
        });
    let inner = panel(cx, area, &format!("Disk{freshness}"), false);
    if summary.volumes.is_empty() {
        line_at(cx, inner, 0, &styled("no volume data", MUTED));
        return;
    }
    for (i, volume) in summary.volumes.iter().take(3).enumerate() {
        let y = u16::try_from(i * 4).unwrap_or(u16::MAX);
        volume_rows(volume, cx, inner, y);
    }
}

fn volume_rows(volume: &VolumeSummary, cx: &mut PaintCx<'_, '_>, inner: Rect, y: u16) {
    if y >= inner.height {
        return;
    }
    let stats = &volume.stats;
    // A volume sharing its disk (APFS): its own usage first, then the disk's.
    let label = if volume.other_volumes() > 0 {
        format!(
            "{}  {} · disk {} / {}",
            stats.mount_point,
            format::size(volume.volume_used()),
            format::size(stats.used()),
            format::size(stats.total)
        )
    } else {
        format!(
            "{}  {} / {}",
            stats.mount_point,
            format::size(stats.used()),
            format::size(stats.total)
        )
    };
    let label_width = u16::try_from(display_width(&label) + 2)
        .unwrap_or(u16::MAX)
        .min(inner.width / 2);
    line_at(
        cx,
        inner,
        y,
        &styled(
            truncate_to_display_width(&label, usize::from(label_width)),
            Style::new().add_modifier(Modifier::BOLD),
        ),
    );
    let extra = format!(
        "  avail {}{}",
        format::size(stats.available),
        stats
            .purgeable
            .map(|p| format!(" · purgeable {}", format::size(p)))
            .unwrap_or_default()
    );
    let extra_width = u16::try_from(display_width(&extra)).unwrap_or(0);
    let gauge_width = inner
        .width
        .saturating_sub(label_width + extra_width)
        .max(10);
    let gauge = UsageGauge::new(stats.used(), stats.total);
    gauge.paint(
        &format!("{:.0}%", gauge.fraction() * 100.0),
        Rect::new(inner.x + label_width, inner.y + y, gauge_width, 1),
        cx,
    );
    let ex = inner.x + label_width + gauge_width;
    write(
        cx,
        Rect::new(ex, inner.y + y, inner.right().saturating_sub(ex), 1),
        &styled(extra, MUTED),
    );

    // Composition: reclaimable | other indexed | outside the index | other volumes | free.
    let segments: Vec<(u64, Color, &str)> = [
        (volume.idle_reclaimable(), RECLAIMABLE, "reclaimable"),
        (volume.active_on_volume(), ACTIVE, "active builds"),
        (volume.indexed_kept(), INDEXED, "rest of index"),
        (volume.outside_index(), OUTSIDE, "outside index"),
        (volume.other_volumes(), OTHER_VOLUMES, "other volumes"),
        (stats.available, FREE, "free"),
    ]
    .into_iter()
    .filter(|&(bytes, _, name)| bytes > 0 || !matches!(name, "other volumes" | "active builds"))
    .collect();
    if y + 1 < inner.height {
        stacked_bar(&segments, cx, row(inner, y + 1));
    }
    let mut legend = Vec::new();
    for (bytes, color, name) in segments {
        legend.push(Span::styled("■ ", Style::new().fg(color)));
        legend.push(Span::raw(format!("{name} {}   ", format::size(bytes))));
    }
    line_at(cx, inner, y + 2, &Line::from_spans(legend));

    if y + 3 < inner.height {
        trend_row(volume, cx, row(inner, y + 3));
    }
}

/// A one-row bar split proportionally into colored segments.
fn stacked_bar(segments: &[(u64, Color, &str)], cx: &mut PaintCx<'_, '_>, area: Rect) {
    let total: u64 = segments.iter().map(|s| s.0).sum::<u64>().max(1);
    let width = u64::from(area.width);
    let mut spans = Vec::new();
    let mut used = 0u64;
    let mut acc = 0u64;
    for (i, (bytes, color, _)) in segments.iter().enumerate() {
        acc += bytes;
        // Cumulative rounding keeps the bar exactly `width` cells wide.
        let end = if i + 1 == segments.len() {
            width
        } else {
            (acc * width + total / 2) / total
        };
        let cells = usize::try_from(end.saturating_sub(used)).unwrap_or(0);
        used = end.max(used);
        if cells > 0 {
            spans.push(Span::styled("█".repeat(cells), Style::new().fg(*color)));
        }
    }
    write(cx, area, &Line::from_spans(spans));
}

fn trend_row(volume: &VolumeSummary, cx: &mut PaintCx<'_, '_>, area: Rect) {
    let span = if volume.trend_span_secs == 0 {
        "now".to_owned()
    } else {
        format::age(volume.trend_span_secs)
    };
    let label = format!("{span:>4} ");
    let label_width = u16::try_from(display_width(&label)).unwrap_or(5);
    write(
        cx,
        Rect::new(area.x, area.y, label_width, 1),
        &styled(label, MUTED),
    );
    let spark_width = u16::try_from(volume.trend.len())
        .unwrap_or(60)
        .min(area.width.saturating_sub(label_width + 30))
        .max(8);
    // Relative to the window's minimum, so changes are visible on a mostly full disk.
    let min = volume.trend.iter().copied().min().unwrap_or(0);
    let samples: Vec<u64> = volume.trend.iter().map(|v| v - min + 1).collect();
    let styles = SparklineStyles {
        normal: Style::new().fg(ACCENT),
        latest: Style::new().fg(Color::BrightWhite),
        ..SparklineStyles::default()
    };
    cx.paint_component(
        &SparklineComponent::new("trend", &samples)
            .policy(SparklinePolicy::bare())
            .styles(styles),
        Rect::new(area.x + label_width, area.y, spark_width, 1),
    );
    let forecast = match (volume.growth_per_day, volume.full_in_secs) {
        (Some(rate), Some(full)) if rate > 0 => vec![
            Span::raw(format!("  +{}/day", format::size(rate.unsigned_abs()))),
            Span::styled(
                format!(" → full in ~{}", format::age(full)),
                if full < 7 * 86_400 {
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else if full < 30 * 86_400 {
                    REVIEW
                } else {
                    MUTED
                },
            ),
        ],
        (Some(rate), _) if rate < 0 => vec![Span::styled(
            format!("  −{}/day (shrinking)", format::size(rate.unsigned_abs())),
            SAFE,
        )],
        (Some(_), _) => vec![Span::styled("  steady", MUTED)],
        (None, _) => vec![Span::styled("  collecting history for a forecast", MUTED)],
    };
    let x = area.x + label_width + spark_width;
    write(
        cx,
        Rect::new(x, area.y, area.right().saturating_sub(x), 1),
        &Line::from_spans(forecast),
    );
}

fn reclaim_panel(summary: &DashboardSummary, cx: &mut PaintCx<'_, '_>, area: Rect) {
    let r = &summary.reclaimable;
    let inner = panel(cx, area, "Reclaimable", false);
    line_at(
        cx,
        inner,
        0,
        &Line::from_spans(vec![
            Span::styled(
                format::size(r.total),
                Style::new().fg(RECLAIMABLE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" in {} items", format::count(r.items)), MUTED),
        ]),
    );
    line_at(
        cx,
        inner,
        1,
        &Line::from_spans(vec![
            Span::styled("safe now  ", SAFE),
            Span::styled(
                format!("{:>9}", format::size(r.safe)),
                SAFE.add_modifier(Modifier::BOLD),
            ),
            Span::styled(if r.safe > 0 { "  c clean" } else { "" }, MUTED),
        ]),
    );
    line_at(
        cx,
        inner,
        2,
        &Line::from_spans(vec![
            Span::styled("review    ", REVIEW),
            Span::styled(format!("{:>9}", format::size(r.review())), REVIEW),
        ]),
    );
    line_at(
        cx,
        inner,
        3,
        &Line::from_spans(vec![
            Span::styled("active    ", MUTED),
            Span::styled(format!("{:>9}", format::size(r.active)), MUTED),
            Span::styled("  build output of projects in use", MUTED),
        ]),
    );
    let cell = |bytes: u64| {
        if bytes == 0 {
            format!("{:>11}", "—")
        } else {
            format!("{:>11}", format::size(bytes))
        }
    };
    line_at(
        cx,
        inner,
        4,
        &styled(format!("{:10}{:>11}{:>11}", "", "stale", "fresh"), MUTED),
    );
    line_at(
        cx,
        inner,
        5,
        &Line::raw(format!(
            "{:10}{}{}",
            "ephemeral",
            cell(r.ephemeral_stale),
            cell(r.ephemeral_fresh)
        )),
    );
    line_at(
        cx,
        inner,
        6,
        &Line::raw(format!(
            "{:10}{}{}",
            "durable",
            cell(r.durable_stale),
            cell(0)
        )),
    );
}

/// Highlights the selected row of a focused panel.
const fn row_style(focused: bool, selected: bool) -> Style {
    if focused && selected {
        SELECTED
    } else {
        Style::new()
    }
}

/// First visible row so `selected` stays in view.
const fn scroll(selected: usize, visible: usize) -> usize {
    if visible == 0 || selected < visible {
        0
    } else {
        selected + 1 - visible
    }
}

/// Paints a size bar like the browser's.
fn size_bar(value: u64, max: u64, cx: &mut PaintCx<'_, '_>, area: Rect, color: Color) {
    let styles = ProgressBarStyles {
        filled: Style::new().fg(color),
        empty: Style::new().fg(Color::BrightBlack),
        complete: Style::new().fg(color),
        ..ProgressBarStyles::default()
    };
    cx.paint_component(
        &ProgressBarComponent::new("dash-bar", ProgressBarValue::ratio(value, max.max(1)))
            .policy(ProgressBarPolicy::bare().symbols("█", " ", "▌"))
            .styles(styles),
        area,
    );
}

fn kinds_panel(
    summary: &DashboardSummary,
    state: &DashState,
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
) {
    let focused = state.focus == DashPanel::Kinds;
    let title = match state.group {
        DashGroup::Kind => "By kind (v: by ecosystem)",
        DashGroup::Category => "By ecosystem (v: by kind)",
    };
    let inner = panel(cx, area, title, focused);
    let rows: Vec<(String, u64, u64, u64)> = match state.group {
        DashGroup::Kind => summary
            .by_kind
            .iter()
            .map(|b| (kind_label(b.kind).to_owned(), b.bytes, b.items, b.safe))
            .collect(),
        DashGroup::Category => summary
            .by_category
            .iter()
            .map(|b| {
                (
                    b.category
                        .clone()
                        .unwrap_or_else(|| "uncategorized".to_owned()),
                    b.bytes,
                    b.items,
                    b.safe,
                )
            })
            .collect(),
    };
    if rows.is_empty() {
        line_at(cx, inner, 0, &styled("nothing reclaimable found", MUTED));
        return;
    }
    let max = rows.iter().map(|r| r.1).max().unwrap_or(1);
    let visible = usize::from(inner.height);
    let selected = state.selected(DashPanel::Kinds);
    let first = scroll(selected, visible);
    let label_width = 17u16.min(inner.width / 3);
    let numbers = 24u16;
    let bar_width = inner.width.saturating_sub(label_width + numbers + 1);
    for (offset, (index, (label, bytes, items, safe))) in rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let y = u16::try_from(offset).unwrap_or(u16::MAX);
        let line_area = row(inner, y);
        let style = row_style(focused, index == selected);
        cx.fill(crate::render::rect_local(line_area), " ", style);
        write(
            cx,
            Rect::new(line_area.x, line_area.y, label_width, 1),
            &styled(pad(label, usize::from(label_width)), style),
        );
        if bar_width >= 4 {
            size_bar(
                *bytes,
                max,
                cx,
                Rect::new(line_area.x + label_width, line_area.y, bar_width, 1),
                if *safe == *bytes {
                    Color::Green
                } else {
                    RECLAIMABLE
                },
            );
        }
        let x = line_area.x + label_width + bar_width + 1;
        write(
            cx,
            Rect::new(x, line_area.y, line_area.right().saturating_sub(x), 1),
            &styled(
                format!(
                    "{:>9} {:>6} item{}",
                    format::size(*bytes),
                    format::count(*items),
                    if *items == 1 { "" } else { "s" }
                ),
                style,
            ),
        );
    }
}

fn wins_panel<B: Backend>(
    app: &App<B>,
    summary: &DashboardSummary,
    state: &DashState,
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
) {
    let focused = state.focus == DashPanel::Wins;
    let inner = panel(cx, area, "Top candidates · by score (✓ safe)", focused);
    if summary.top_items.is_empty() {
        line_at(cx, inner, 0, &styled("no cleanup candidates", MUTED));
        return;
    }
    let visible = usize::from(inner.height);
    let selected = state.selected(DashPanel::Wins);
    let first = scroll(selected, visible);
    let width = usize::from(inner.width);
    for (offset, (index, item)) in summary
        .top_items
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let y = u16::try_from(offset).unwrap_or(u16::MAX);
        let style = row_style(focused, index == selected);
        let mark = if app.marked.contains(&item.path) {
            Span::styled("● ", style.fg(Color::Yellow))
        } else {
            Span::styled("  ", style)
        };
        let tag = item
            .category
            .clone()
            .unwrap_or_else(|| kind_label(item.kind).to_owned());
        // Build output of a project in use: its own age says nothing about the project's.
        let idle = if item.active {
            "active".to_owned()
        } else {
            item.idle_secs
                .map_or_else(String::new, |s| format!("idle {}", format::age(s)))
        };
        let right = format!(
            " {tag:>10} {idle:>10} {}",
            if item.safe { "✓" } else { " " }
        );
        let size = format!("{:>9}  ", format::size(item.size));
        let path_width = width.saturating_sub(2 + display_width(&size) + display_width(&right));
        let path = format::truncate_left(&tilde(&item.path), path_width);
        cx.fill(crate::render::rect_local(row(inner, y)), " ", style);
        line_at(
            cx,
            inner,
            y,
            &Line::from_spans(vec![
                mark,
                Span::styled(size, style.add_modifier(Modifier::BOLD)),
                Span::styled(pad(&path, path_width), style),
                Span::styled(
                    right,
                    if focused && index == selected {
                        style
                    } else {
                        MUTED
                    },
                ),
            ]),
        );
    }
}

fn projects_panel<B: Backend>(
    app: &App<B>,
    summary: &DashboardSummary,
    state: &DashState,
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
) {
    let focused = state.focus == DashPanel::Projects;
    let inner = panel(cx, area, "Projects", focused);
    if summary.projects.is_empty() {
        line_at(
            cx,
            inner,
            0,
            &styled("no projects with reclaimable data", MUTED),
        );
        return;
    }
    let visible = usize::from(inner.height);
    let selected = state.selected(DashPanel::Projects);
    let first = scroll(selected, visible);
    let width = usize::from(inner.width);
    for (offset, (index, project)) in summary
        .projects
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let y = u16::try_from(offset).unwrap_or(u16::MAX);
        let style = row_style(focused, index == selected);
        let all_marked = project.paths.iter().all(|p| app.marked.contains(p));
        let mark = if all_marked && !project.paths.is_empty() {
            Span::styled("● ", style.fg(Color::Yellow))
        } else {
            Span::styled("  ", style)
        };
        let name = project.path.file_name().map_or_else(
            || tilde(&project.path),
            |n| n.to_string_lossy().into_owned(),
        );
        let size = format!("{:>9}  ", format::size(project.bytes));
        let entries = project.entries.join(", ");
        let name_width = 18.min(width / 3);
        let rest = width.saturating_sub(2 + display_width(&size) + name_width + 1);
        cx.fill(crate::render::rect_local(row(inner, y)), " ", style);
        line_at(
            cx,
            inner,
            y,
            &Line::from_spans(vec![
                mark,
                Span::styled(pad(&name, name_width), style.add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {size}"), style),
                Span::styled(
                    truncate_to_display_width(&entries, rest),
                    if focused && index == selected {
                        style
                    } else {
                        MUTED
                    },
                ),
            ]),
        );
    }
}

fn growth_panel(
    summary: &DashboardSummary,
    state: &DashState,
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
) {
    let focused = state.focus == DashPanel::Growth;
    let title = if state.growth_week {
        "Grew · 7 days (w: 24h)"
    } else {
        "Grew · 24 hours (w: 7d)"
    };
    let inner = panel(cx, area, title, focused);
    let items = growth(summary, state);
    if items.is_empty() {
        line_at(cx, inner, 0, &styled("no growth recorded yet", MUTED));
        return;
    }
    let visible = usize::from(inner.height);
    let selected = state.selected(DashPanel::Growth);
    let first = scroll(selected, visible);
    let width = usize::from(inner.width);
    for (offset, (index, item)) in items
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .enumerate()
    {
        let y = u16::try_from(offset).unwrap_or(u16::MAX);
        let style = row_style(focused, index == selected);
        let delta = format!("+{:>9}  ", format::size(item.delta().unsigned_abs()));
        let path_width = width.saturating_sub(display_width(&delta));
        cx.fill(crate::render::rect_local(row(inner, y)), " ", style);
        line_at(
            cx,
            inner,
            y,
            &Line::from_spans(vec![
                Span::styled(
                    delta,
                    style.fg(if focused && index == selected {
                        Color::BrightWhite
                    } else {
                        Color::Yellow
                    }),
                ),
                Span::styled(format::truncate_left(&tilde(&item.path), path_width), style),
            ]),
        );
    }
}

fn activity_panel(summary: &DashboardSummary, cx: &mut PaintCx<'_, '_>, area: Rect) {
    let a = &summary.activity;
    let inner = panel(cx, area, "Activity", false);
    let now = summary.generated_at;
    let freed = if a.actions_week == 0 {
        Line::from_spans(vec![Span::styled("nothing cleaned this week", MUTED)])
    } else {
        Line::from_spans(vec![
            Span::raw("freed "),
            Span::styled(
                format::size(a.freed_week),
                SAFE.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " this week in {} action{}",
                    a.actions_week,
                    if a.actions_week == 1 { "" } else { "s" }
                ),
                MUTED,
            ),
        ])
    };
    line_at(cx, inner, 0, &freed);
    if let Some(last) = &a.last_action {
        line_at(
            cx,
            inner,
            1,
            &styled(
                format!(
                    "last: {} {} ago{}",
                    tilde(&last.path),
                    ago(last.at, now),
                    if last.reversible {
                        " · bclean undo"
                    } else {
                        ""
                    }
                ),
                MUTED,
            ),
        );
    }
    let alerts = match (&a.top_alert, a.alerts) {
        (_, 0) => styled("no active alerts", MUTED),
        (Some(top), n) => Line::from_spans(vec![
            Span::styled(
                format!("{n} alert{}: ", if n == 1 { "" } else { "s" }),
                REVIEW.add_modifier(Modifier::BOLD),
            ),
            Span::raw(top.clone()),
        ]),
        (None, n) => styled(format!("{n} alerts"), REVIEW),
    };
    line_at(cx, inner, 2, &alerts);
    let watching = if a.watchers.is_empty() {
        "not watching".to_owned()
    } else {
        format!("watching ({})", a.watchers.join(", ").to_lowercase())
    };
    line_at(
        cx,
        inner,
        3,
        &styled(
            format!(
                "{watching} · {} rescan{} · {} refreshes",
                a.full_scans,
                if a.full_scans == 1 { "" } else { "s" },
                format::count(a.refreshes)
            ),
            MUTED,
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panels_cycle_and_selection_clamps() {
        assert_eq!(DashPanel::Kinds.cycle(false), DashPanel::Wins);
        assert_eq!(DashPanel::Kinds.cycle(true), DashPanel::Growth);
        let mut state = DashState::default();
        state.move_by(5, 3);
        assert_eq!(state.selected(DashPanel::Kinds), 2);
        state.move_by(-10, 3);
        assert_eq!(state.selected(DashPanel::Kinds), 0);
        state.jump(true, 4);
        assert_eq!(state.selected(DashPanel::Kinds), 3);
        state.clamp(&DashboardSummary::default());
        assert_eq!(state.selected(DashPanel::Kinds), 0);
    }

    #[test]
    fn scroll_keeps_selection_visible() {
        assert_eq!(scroll(0, 5), 0);
        assert_eq!(scroll(4, 5), 0);
        assert_eq!(scroll(7, 5), 3);
    }
}
