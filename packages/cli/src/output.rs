//! Human output helpers: aligned tables, sizes, ages.

use std::fmt::Write as _;

use bclean_model::{ActionSpec, ByteSize, Classification, Quadrant};

/// Column alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Left-aligned.
    Left,
    /// Right-aligned.
    Right,
}

/// A simple aligned text table.
#[derive(Debug, Clone)]
pub struct Table {
    headers: Vec<(String, Align)>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// Creates a table with `(header, alignment)` columns.
    #[must_use]
    pub fn new(headers: &[(&str, Align)]) -> Self {
        Self {
            headers: headers.iter().map(|(h, a)| ((*h).to_owned(), *a)).collect(),
            rows: Vec::new(),
        }
    }

    /// Adds a row (missing cells are blank).
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Whether the table has no rows.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Renders the table; the last column is never padded.
    #[must_use]
    pub fn render(&self) -> String {
        let n = self.headers.len();
        let mut widths: Vec<usize> = self
            .headers
            .iter()
            .map(|(h, _)| h.chars().count())
            .collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate().take(n) {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut out = String::new();
        let header: Vec<String> = self.headers.iter().map(|(h, _)| h.clone()).collect();
        for row in std::iter::once(&header).chain(self.rows.iter()) {
            let mut line = String::new();
            for (i, (_, align)) in self.headers.iter().enumerate() {
                let cell = row.get(i).map_or("", String::as_str);
                let pad = widths[i].saturating_sub(cell.chars().count());
                if i > 0 {
                    line.push_str("  ");
                }
                let last = i + 1 == n;
                match align {
                    Align::Right => {
                        line.push_str(&" ".repeat(pad));
                        line.push_str(cell);
                    }
                    Align::Left => {
                        line.push_str(cell);
                        if !last {
                            line.push_str(&" ".repeat(pad));
                        }
                    }
                }
            }
            let _ = writeln!(out, "{}", line.trim_end());
        }
        out
    }
}

/// Human-readable size.
#[must_use]
pub fn size(bytes: u64) -> String {
    ByteSize(bytes).to_string()
}

/// Human-readable signed size.
#[must_use]
pub fn signed_size(bytes: i64) -> String {
    if bytes < 0 {
        format!("-{}", size(bytes.unsigned_abs()))
    } else {
        size(bytes.unsigned_abs())
    }
}

/// Compact duration (`45s`, `12m`, `5h`, `3d`, `2y`).
#[must_use]
pub fn age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    match secs {
        s if s < MIN => format!("{s}s"),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < 365 * DAY => format!("{}d", s / DAY),
        s => format!("{}y", s / (365 * DAY)),
    }
}

/// Short timestamp relative to now (`5m ago`).
#[must_use]
pub fn ago(at: i64, now: i64) -> String {
    format!(
        "{} ago",
        age(u64::try_from(now.saturating_sub(at)).unwrap_or(0))
    )
}

/// Short action name.
#[must_use]
pub const fn action_name(action: &ActionSpec) -> &'static str {
    match action {
        ActionSpec::Delete => "DELETE",
        ActionSpec::Trash => "TRASH",
        ActionSpec::Command { .. } => "COMMAND",
        ActionSpec::Archive { .. } => "ARCHIVE",
        ActionSpec::Move { .. } => "MOVE",
        ActionSpec::Report => "REPORT",
    }
}

/// Short quadrant label.
#[must_use]
pub const fn quadrant(class: &Classification) -> &'static str {
    match class.quadrant() {
        Quadrant::EphemeralStale => "eph/stale",
        Quadrant::EphemeralFresh => "eph/fresh",
        Quadrant::DurableStale => "dur/stale",
        Quadrant::DurableFresh => "dur/fresh",
    }
}

/// Prints JSON to stdout.
///
/// # Errors
///
/// * If serialization fails
pub fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_aligned() {
        let mut t = Table::new(&[("SIZE", Align::Right), ("PATH", Align::Left)]);
        t.row(vec!["1.0 KiB".into(), "/a".into()]);
        t.row(vec!["10 B".into(), "/bb".into()]);
        assert_eq!(t.render(), "   SIZE  PATH\n1.0 KiB  /a\n   10 B  /bb\n");
        assert_eq!(age(90), "1m");
        assert_eq!(age(3 * 86_400), "3d");
    }
}
