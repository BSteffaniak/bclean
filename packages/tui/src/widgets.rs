//! Widgets built on `bmux_tui_components`.
//!
//! * [`UsageGauge`]: a threshold-colored usage gauge built on bmux's `ProgressBarComponent`.

use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarStyles, ProgressBarValue,
};

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
        cx.paint_component(&component, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_colors() {
        assert_eq!(UsageGauge::new(1, 10).color(), Color::Green);
        assert_eq!(UsageGauge::new(85, 100).color(), Color::Yellow);
        assert_eq!(UsageGauge::new(95, 100).color(), Color::Red);
        assert!(UsageGauge::new(1, 0).fraction().abs() < f64::EPSILON);
    }
}
