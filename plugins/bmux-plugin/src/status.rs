//! Pure status model and rendering: pressure classification, the compact status segment, and
//! the detailed status report.

use std::fmt::Write as _;

use bclean_model::{AlertEvent, Severity, VolumeStats};
use bclean_protocol::DaemonStatus;

use crate::settings::{Settings, Thresholds};

/// Pressure level of a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pressure {
    /// Below the warning threshold.
    Ok,
    /// At or above the warning threshold.
    Warning,
    /// At or above the critical threshold.
    Critical,
}

impl Pressure {
    /// Classifies a used fraction against `thresholds`.
    #[must_use]
    pub fn classify(fraction: f64, thresholds: Thresholds) -> Self {
        if fraction >= thresholds.critical {
            Self::Critical
        } else if fraction >= thresholds.warning {
            Self::Warning
        } else {
            Self::Ok
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// Connection state of the plugin's view of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Connection {
    /// No attempt has completed yet.
    #[default]
    Connecting,
    /// Connected and receiving updates.
    Connected,
    /// The daemon is unreachable; the reason is kept for `status`.
    Offline(String),
}

/// Everything the plugin knows about the daemon at one point in time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Connection state.
    pub connection: Connection,
    /// Last daemon status.
    pub daemon: Option<DaemonStatus>,
    /// Last known volume samples.
    pub volumes: Vec<VolumeStats>,
    /// Active (unacknowledged) alerts.
    pub alerts: Vec<AlertEvent>,
    /// Reclaimable bytes from the daemon's dashboard summary (absent on older daemons).
    pub reclaimable: Option<Reclaimable>,
    /// When the snapshot was last updated (Unix seconds).
    pub updated_at: Option<i64>,
}

/// Reclaimable totals shown by the plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reclaimable {
    /// All reclaimable bytes.
    pub total: u64,
    /// Bytes an automatic clean would select.
    pub safe: u64,
}

/// Smallest safe-to-clean amount worth a tab-bar mention.
pub const RECLAIMABLE_SEGMENT_MIN: u64 = 1 << 30;

impl Snapshot {
    /// Upserts volume samples by mount point.
    pub fn merge_volumes(&mut self, volumes: Vec<VolumeStats>) {
        for volume in volumes {
            match self
                .volumes
                .iter_mut()
                .find(|v| v.mount_point == volume.mount_point)
            {
                Some(existing) => *existing = volume,
                None => self.volumes.push(volume),
            }
        }
    }

    /// Records a raised alert. Returns `true` when it is new or escalated (worth notifying).
    pub fn raise_alert(&mut self, alert: AlertEvent) -> bool {
        if let Some(existing) = self.alerts.iter_mut().find(|a| a.key == alert.key) {
            let escalated = alert.severity > existing.severity;
            *existing = alert;
            escalated
        } else {
            self.alerts.push(alert);
            true
        }
    }
}

/// The most pressured volume among those selected by `filter` (empty = all), by used fraction.
#[must_use]
pub fn most_pressured<'a>(
    volumes: &'a [VolumeStats],
    filter: &[String],
) -> Option<&'a VolumeStats> {
    volumes
        .iter()
        .filter(|v| v.total > 0)
        .filter(|v| filter.is_empty() || filter.contains(&v.mount_point))
        .max_by(|a, b| a.used_fraction().total_cmp(&b.used_fraction()))
}

/// A quarter-step pie glyph for a used fraction.
#[must_use]
pub fn glyph(fraction: f64) -> char {
    match fraction {
        f if f >= 0.875 => '●',
        f if f >= 0.625 => '◕',
        f if f >= 0.375 => '◑',
        f if f >= 0.125 => '◔',
        _ => '○',
    }
}

/// Whole percent used, rounded down so `99.9%` never displays as `100%`.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // clamped to 0..=100 first
pub const fn percent(fraction: f64) -> u8 {
    // The epsilon absorbs binary representation error (0.87 * 100 = 86.999...).
    fraction.clamp(0.0, 1.0).mul_add(100.0, 1e-9).floor() as u8
}

/// Output style for the segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Plain text.
    Plain,
    /// ANSI SGR colors (yellow warning, bold red critical, dim offline).
    Ansi,
}

const RESET: &str = "\x1b[0m";

fn paint(out: &mut String, style: Style, sgr: &str, text: &str) {
    if style == Style::Ansi && !sgr.is_empty() {
        let _ = write!(out, "\x1b[{sgr}m{text}{RESET}");
    } else {
        out.push_str(text);
    }
}

const fn pressure_sgr(pressure: Pressure) -> &'static str {
    match pressure {
        Pressure::Ok => "",
        Pressure::Warning => "33",
        Pressure::Critical => "1;31",
    }
}

/// Renders the compact status segment, e.g. `◕ 87% /  ⚠ 2`. Empty when there is nothing
/// to show under the current settings.
#[must_use]
pub fn render_segment(snapshot: &Snapshot, settings: &Settings, style: Style) -> String {
    let mut out = String::new();
    if snapshot.connection != Connection::Connected {
        if settings.offline_indicator {
            paint(&mut out, style, "2", "◌ bclean");
        }
        return out;
    }
    let thresholds = settings.thresholds();
    let top = most_pressured(&snapshot.volumes, &settings.volumes);
    if let Some(volume) = top {
        let fraction = volume.used_fraction();
        let pressure = Pressure::classify(fraction, thresholds);
        if pressure != Pressure::Ok || settings.show_when_ok {
            let text = format!(
                "{} {}% {}",
                glyph(fraction),
                percent(fraction),
                volume.mount_point
            );
            paint(&mut out, style, pressure_sgr(pressure), &text);
        }
    }
    if settings.show_reclaimable
        && let Some(reclaimable) = snapshot.reclaimable
        && reclaimable.safe >= RECLAIMABLE_SEGMENT_MIN
    {
        if !out.is_empty() {
            out.push_str("  ");
        }
        paint(
            &mut out,
            style,
            "32",
            &format!("♻ {}", human_bytes(reclaimable.safe)),
        );
    }
    let alerts = snapshot.alerts.len();
    if alerts > 0 {
        if !out.is_empty() {
            out.push_str("  ");
        }
        let critical = snapshot
            .alerts
            .iter()
            .any(|a| a.severity == Severity::Critical);
        let sgr = if critical { "1;31" } else { "33" };
        paint(&mut out, style, sgr, &format!("⚠ {alerts}"));
    }
    out
}

/// Formats a byte count with binary units (`12.3 GiB`).
#[must_use]
#[allow(clippy::cast_precision_loss)] // display only
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

const fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Critical => "critical",
    }
}

/// One-line summary used as a bmux command status message.
#[must_use]
pub fn render_summary(snapshot: &Snapshot, settings: &Settings) -> String {
    match &snapshot.connection {
        Connection::Connecting => "bclean: connecting to daemon".to_owned(),
        Connection::Offline(reason) => format!("bclean: daemon unavailable ({reason})"),
        Connection::Connected => {
            let segment = render_segment(snapshot, settings, Style::Plain);
            if segment.is_empty() {
                "bclean: no monitored volumes".to_owned()
            } else {
                format!("bclean: {segment}")
            }
        }
    }
}

/// One-line description of an alert.
#[must_use]
pub fn alert_line(alert: &AlertEvent) -> String {
    format!("[{}] {}", severity_label(alert.severity), alert.title)
}

/// Multi-line detailed report for `status`.
#[must_use]
pub fn render_details(snapshot: &Snapshot, settings: &Settings, endpoint: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "bclean daemon: {endpoint}");
    match &snapshot.connection {
        Connection::Connecting => {
            let _ = writeln!(out, "  state: connecting");
        }
        Connection::Offline(reason) => {
            let _ = writeln!(out, "  state: unavailable ({reason})");
            return out;
        }
        Connection::Connected => {
            let _ = writeln!(out, "  state: connected");
        }
    }
    if let Some(daemon) = &snapshot.daemon {
        let _ = writeln!(
            out,
            "  version {} (pid {}), {} root(s), {} policy(ies)",
            daemon.version,
            daemon.pid,
            daemon.roots.len(),
            daemon.policies
        );
    }

    let thresholds = settings.thresholds();
    let mut volumes: Vec<&VolumeStats> = snapshot
        .volumes
        .iter()
        .filter(|v| settings.volumes.is_empty() || settings.volumes.contains(&v.mount_point))
        .collect();
    volumes.sort_by(|a, b| b.used_fraction().total_cmp(&a.used_fraction()));
    let _ = writeln!(
        out,
        "volumes (warning {}%, critical {}%):",
        settings.warning_percent, settings.critical_percent
    );
    if volumes.is_empty() {
        let _ = writeln!(out, "  (none reported)");
    }
    for volume in volumes {
        let fraction = volume.used_fraction();
        let _ = writeln!(
            out,
            "  {} {:>3}% {:<8} {}  ({} free of {}, {})",
            glyph(fraction),
            percent(fraction),
            Pressure::classify(fraction, thresholds).label(),
            volume.mount_point,
            human_bytes(volume.available),
            human_bytes(volume.total),
            volume.fs_type
        );
    }

    if let Some(reclaimable) = snapshot.reclaimable {
        let _ = writeln!(
            out,
            "reclaimable: {} ({} safe to clean now; run `bclean` for the dashboard)",
            human_bytes(reclaimable.total),
            human_bytes(reclaimable.safe)
        );
    }
    let _ = writeln!(out, "active alerts: {}", snapshot.alerts.len());
    for alert in &snapshot.alerts {
        let _ = writeln!(out, "  {}", alert_line(alert));
        if !alert.body.is_empty() {
            let _ = writeln!(out, "      {}", alert.body);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(mount: &str, total: u64, free: u64) -> VolumeStats {
        VolumeStats {
            mount_point: mount.to_owned(),
            fs_type: "apfs".to_owned(),
            total,
            free,
            available: free,
            purgeable: None,
            at: 0,
        }
    }

    fn alert(key: &str, severity: Severity) -> AlertEvent {
        AlertEvent {
            key: key.to_owned(),
            policy_id: None,
            severity,
            title: format!("{key} is filling up"),
            body: String::new(),
            volume: None,
            at: 0,
        }
    }

    fn connected(volumes: Vec<VolumeStats>, alerts: Vec<AlertEvent>) -> Snapshot {
        Snapshot {
            connection: Connection::Connected,
            daemon: None,
            volumes,
            alerts,
            reclaimable: None,
            updated_at: Some(0),
        }
    }

    #[test]
    fn classifies_thresholds() {
        let t = Thresholds {
            warning: 0.8,
            critical: 0.9,
        };
        assert_eq!(Pressure::classify(0.5, t), Pressure::Ok);
        assert_eq!(Pressure::classify(0.8, t), Pressure::Warning);
        assert_eq!(Pressure::classify(0.95, t), Pressure::Critical);
    }

    #[test]
    fn picks_most_pressured_volume_honoring_filter() {
        let volumes = vec![
            volume("/", 100, 13),
            volume("/data", 100, 5),
            volume("/empty", 0, 0),
        ];
        assert_eq!(most_pressured(&volumes, &[]).unwrap().mount_point, "/data");
        assert_eq!(
            most_pressured(&volumes, &["/".to_owned()])
                .unwrap()
                .mount_point,
            "/"
        );
        assert!(most_pressured(&volumes, &["/nope".to_owned()]).is_none());
    }

    #[test]
    fn renders_plain_segment() {
        let settings = Settings::default();
        let snapshot = connected(
            vec![volume("/", 100, 13)],
            vec![alert("a", Severity::Warning), alert("b", Severity::Warning)],
        );
        assert_eq!(
            render_segment(&snapshot, &settings, Style::Plain),
            "◕ 87% /  ⚠ 2"
        );
    }

    #[test]
    fn colors_by_pressure() {
        let settings = Settings::default();
        let warn = connected(vec![volume("/", 100, 15)], Vec::new());
        assert_eq!(
            render_segment(&warn, &settings, Style::Ansi),
            "\x1b[33m◕ 85% /\x1b[0m"
        );
        let crit = connected(
            vec![volume("/", 100, 4)],
            vec![alert("a", Severity::Critical)],
        );
        assert_eq!(
            render_segment(&crit, &settings, Style::Ansi),
            "\x1b[1;31m● 96% /\x1b[0m  \x1b[1;31m⚠ 1\x1b[0m"
        );
        let ok = connected(vec![volume("/", 100, 60)], Vec::new());
        assert_eq!(render_segment(&ok, &settings, Style::Ansi), "◑ 40% /");
    }

    #[test]
    fn hides_ok_volume_when_configured() {
        let settings = Settings {
            show_when_ok: false,
            ..Settings::default()
        };
        let ok = connected(vec![volume("/", 100, 60)], Vec::new());
        assert_eq!(render_segment(&ok, &settings, Style::Plain), "");
        let ok_with_alert = connected(vec![volume("/", 100, 60)], vec![alert("a", Severity::Info)]);
        assert_eq!(
            render_segment(&ok_with_alert, &settings, Style::Plain),
            "⚠ 1"
        );
    }

    #[test]
    fn offline_indicator_is_dim_or_absent() {
        let snapshot = Snapshot {
            connection: Connection::Offline("not running".to_owned()),
            ..Snapshot::default()
        };
        assert_eq!(
            render_segment(&snapshot, &Settings::default(), Style::Ansi),
            "\x1b[2m◌ bclean\x1b[0m"
        );
        let hidden = Settings {
            offline_indicator: false,
            ..Settings::default()
        };
        assert_eq!(render_segment(&snapshot, &hidden, Style::Plain), "");
        assert!(render_summary(&snapshot, &hidden).contains("not running"));
    }

    #[test]
    fn merges_volumes_and_dedupes_alerts() {
        let mut snapshot = connected(vec![volume("/", 100, 50)], Vec::new());
        snapshot.merge_volumes(vec![volume("/", 100, 10), volume("/data", 100, 90)]);
        assert_eq!(snapshot.volumes.len(), 2);
        assert_eq!(snapshot.volumes[0].free, 10);

        assert!(snapshot.raise_alert(alert("a", Severity::Warning)));
        assert!(!snapshot.raise_alert(alert("a", Severity::Warning)));
        assert!(snapshot.raise_alert(alert("a", Severity::Critical)));
        assert_eq!(snapshot.alerts.len(), 1);
    }

    #[test]
    fn glyphs_and_percent() {
        assert_eq!(glyph(0.0), '○');
        assert_eq!(glyph(0.3), '◔');
        assert_eq!(glyph(0.5), '◑');
        assert_eq!(glyph(0.7), '◕');
        assert_eq!(glyph(0.99), '●');
        assert_eq!(percent(0.999), 99);
        assert_eq!(percent(1.5), 100);
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn segment_and_details_show_reclaimable_space() {
        let mut snapshot = connected(vec![volume("/", 100, 50)], Vec::new());
        snapshot.reclaimable = Some(Reclaimable {
            total: 80 << 30,
            safe: 61 << 30,
        });
        let segment = render_segment(&snapshot, &Settings::default(), Style::Plain);
        assert!(segment.ends_with("♻ 61.0 GiB"), "{segment}");
        let details = render_details(&snapshot, &Settings::default(), "/tmp/b.sock");
        assert!(
            details.contains("reclaimable: 80.0 GiB (61.0 GiB safe"),
            "{details}"
        );

        // Small amounts and opting out keep the segment quiet.
        snapshot.reclaimable = Some(Reclaimable {
            total: 10,
            safe: 10,
        });
        let segment = render_segment(&snapshot, &Settings::default(), Style::Plain);
        assert!(!segment.contains('♻'), "{segment}");
        snapshot.reclaimable = Some(Reclaimable {
            total: 80 << 30,
            safe: 61 << 30,
        });
        let quiet = Settings {
            show_reclaimable: false,
            ..Settings::default()
        };
        assert!(!render_segment(&snapshot, &quiet, Style::Plain).contains('♻'));
    }

    #[test]
    fn details_list_volumes_and_alerts() {
        let snapshot = connected(
            vec![volume("/", 100, 13), volume("/data", 100, 80)],
            vec![alert("a", Severity::Critical)],
        );
        let text = render_details(&snapshot, &Settings::default(), "/tmp/b.sock");
        assert!(text.contains("state: connected"));
        assert!(text.contains(" 87% warning  /"));
        assert!(text.contains(" 20% ok       /data"));
        assert!(text.contains("[critical] a is filling up"));
        let first = text.find(" /  (").unwrap();
        let second = text.find("/data").unwrap();
        assert!(first < second, "sorted by pressure: {text}");
    }
}
