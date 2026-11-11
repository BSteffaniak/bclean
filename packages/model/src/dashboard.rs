//! The dashboard summary: one compact, precomputed view of disk usage and reclaimable data.
//!
//! The engine computes it from its in-memory index, volume history, and journal; clients (the
//! TUI, `bclean status`, the bmux plugin) render it without needing every classification.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{EphemeralKind, VolumeStats};

/// Everything the dashboard shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DashboardSummary {
    /// Unix seconds at which the summary was computed.
    pub generated_at: i64,
    /// Idle time after which items counted as unused for this summary, when overridden
    /// (`None`: each rule's own thresholds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_after_secs: Option<u64>,
    /// Monitored volumes, fullest first.
    pub volumes: Vec<VolumeSummary>,
    /// Indexed roots the summary covers.
    pub roots: Vec<RootSummary>,
    /// Reclaimable totals.
    pub reclaimable: ReclaimSummary,
    /// Reclaimable bytes per ephemeral kind (durable stale items under `None`), largest first.
    pub by_kind: Vec<KindBucket>,
    /// Reclaimable bytes per rule category (ecosystem: `rust`, `node`, ...), largest first.
    pub by_category: Vec<CategoryBucket>,
    /// Best cleanup candidates, by score.
    pub top_items: Vec<DashboardItem>,
    /// Projects holding the most reclaimable data.
    pub projects: Vec<ProjectSummary>,
    /// Directories that grew the most over the last day.
    pub growth_day: Vec<GrowthItem>,
    /// Directories that grew the most over the last week.
    pub growth_week: Vec<GrowthItem>,
    /// Recent cleaning and daemon health.
    pub activity: ActivitySummary,
}

/// One volume with its composition and trend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSummary {
    /// Latest usage sample.
    pub stats: VolumeStats,
    /// Bytes on this volume covered by indexed roots (each byte counted once).
    pub indexed: u64,
    /// Reclaimable bytes on this volume.
    pub reclaimable: u64,
    /// Part of `reclaimable` that is build output of projects still in use.
    #[serde(default)]
    pub active: u64,
    /// Used bytes over time, oldest first, evenly spaced over `trend_span_secs`.
    pub trend: Vec<u64>,
    /// Time covered by `trend`.
    pub trend_span_secs: u64,
    /// Recent growth of used space in bytes per day (negative when shrinking), when enough
    /// history exists.
    pub growth_per_day: Option<i64>,
    /// Estimated seconds until the volume is full at `growth_per_day`, when growing.
    pub full_in_secs: Option<u64>,
    /// Bytes used by this volume alone when it shares its capacity with other volumes (an
    /// APFS container: the macOS data volume, the Nix store, the system volume, ...).
    /// `stats.used()` then counts them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_used: Option<u64>,
}

impl VolumeSummary {
    /// Bytes used by this volume itself.
    #[must_use]
    pub fn volume_used(&self) -> u64 {
        let used = self.stats.used();
        self.own_used.map_or(used, |own| own.min(used))
    }

    /// Bytes used by other volumes sharing this one's capacity.
    #[must_use]
    pub fn other_volumes(&self) -> u64 {
        self.stats.used().saturating_sub(self.volume_used())
    }

    /// Indexed bytes, at most what the volume uses: copy-on-write clones and hard links make
    /// file sizes add up to more than the blocks they share.
    #[must_use]
    pub fn indexed_on_volume(&self) -> u64 {
        self.indexed.min(self.volume_used())
    }

    /// Reclaimable bytes, at most [`VolumeSummary::indexed_on_volume`].
    #[must_use]
    pub fn reclaimable_on_volume(&self) -> u64 {
        self.reclaimable.min(self.indexed_on_volume())
    }

    /// Reclaimable bytes that are not build output of projects in use.
    #[must_use]
    pub fn idle_reclaimable(&self) -> u64 {
        self.reclaimable_on_volume()
            .saturating_sub(self.active_on_volume())
    }

    /// Build output of projects in use, at most [`VolumeSummary::reclaimable_on_volume`].
    #[must_use]
    pub fn active_on_volume(&self) -> u64 {
        self.active.min(self.reclaimable_on_volume())
    }

    /// Bytes this volume uses outside every indexed root (system files, apps, other users,
    /// unindexed directories).
    #[must_use]
    pub fn outside_index(&self) -> u64 {
        self.volume_used().saturating_sub(self.indexed_on_volume())
    }

    /// Indexed bytes that are not reclaimable.
    #[must_use]
    pub fn indexed_kept(&self) -> u64 {
        self.indexed_on_volume()
            .saturating_sub(self.reclaimable_on_volume())
    }
}

/// An indexed root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootSummary {
    /// Absolute root path.
    pub path: PathBuf,
    /// Total allocated bytes.
    pub size: u64,
    /// Last completed scan (Unix seconds).
    pub scanned_at: Option<i64>,
    /// A scan is running.
    pub scanning: bool,
}

/// Reclaimable totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReclaimSummary {
    /// All reclaimable bytes (ephemeral or stale, actionable).
    pub total: u64,
    /// Reclaimable items.
    pub items: u64,
    /// Bytes an automatic clean would select right now (what "clean safe" plans).
    pub safe: u64,
    /// Items an automatic clean would select.
    pub safe_items: u64,
    /// Paths an automatic clean would select, largest first (what "clean safe" plans).
    pub safe_paths: Vec<PathBuf>,
    /// Ephemeral and stale.
    pub ephemeral_stale: u64,
    /// Ephemeral but recently used.
    pub ephemeral_fresh: u64,
    /// Durable but unused.
    pub durable_stale: u64,
    /// Build output of projects still in use (recently worked on, or with a process working
    /// in them): regenerable, but never selected automatically.
    #[serde(default)]
    pub active: u64,
}

impl ReclaimSummary {
    /// Reclaimable bytes that need a human decision (neither safe now nor in active use).
    #[must_use]
    pub const fn review(&self) -> u64 {
        self.total
            .saturating_sub(self.safe)
            .saturating_sub(self.active)
    }
}

/// Reclaimable bytes of one ephemeral kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindBucket {
    /// Kind (`None` = durable but stale).
    pub kind: Option<EphemeralKind>,
    /// Bytes.
    pub bytes: u64,
    /// Items.
    pub items: u64,
    /// Bytes an automatic clean would select.
    pub safe: u64,
}

/// Reclaimable bytes of one rule category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryBucket {
    /// Category (`None` = uncategorized).
    pub category: Option<String>,
    /// Bytes.
    pub bytes: u64,
    /// Items.
    pub items: u64,
    /// Bytes an automatic clean would select.
    pub safe: u64,
}

/// One cleanup candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardItem {
    /// Absolute path.
    pub path: PathBuf,
    /// Reclaimable bytes.
    pub size: u64,
    /// Rule category.
    pub category: Option<String>,
    /// Ephemeral kind (`None` = durable).
    pub kind: Option<EphemeralKind>,
    /// Seconds since last use, if known.
    pub idle_secs: Option<u64>,
    /// Stale.
    pub stale: bool,
    /// Cleanup score.
    pub score: f64,
    /// An automatic clean would select it.
    pub safe: bool,
    /// Build output of a project still in use (see [`ReclaimSummary::active`]).
    #[serde(default)]
    pub active: bool,
}

/// A project (directory with a project marker such as `.git` or `Cargo.toml`) and the
/// reclaimable data inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSummary {
    /// Project directory.
    pub path: PathBuf,
    /// Reclaimable bytes inside it.
    pub bytes: u64,
    /// Reclaimable items inside it.
    pub items: u64,
    /// Names of its largest reclaimable entries (`target`, `node_modules`, ...), largest first.
    pub entries: Vec<String>,
    /// Its reclaimable paths (what marking the project selects; never the project itself).
    pub paths: Vec<PathBuf>,
    /// Newest activity in the project (Unix seconds), if known.
    pub last_activity: Option<i64>,
}

/// Growth of one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthItem {
    /// Directory.
    pub path: PathBuf,
    /// Size at the start of the window.
    pub from: u64,
    /// Size now.
    pub to: u64,
}

impl GrowthItem {
    /// Bytes gained (negative when shrunk).
    #[must_use]
    pub fn delta(&self) -> i64 {
        i64::try_from(self.to).unwrap_or(i64::MAX) - i64::try_from(self.from).unwrap_or(i64::MAX)
    }
}

/// Recent cleaning and daemon health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ActivitySummary {
    /// Bytes freed by actions in the last 7 days (measured where available).
    pub freed_week: u64,
    /// Completed actions in the last 7 days.
    pub actions_week: u64,
    /// Most recent completed action.
    pub last_action: Option<LastAction>,
    /// Active alerts.
    pub alerts: u64,
    /// Message of the most severe active alert.
    pub top_alert: Option<String>,
    /// Full scans since the engine started, across roots.
    pub full_scans: u64,
    /// Incremental refreshes since the engine started, across roots.
    pub refreshes: u64,
    /// Active change-watching backends.
    pub watchers: Vec<String>,
}

/// The most recent completed action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastAction {
    /// Journal entry id (for undo).
    pub id: String,
    /// Unix seconds.
    pub at: i64,
    /// Target path.
    pub path: PathBuf,
    /// Bytes freed.
    pub freed: u64,
    /// Can be undone.
    pub reversible: bool,
}

/// Short human label for an ephemeral kind (`None` = durable but stale).
#[must_use]
pub const fn kind_label(kind: Option<EphemeralKind>) -> &'static str {
    match kind {
        Some(EphemeralKind::BuildArtifact) => "build output",
        Some(EphemeralKind::Dependencies) => "dependencies",
        Some(EphemeralKind::PackageCache) => "package cache",
        Some(EphemeralKind::ToolCache) => "tool cache",
        Some(EphemeralKind::BrowserCache) => "browser cache",
        Some(EphemeralKind::Log) => "logs",
        Some(EphemeralKind::Temp) => "temp",
        Some(EphemeralKind::CrashDump) => "crash dumps",
        Some(EphemeralKind::Container) => "containers",
        Some(EphemeralKind::VmImage) => "VM images",
        Some(EphemeralKind::Simulator) => "simulators",
        Some(EphemeralKind::Trash) => "trash",
        Some(EphemeralKind::Other) => "other",
        None => "unused (durable)",
    }
}
