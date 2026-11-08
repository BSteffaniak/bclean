//! Text formatting helpers (sizes, dates, ages, classification badges).

use bclean_model::{ActionSpec, ByteSize, Classification, EphemeralKind, Quadrant};
use bmux_tui::style::{Color, Modifier, Style};

/// Human-readable byte size (`"1.5 GiB"`).
#[must_use]
pub fn size(bytes: u64) -> String {
    ByteSize(bytes).to_string()
}

/// Thousands-separated integer (`"12,345"`).
#[must_use]
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Percentage of `part` in `whole` with one decimal (`"42.0%"`).
#[must_use]
#[allow(clippy::cast_precision_loss)] // display only
pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "  0.0%".to_owned();
    }
    format!("{:5.1}%", part as f64 * 100.0 / whole as f64)
}

/// Converts days since the Unix epoch to a civil `(year, month, day)` (proleptic Gregorian).
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // bounded ranges
    let (m, d) = (m as u32, d as u32);
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` (UTC) for a Unix timestamp, or `"-"` when unknown.
#[must_use]
pub fn date(unix_secs: i64) -> String {
    if unix_secs <= 0 {
        return "-".to_owned();
    }
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM` (UTC) for a Unix timestamp.
#[must_use]
pub fn datetime(unix_secs: i64) -> String {
    if unix_secs <= 0 {
        return "-".to_owned();
    }
    let secs = unix_secs.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}",
        date(unix_secs),
        secs / 3_600,
        (secs / 60) % 60
    )
}

/// Compact age (`"45s"`, `"3m"`, `"5h"`, `"12d"`, `"4mo"`, `"2y"`).
#[must_use]
pub fn age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..5_184_000 => format!("{}d", secs / 86_400),
        5_184_000..63_072_000 => format!("{}mo", secs / 2_592_000),
        _ => format!("{}y", secs / 31_536_000),
    }
}

/// Short label of an ephemeral kind.
#[must_use]
pub const fn kind_label(kind: EphemeralKind) -> &'static str {
    match kind {
        EphemeralKind::BuildArtifact => "build",
        EphemeralKind::Dependencies => "deps",
        EphemeralKind::PackageCache => "pkg-cache",
        EphemeralKind::ToolCache => "tool-cache",
        EphemeralKind::BrowserCache => "browser",
        EphemeralKind::Log => "log",
        EphemeralKind::Temp => "temp",
        EphemeralKind::CrashDump => "crash",
        EphemeralKind::Container => "container",
        EphemeralKind::VmImage => "vm",
        EphemeralKind::Simulator => "simulator",
        EphemeralKind::Trash => "trash",
        EphemeralKind::Other => "ephemeral",
    }
}

/// Compact badge text for a classification (`"build·90d"`, `"stale 1y"`, `"deps"`).
#[must_use]
pub fn badge(class: &Classification) -> String {
    let stale_age = class
        .staleness
        .stale
        .then(|| class.staleness.idle_secs.map(age))
        .flatten();
    match (&class.ephemeral, stale_age) {
        (Some(eph), Some(idle)) => format!("{}·{idle}", kind_label(eph.kind)),
        (Some(eph), None) => kind_label(eph.kind).to_owned(),
        (None, Some(idle)) => format!("stale {idle}"),
        (None, None) if class.staleness.stale => "stale".to_owned(),
        (None, None) => class
            .category
            .clone()
            .unwrap_or_else(|| "durable".to_owned()),
    }
}

/// Badge color by ephemeral × stale quadrant.
#[must_use]
pub const fn quadrant_style(quadrant: Quadrant) -> Style {
    match quadrant {
        Quadrant::EphemeralStale => Style::new()
            .fg(Color::BrightGreen)
            .add_modifier(Modifier::BOLD),
        Quadrant::EphemeralFresh => Style::new().fg(Color::Yellow),
        Quadrant::DurableStale => Style::new().fg(Color::BrightMagenta),
        Quadrant::DurableFresh => Style::new().fg(Color::BrightBlack),
    }
}

/// Human name of a quadrant.
#[must_use]
pub const fn quadrant_label(quadrant: Quadrant) -> &'static str {
    match quadrant {
        Quadrant::EphemeralStale => "ephemeral + stale (prime candidate)",
        Quadrant::EphemeralFresh => "ephemeral, in use (clean under pressure)",
        Quadrant::DurableStale => "durable + stale (review)",
        Quadrant::DurableFresh => "durable, in use (leave alone)",
    }
}

/// Short action description.
#[must_use]
pub fn action(spec: &ActionSpec) -> String {
    match spec {
        ActionSpec::Delete => "delete".to_owned(),
        ActionSpec::Trash => "trash".to_owned(),
        ActionSpec::Command { run, .. } => format!("run `{run}`"),
        ActionSpec::Archive { dest } => format!("archive → {dest}"),
        ActionSpec::Move { dest } => format!("move → {dest}"),
        ActionSpec::Trim { .. } => "trim (keep outputs)".to_owned(),
        ActionSpec::Report => "report only".to_owned(),
    }
}

/// The action's few-word label for a list row: the command itself, or the verb.
#[must_use]
pub fn action_label(spec: &ActionSpec) -> String {
    match spec {
        ActionSpec::Command { run, .. } => run.clone(),
        ActionSpec::Trim { .. } => "trim".to_owned(),
        ActionSpec::Archive { .. } => "archive".to_owned(),
        ActionSpec::Move { .. } => "move".to_owned(),
        other => action(other),
    }
}

/// What the action does to `path`, in full: the command with its working directory and
/// fallback, or what a trim keeps.
#[must_use]
#[allow(clippy::literal_string_with_formatting_args)] // `{path}`, ... are action placeholders
pub fn action_detail(spec: &ActionSpec, path: &std::path::Path) -> String {
    use bclean_model::FallbackAction;
    let fill = |template: &str| {
        let parent = path.parent().unwrap_or(path);
        template
            .replace("{path}", &path.display().to_string())
            .replace("{parent}", &parent.display().to_string())
            .replace(
                "{name}",
                &path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            )
    };
    match spec {
        ActionSpec::Delete => "delete permanently (frees the space now)".to_owned(),
        ActionSpec::Trash => "move to the trash (undoable; frees space once emptied)".to_owned(),
        ActionSpec::Command { run, cwd, fallback } => {
            let dir = fill(cwd.as_deref().unwrap_or("{parent}"));
            let fallback = match fallback {
                FallbackAction::None => "nothing else is done",
                FallbackAction::Delete => "it is deleted instead",
                FallbackAction::Trash => "it is moved to the trash instead",
            };
            format!("run `{}` in {dir}; if that fails, {fallback}", fill(run))
        }
        ActionSpec::Archive { dest } => format!("archive into {}, then delete", fill(dest)),
        ActionSpec::Move { dest } => format!("move into {}", fill(dest)),
        ActionSpec::Report => "report only; never acted on".to_owned(),
        ActionSpec::Trim { keep } => {
            let kept: Vec<String> = keep
                .iter()
                .map(|k| {
                    if k.executable {
                        format!("executables matching {}", k.glob)
                    } else {
                        k.glob.clone()
                    }
                })
                .collect();
            format!(
                "delete everything inside except {}",
                if kept.is_empty() {
                    "nothing".to_owned()
                } else {
                    kept.join(", ")
                }
            )
        }
    }
}

/// Score with sensible precision.
#[must_use]
pub fn score(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// Truncates `text` to `width` display cells from the left, keeping the end (`"…/deep/path"`).
#[must_use]
pub fn truncate_left(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let keep = width - 1;
    let mut out = String::from("…");
    out.extend(&chars[chars.len() - keep..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_and_percent() {
        assert_eq!(count(0), "0");
        assert_eq!(count(1_234), "1,234");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(percent(1, 4), " 25.0%");
        assert_eq!(percent(1, 0), "  0.0%");
    }

    #[test]
    fn formats_dates() {
        assert_eq!(date(0), "-");
        assert_eq!(date(86_400), "1970-01-02");
        assert_eq!(date(1_700_000_000), "2023-11-14");
        assert_eq!(datetime(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(date(951_782_400), "2000-02-29");
    }

    #[test]
    fn formats_ages() {
        assert_eq!(age(5), "5s");
        assert_eq!(age(120), "2m");
        assert_eq!(age(7_200), "2h");
        assert_eq!(age(86_400 * 45), "45d");
        assert_eq!(age(86_400 * 120), "4mo");
        assert_eq!(age(86_400 * 800), "2y");
    }

    #[test]
    fn truncates_from_left() {
        assert_eq!(truncate_left("abc", 5), "abc");
        assert_eq!(truncate_left("abcdef", 4), "…def");
        assert_eq!(truncate_left("abcdef", 0), "");
    }
}
