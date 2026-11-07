//! Classification results, monitoring snapshots, and alert payloads.

use serde::{Deserialize, Serialize};

use crate::{ActionSpec, EphemeralSpec, SignalKind};

/// Detection confidence.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Confidence {
    /// Heuristic guess (for example "gitignored").
    Low,
    /// Likely correct.
    Medium,
    /// Anchored detection (marker files, `CACHEDIR.TAG`).
    #[default]
    High,
}

impl Confidence {
    /// Multiplier applied to the cleanup score.
    #[must_use]
    pub const fn weight(self) -> f64 {
        match self {
            Self::Low => 0.25,
            Self::Medium => 0.6,
            Self::High => 1.0,
        }
    }
}

/// Alert and policy severity.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Severity {
    /// Informational.
    Info,
    /// Needs attention.
    #[default]
    Warning,
    /// Urgent.
    Critical,
}

/// One staleness signal's reading for an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalReading {
    /// Which provider produced it.
    pub kind: SignalKind,
    /// Most recent activity seen by this provider (Unix seconds), if any.
    pub last_activity: Option<i64>,
    /// Provider forces staleness regardless of time (for example "branch merged").
    #[serde(default)]
    pub forces_stale: bool,
    /// Human-readable detail.
    pub detail: String,
}

/// Staleness verdict for an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Staleness {
    /// Whether the item is considered stale.
    pub stale: bool,
    /// Most recent activity across all signals (Unix seconds).
    pub last_activity: Option<i64>,
    /// Seconds since `last_activity` at evaluation time.
    pub idle_secs: Option<u64>,
    /// Idle threshold that was applied, in seconds.
    pub threshold_secs: u64,
    /// Individual signal readings.
    pub signals: Vec<SignalReading>,
}

/// Position in the ephemeral × stale matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Quadrant {
    /// Regenerable and unused: prime auto-clean candidates.
    EphemeralStale,
    /// Regenerable but in use: clean under pressure.
    EphemeralFresh,
    /// Not regenerable and unused: review suggestions.
    DurableStale,
    /// Not regenerable and in use: leave alone.
    DurableFresh,
}

/// A single explanation line for `bclean explain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    /// Stable machine code (`"RULE_MATCH"`, `"ANCHOR_FOUND"`, `"SIGNAL"`, ...).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl Reason {
    /// Creates a reason.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The classification of one filesystem entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Classification {
    /// Matching rule id, if any.
    pub rule_id: Option<String>,
    /// Rule category, if any.
    pub category: Option<String>,
    /// Ephemeral classification (`None` = durable).
    pub ephemeral: Option<EphemeralSpec>,
    /// Detection confidence.
    pub confidence: Confidence,
    /// Staleness verdict.
    pub staleness: Staleness,
    /// Reclaimable bytes (allocated size, or apparent if configured).
    pub size: u64,
    /// Cleanup score: higher = better candidate.
    pub score: f64,
    /// Action to perform.
    pub action: ActionSpec,
    /// Eligible for automatic cleaning.
    pub auto: bool,
    /// Explanation trail.
    pub reasons: Vec<Reason>,
}

impl Classification {
    /// Worth offering for cleanup: ephemeral or stale, actionable (not `REPORT`), and scored.
    #[must_use]
    pub fn is_reclaimable(&self) -> bool {
        (self.ephemeral.is_some() || self.staleness.stale)
            && !matches!(self.action, ActionSpec::Report)
            && self.score > 0.0
    }

    /// Position in the ephemeral × stale matrix.
    #[must_use]
    pub const fn quadrant(&self) -> Quadrant {
        match (self.ephemeral.is_some(), self.staleness.stale) {
            (true, true) => Quadrant::EphemeralStale,
            (true, false) => Quadrant::EphemeralFresh,
            (false, true) => Quadrant::DurableStale,
            (false, false) => Quadrant::DurableFresh,
        }
    }
}

/// Usage of one mounted volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStats {
    /// Mount point.
    pub mount_point: String,
    /// Filesystem type (`apfs`, `ext4`, `ntfs`, ...).
    pub fs_type: String,
    /// Total capacity in bytes.
    pub total: u64,
    /// Free bytes (including root-reserved).
    pub free: u64,
    /// Bytes available to the current user.
    pub available: u64,
    /// Additional purgeable bytes the OS can reclaim on demand (macOS), if known.
    pub purgeable: Option<u64>,
    /// Sample time (Unix seconds).
    pub at: i64,
}

impl VolumeStats {
    /// Used bytes.
    #[must_use]
    pub const fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Fraction used, `0.0..=1.0`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn used_fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.used() as f64 / self.total as f64
        }
    }
}

/// An alert raised by a policy or monitor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertEvent {
    /// Stable dedupe key (policy id + volume).
    pub key: String,
    /// Policy that raised it, if any.
    pub policy_id: Option<String>,
    /// Severity.
    pub severity: Severity,
    /// Short title.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Related volume, if any.
    pub volume: Option<VolumeStats>,
    /// Raise time (Unix seconds).
    pub at: i64,
}
