//! Generic widgets that `bmux_tui_components` does not (yet) provide.
//!
//! Everything in this module is domain-neutral (no bclean types) and written in the style of
//! bmux components so it can be upstreamed to `bmux_tui_components` later:
//!
//! * [`FixedRowsState`] / [`paint_fixed_rows`]: a virtual list for *uniform-height* rows over
//!   an arbitrarily large collection. bmux's `VirtualList` needs every item materialized as a
//!   component to measure it; with uniform rows the visible window is pure arithmetic, so only
//!   the visible rows are ever touched (a directory with millions of entries costs the same
//!   per frame as one with ten).
//! * [`UsageGauge`]: a threshold-colored usage gauge built on bmux's `ProgressBarComponent`.
//! * [`paint_component`]: paints a bmux `Component` into a terminal-space rectangle.

use std::ops::Range;

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarStyles, ProgressBarValue,
};
use bmux_tui_components::scrollbar::{Scrollbar, ScrollbarState};

/// Lays out and paints `component` into `area` (terminal coordinates of `cx`'s root).
pub fn paint_component(component: &impl Component, area: Rect, cx: &mut PaintCx<'_, '_>) {
    if area.is_empty() {
        return;
    }
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
}

/// Selection and scroll state of a uniform-height virtual list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FixedRowsState {
    /// Selected row index.
    pub selected: usize,
    /// First visible row index.
    pub offset: usize,
}

impl FixedRowsState {
    /// Clamps selection and offset to a collection of `len` rows and a `viewport` of rows,
    /// scrolling the minimum amount to keep the selection visible.
    pub const fn clamp(&mut self, len: usize, viewport: usize) {
        if len == 0 {
            self.selected = 0;
            self.offset = 0;
            return;
        }
        if self.selected >= len {
            self.selected = len - 1;
        }
        let viewport = if viewport == 0 { 1 } else { viewport };
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
        let max_offset = len.saturating_sub(viewport);
        if self.offset > max_offset {
            self.offset = max_offset;
        }
    }

    /// Moves the selection by `delta` rows (saturating at both ends).
    pub const fn move_by(&mut self, delta: isize, len: usize, viewport: usize) {
        self.selected = self.selected.saturating_add_signed(delta);
        self.clamp(len, viewport);
    }

    /// Moves one page down (`forward`) or up.
    pub const fn page(&mut self, forward: bool, len: usize, viewport: usize) {
        let step = if viewport > 1 { viewport - 1 } else { 1 };
        #[allow(clippy::cast_possible_wrap)] // viewport heights are tiny
        let step = step as isize;
        self.move_by(if forward { step } else { -step }, len, viewport);
    }

    /// Selects the first row.
    pub const fn top(&mut self, len: usize, viewport: usize) {
        self.selected = 0;
        self.clamp(len, viewport);
    }

    /// Selects the last row.
    pub const fn bottom(&mut self, len: usize, viewport: usize) {
        self.selected = len.saturating_sub(1);
        self.clamp(len, viewport);
    }

    /// Scrolls the viewport by `delta` rows without moving the selection out of view
    /// (mouse-wheel behavior).
    pub fn scroll_by(&mut self, delta: isize, len: usize, viewport: usize) {
        let max_offset = len.saturating_sub(viewport.max(1));
        self.offset = self.offset.saturating_add_signed(delta).min(max_offset);
        let last = self.offset + viewport.max(1) - 1;
        self.selected = self
            .selected
            .clamp(self.offset, last.min(len.saturating_sub(1)));
    }

    /// Indices of the rows visible in a `viewport` of rows.
    #[must_use]
    pub fn visible(&self, len: usize, viewport: usize) -> Range<usize> {
        let start = self.offset.min(len);
        start..(start + viewport).min(len)
    }
}

/// Paints the visible window of a uniform-height list into `area`, with a bmux scrollbar in
/// the rightmost column when the content overflows.
///
/// `paint_row(index, row_area, selected, cx)` is called once per visible row only.
/// The caller should [`FixedRowsState::clamp`] beforehand with `area.height`.
pub fn paint_fixed_rows(
    cx: &mut PaintCx<'_, '_>,
    area: Rect,
    len: usize,
    state: FixedRowsState,
    mut paint_row: impl FnMut(usize, Rect, bool, &mut PaintCx<'_, '_>),
) {
    if area.is_empty() {
        return;
    }
    let viewport = usize::from(area.height);
    let overflow = len > viewport;
    let rows_width = if overflow {
        area.width.saturating_sub(1)
    } else {
        area.width
    };
    for (row, index) in state.visible(len, viewport).enumerate() {
        let y = area
            .y
            .saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        paint_row(
            index,
            Rect::new(area.x, y, rows_width, 1),
            index == state.selected,
            cx,
        );
    }
    if overflow {
        let bar_area = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
        // The scrollbar works in u16 units; scale very long lists down proportionally.
        let (content, offset) = scale_for_scrollbar(len, state.offset, viewport);
        let scroll_state = ScrollbarState::new(content, area.height).offset(offset);
        Scrollbar::new().paint(bar_area, &scroll_state, cx);
    }
}

fn scale_for_scrollbar(len: usize, offset: usize, viewport: usize) -> (u16, u16) {
    let max = usize::from(u16::MAX);
    if len <= max {
        return (
            u16::try_from(len).unwrap_or(u16::MAX),
            u16::try_from(offset).unwrap_or(u16::MAX),
        );
    }
    let factor = len.div_ceil(max - viewport.min(max - 1));
    (
        u16::try_from(len / factor).unwrap_or(u16::MAX),
        u16::try_from(offset / factor).unwrap_or(u16::MAX),
    )
}

/// Threshold-colored usage gauge (green → yellow → red) over bmux's progress bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UsageGauge {
    /// Used units.
    pub used: u64,
    /// Total units.
    pub total: u64,
    /// Fraction at which the gauge turns yellow.
    pub warn_at: f64,
    /// Fraction at which the gauge turns red.
    pub critical_at: f64,
}

impl UsageGauge {
    /// Creates a gauge with default thresholds (80% / 90%).
    #[must_use]
    pub const fn new(used: u64, total: u64) -> Self {
        Self {
            used,
            total,
            warn_at: 0.8,
            critical_at: 0.9,
        }
    }

    /// Used fraction.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // display only
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used as f64 / self.total as f64
        }
    }

    /// Fill color for the current fraction.
    #[must_use]
    pub fn color(&self) -> Color {
        let fraction = self.fraction();
        if fraction >= self.critical_at {
            Color::Red
        } else if fraction >= self.warn_at {
            Color::Yellow
        } else {
            Color::Green
        }
    }

    /// Paints the gauge with `label` drawn inside the bar.
    pub fn paint(&self, label: &str, area: Rect, cx: &mut PaintCx<'_, '_>) {
        let color = self.color();
        let styles = ProgressBarStyles {
            filled: Style::new().fg(color),
            empty: Style::new().fg(Color::BrightBlack),
            label: Style::new()
                .fg(Color::BrightWhite)
                .add_modifier(Modifier::BOLD),
            complete: Style::new().fg(color),
            indeterminate: Style::new().fg(color),
            background: Style::new(),
        };
        let component = ProgressBarComponent::new(
            "usage-gauge",
            ProgressBarValue::ratio(self.used, self.total),
        )
        .label(label)
        .policy(ProgressBarPolicy::compact())
        .styles(styles);
        paint_component(&component, area, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_rows_navigation() {
        let mut s = FixedRowsState::default();
        s.move_by(5, 100, 10);
        assert_eq!((s.selected, s.offset), (5, 0));
        s.move_by(10, 100, 10);
        assert_eq!((s.selected, s.offset), (15, 6));
        s.page(true, 100, 10);
        assert_eq!(s.selected, 24);
        s.bottom(100, 10);
        assert_eq!((s.selected, s.offset), (99, 90));
        s.move_by(-200, 100, 10);
        assert_eq!((s.selected, s.offset), (0, 0));
        s.selected = 50;
        s.clamp(20, 10);
        assert_eq!((s.selected, s.offset), (19, 10));
        s.clamp(0, 10);
        assert_eq!((s.selected, s.offset), (0, 0));
        assert_eq!(
            FixedRowsState {
                selected: 3,
                offset: 2
            }
            .visible(4, 10),
            2..4
        );
    }

    #[test]
    fn wheel_scroll_keeps_selection_visible() {
        let mut s = FixedRowsState::default();
        s.scroll_by(5, 100, 10);
        assert_eq!((s.selected, s.offset), (5, 5));
        s.scroll_by(1_000, 100, 10);
        assert_eq!(s.offset, 90);
    }

    #[test]
    fn gauge_colors() {
        assert_eq!(UsageGauge::new(1, 10).color(), Color::Green);
        assert_eq!(UsageGauge::new(85, 100).color(), Color::Yellow);
        assert_eq!(UsageGauge::new(95, 100).color(), Color::Red);
        assert!(UsageGauge::new(1, 0).fraction().abs() < f64::EPSILON);
    }

    #[test]
    fn scrollbar_scaling() {
        assert_eq!(scale_for_scrollbar(100, 10, 5), (100, 10));
        let (content, offset) = scale_for_scrollbar(10_000_000, 5_000_000, 40);
        assert!(content > 0 && offset <= content);
    }
}
