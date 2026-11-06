//! Human-friendly units used in configuration: byte sizes, durations, and thresholds.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Error parsing a unit string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} `{input}`: {reason}")]
pub struct UnitParseError {
    kind: &'static str,
    input: String,
    reason: &'static str,
}

impl UnitParseError {
    fn new(kind: &'static str, input: &str, reason: &'static str) -> Self {
        Self {
            kind,
            input: input.to_owned(),
            reason,
        }
    }
}

fn split_number(input: &str) -> (&str, &str) {
    let trimmed = input.trim();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(trimmed.len());
    (&trimmed[..end], trimmed[end..].trim())
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn scale(number: &str, multiplier: u64) -> Option<u64> {
    if let Ok(whole) = number.parse::<u64>() {
        return whole.checked_mul(multiplier);
    }
    // Fractional values ("1.5G") are rounded to the nearest unit; precision loss is fine here.
    let value: f64 = number.parse().ok()?;
    let scaled = value * multiplier as f64;
    (scaled.is_finite() && scaled >= 0.0 && scaled < u64::MAX as f64).then(|| scaled.round() as u64)
}

/// A byte count, written as an integer or a string like `"512"`, `"100M"`, `"1.5GiB"`.
///
/// Suffixes are case-insensitive. `K/M/G/T/P` and `KiB/MiB/...` are binary (1024-based);
/// `KB/MB/...` are decimal (1000-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// Returns the size in bytes.
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl FromStr for ByteSize {
    type Err = UnitParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (number, suffix) = split_number(input);
        if number.is_empty() {
            return Err(UnitParseError::new("byte size", input, "missing number"));
        }
        let multiplier: u64 = match suffix.to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "k" | "kib" => 1 << 10,
            "m" | "mib" => 1 << 20,
            "g" | "gib" => 1 << 30,
            "t" | "tib" => 1 << 40,
            "p" | "pib" => 1 << 50,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            "tb" => 1_000_000_000_000,
            "pb" => 1_000_000_000_000_000,
            _ => return Err(UnitParseError::new("byte size", input, "unknown suffix")),
        };
        scale(number, multiplier)
            .map(Self)
            .ok_or_else(|| UnitParseError::new("byte size", input, "out of range"))
    }
}

impl fmt::Display for ByteSize {
    #[allow(clippy::cast_precision_loss)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
        let mut value = self.0 as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < UNITS.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            write!(f, "{} B", self.0)
        } else {
            write!(f, "{value:.1} {}", UNITS[unit])
        }
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Int(v) => Ok(Self(v)),
            Raw::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// A duration, written as seconds or a string like `"90s"`, `"15m"`, `"12h"`, `"30d"`, `"2w"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DurationSpec(pub Duration);

impl DurationSpec {
    /// Creates a duration from whole days.
    #[must_use]
    pub const fn days(days: u64) -> Self {
        Self(Duration::from_secs(days * 86_400))
    }

    /// Returns the inner duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }

    /// Returns the duration in whole seconds.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl FromStr for DurationSpec {
    type Err = UnitParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let (number, suffix) = split_number(input);
        if number.is_empty() {
            return Err(UnitParseError::new("duration", input, "missing number"));
        }
        let multiplier: u64 = match suffix.to_ascii_lowercase().as_str() {
            "" | "s" | "sec" | "secs" => 1,
            "m" | "min" | "mins" => 60,
            "h" | "hr" | "hrs" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            "y" | "year" | "years" => 31_536_000,
            _ => return Err(UnitParseError::new("duration", input, "unknown suffix")),
        };
        scale(number, multiplier)
            .map(|secs| Self(Duration::from_secs(secs)))
            .ok_or_else(|| UnitParseError::new("duration", input, "out of range"))
    }
}

impl fmt::Display for DurationSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        for (unit, size) in [("w", 604_800), ("d", 86_400), ("h", 3_600), ("m", 60)] {
            if secs >= size && secs.is_multiple_of(size) {
                return write!(f, "{}{unit}", secs / size);
            }
        }
        write!(f, "{secs}s")
    }
}

impl Serialize for DurationSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for DurationSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Int(v) => Ok(Self(Duration::from_secs(v))),
            Raw::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

/// A capacity threshold: an absolute byte size (`"20G"`) or a percentage of the volume (`"10%"`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Threshold {
    /// Absolute byte count.
    Bytes(ByteSize),
    /// Percentage of total volume capacity, `0.0..=100.0`.
    Percent(f64),
}

impl Threshold {
    /// Resolves the threshold to bytes for a volume with `total` capacity.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn resolve(self, total: u64) -> u64 {
        match self {
            Self::Bytes(b) => b.0,
            Self::Percent(p) => ((total as f64) * (p.clamp(0.0, 100.0) / 100.0)) as u64,
        }
    }
}

impl FromStr for Threshold {
    type Err = UnitParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let trimmed = input.trim();
        if let Some(pct) = trimmed.strip_suffix('%') {
            let value: f64 = pct
                .trim()
                .parse()
                .map_err(|_| UnitParseError::new("threshold", input, "bad percentage"))?;
            if !(0.0..=100.0).contains(&value) {
                return Err(UnitParseError::new(
                    "threshold",
                    input,
                    "percentage out of range",
                ));
            }
            return Ok(Self::Percent(value));
        }
        trimmed.parse().map(Self::Bytes)
    }
}

impl<'de> Deserialize<'de> for Threshold {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Int(v) => Ok(Self::Bytes(ByteSize(v))),
            Raw::Str(s) => s.parse().map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byte_sizes() {
        assert_eq!("512".parse::<ByteSize>().unwrap().0, 512);
        assert_eq!("100M".parse::<ByteSize>().unwrap().0, 100 << 20);
        assert_eq!("1.5GiB".parse::<ByteSize>().unwrap().0, 3 << 29);
        assert_eq!("2 GB".parse::<ByteSize>().unwrap().0, 2_000_000_000);
        assert!("12Q".parse::<ByteSize>().is_err());
        assert!("G".parse::<ByteSize>().is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(
            "30d".parse::<DurationSpec>().unwrap(),
            DurationSpec::days(30)
        );
        assert_eq!("90".parse::<DurationSpec>().unwrap().as_secs(), 90);
        assert_eq!("2w".parse::<DurationSpec>().unwrap().to_string(), "2w");
        assert_eq!("36h".parse::<DurationSpec>().unwrap().to_string(), "36h");
    }

    #[test]
    fn parses_thresholds() {
        assert_eq!("10%".parse::<Threshold>().unwrap().resolve(1000), 100);
        assert_eq!("1K".parse::<Threshold>().unwrap().resolve(10), 1024);
        assert!("150%".parse::<Threshold>().is_err());
    }

    #[test]
    fn displays_sizes() {
        assert_eq!(ByteSize(1536).to_string(), "1.5 KiB");
        assert_eq!(ByteSize(12).to_string(), "12 B");
    }
}
