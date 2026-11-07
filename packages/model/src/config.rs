//! Configuration schema. Every section has defaults so partial layers compose cleanly.
//!
//! Rules, policies, and alerts are keyed maps (`[rules.cargo_target]`) rather than arrays so
//! that later configuration layers can override individual fields of earlier ones, including
//! built-in catalog rules.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{ByteSize, Confidence, DurationSpec, Severity, Threshold};

/// Root configuration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Scanner behaviour.
    pub scan: ScanConfig,
    /// Classification and staleness defaults.
    pub classify: ClassifyConfig,
    /// Built-in catalog toggles.
    pub catalog: CatalogConfig,
    /// Paths that must never be touched by any action.
    pub protect: ProtectConfig,
    /// Rules keyed by id.
    pub rules: BTreeMap<String, RuleSpec>,
    /// Auto-clean and alarm policies keyed by id.
    pub policies: BTreeMap<String, PolicySpec>,
    /// Alert sinks keyed by id.
    pub alerts: BTreeMap<String, AlertSpec>,
    /// Disk monitoring.
    pub monitor: MonitorConfig,
    /// Daemon lifecycle.
    pub daemon: DaemonConfig,
    /// Interactive UI defaults.
    pub tui: TuiConfig,
}

/// Size measure used for display and accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SizeModeSpec {
    /// On-disk allocated bytes.
    #[default]
    Allocated,
    /// Logical file sizes.
    Apparent,
}

/// Scanner backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ScanBackend {
    /// Fastest backend available on this platform.
    #[default]
    Auto,
    /// Portable `std::fs` traversal.
    Portable,
    /// Native bulk-metadata syscalls (`getattrlistbulk`, `statx`, `NtQueryDirectoryFile`).
    Native,
    /// NTFS master file table reader (Windows, elevated only).
    Mft,
}

/// Scanner behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Do not descend into other filesystems.
    pub one_filesystem: bool,
    /// Size measure.
    pub size_mode: SizeModeSpec,
    /// Worker threads (`None` = available parallelism).
    pub threads: Option<usize>,
    /// Glob patterns excluded from scanning entirely.
    pub exclude: Vec<String>,
    /// Count matched ephemeral directories without materializing their children.
    pub summarize_ephemeral: bool,
    /// Backend selection.
    pub backend: ScanBackend,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            one_filesystem: true,
            size_mode: SizeModeSpec::Allocated,
            threads: None,
            exclude: Vec::new(),
            summarize_ephemeral: true,
            backend: ScanBackend::Auto,
        }
    }
}

/// Staleness signal providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SignalKind {
    /// Newest mtime/atime/ctime in the subtree.
    FsTimes,
    /// Activity of the owning project (git commits, reflog, source mtimes).
    ProjectActivity,
    /// OS last-used metadata (Spotlight, Windows last access).
    OsLastUsed,
    /// Git/worktree state (merged or deleted branches, orphaned worktrees).
    GitState,
}

impl SignalKind {
    /// All signal kinds.
    pub const ALL: [Self; 4] = [
        Self::FsTimes,
        Self::ProjectActivity,
        Self::OsLastUsed,
        Self::GitState,
    ];
}

/// Classification and staleness defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClassifyConfig {
    /// Idle time after which an ephemeral item is stale (unless its rule overrides it).
    pub stale_after: DurationSpec,
    /// Idle time after which a durable (non-ephemeral) item is stale.
    pub durable_stale_after: DurationSpec,
    /// Roots where durable-stale suggestions are produced (for example `~/Downloads`).
    pub durable_roots: Vec<String>,
    /// Minimum size for durable-stale suggestions.
    pub durable_min_size: ByteSize,
    /// Enabled signal providers.
    pub signals: Vec<SignalKind>,
    /// Treat gitignored directories inside repositories as low-confidence ephemeral.
    pub gitignored_heuristic: bool,
    /// Honour `CACHEDIR.TAG` markers as high-confidence ephemeral.
    pub cachedir_tag: bool,
}

impl Default for ClassifyConfig {
    fn default() -> Self {
        Self {
            stale_after: DurationSpec::days(30),
            durable_stale_after: DurationSpec::days(180),
            durable_roots: vec!["~/Downloads".to_owned()],
            durable_min_size: ByteSize(100 << 20),
            signals: SignalKind::ALL.to_vec(),
            gitignored_heuristic: true,
            cachedir_tag: true,
        }
    }
}

/// Built-in catalog toggles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogConfig {
    /// Load the built-in catalog at all.
    pub enabled: bool,
    /// Built-in categories to disable (for example `"docker"`).
    pub disable_categories: Vec<String>,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disable_categories: Vec::new(),
        }
    }
}

/// Paths that must never be touched by any action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtectConfig {
    /// Glob patterns (with `~` expansion) that actions refuse to touch.
    pub paths: Vec<String>,
    /// Manual cleans above this total require explicit confirmation.
    pub confirm_above: ByteSize,
}

impl Default for ProtectConfig {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            confirm_above: ByteSize(10 << 30),
        }
    }
}

/// Target operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Os {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
    /// Windows.
    Windows,
}

impl Os {
    /// The operating system this binary was compiled for, if supported.
    #[must_use]
    pub const fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else {
            None
        }
    }
}

/// What kind of entry a rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MatchKind {
    /// Directories only.
    #[default]
    Dir,
    /// Files only.
    File,
    /// Any entry.
    Any,
}

/// Conditions that select entries for a rule. All non-empty conditions must hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MatchSpec {
    /// Entry kind.
    pub kind: MatchKind,
    /// Basename globs (`"target"`, `"*.log"`); any may match.
    pub name: Vec<String>,
    /// Full-path globs with `~` and env expansion; any may match.
    pub path: Vec<String>,
    /// Globs of which at least one must exist next to the entry (anchors, e.g. `Cargo.toml`).
    pub sibling_any: Vec<String>,
    /// Globs that must all exist next to the entry.
    pub sibling_all: Vec<String>,
    /// Globs of which at least one must exist inside the directory (markers, e.g. `CACHEDIR.TAG`).
    pub contains_any: Vec<String>,
    /// Path globs under which the rule never matches.
    pub not_under: Vec<String>,
    /// Restrict to these operating systems (empty = all).
    pub os: Vec<Os>,
    /// Require (or forbid) the entry to be gitignored.
    pub git_ignored: Option<bool>,
}

/// What kind of regenerable content an ephemeral item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EphemeralKind {
    /// Compiler/build output (`target/`, `dist/`, `DerivedData`).
    BuildArtifact,
    /// Installed project dependencies (`node_modules`, `.venv`).
    Dependencies,
    /// Package manager download caches.
    PackageCache,
    /// Tool caches (compilers, linters, IDE indexes).
    ToolCache,
    /// Browser caches.
    BrowserCache,
    /// Log files.
    Log,
    /// Temporary files.
    Temp,
    /// Crash dumps and diagnostic reports.
    CrashDump,
    /// Container images, layers, and volumes.
    Container,
    /// Virtual machine images.
    VmImage,
    /// Device simulators and runtimes.
    Simulator,
    /// Trash/recycle bin contents.
    Trash,
    /// Anything else regenerable.
    Other,
}

/// How costly it is to regenerate an ephemeral item.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RegenCost {
    /// Nothing is lost (temp files, trash).
    Free,
    /// Quick local regeneration.
    Cheap,
    /// Requires network downloads.
    #[default]
    Network,
    /// Slow or heavyweight regeneration (full rebuilds, simulator runtimes).
    Expensive,
}

impl RegenCost {
    /// Divisor applied to the cleanup score.
    #[must_use]
    pub const fn weight(self) -> f64 {
        match self {
            Self::Free => 0.5,
            Self::Cheap => 1.0,
            Self::Network => 2.0,
            Self::Expensive => 4.0,
        }
    }
}

/// Ephemeral classification attached to a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EphemeralSpec {
    /// Content kind.
    pub kind: EphemeralKind,
    /// Regeneration cost.
    #[serde(default)]
    pub regen_cost: RegenCost,
}

/// Per-rule staleness overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StaleSpec {
    /// Idle time after which matches are stale.
    pub after: Option<DurationSpec>,
    /// Signals to consult (defaults to the global set).
    pub signals: Option<Vec<SignalKind>>,
    /// Treat the match as stale when its owning project has been idle this long.
    pub project_idle: Option<DurationSpec>,
}

/// What to do if a command action fails or its tool is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FallbackAction {
    /// Give up and report the failure.
    #[default]
    None,
    /// Delete the target.
    Delete,
    /// Move the target to the OS trash.
    Trash,
}

/// How a cleanup is carried out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionSpec {
    /// Permanently delete (frees space immediately).
    Delete,
    /// Move to the OS trash (reversible; does not free space until emptied).
    #[default]
    Trash,
    /// Run the tool's own cleaner. `{path}`, `{parent}`, and `{name}` are substituted.
    Command {
        /// Command line, run via the platform shell.
        run: String,
        /// Working directory (templated); defaults to `{parent}`.
        #[serde(default)]
        cwd: Option<String>,
        /// What to do if the command fails or is not installed.
        #[serde(default)]
        fallback: FallbackAction,
    },
    /// Compress into an archive at `dest` (templated), then delete the original.
    Archive {
        /// Destination directory.
        dest: String,
    },
    /// Move to another location (for example a larger volume).
    Move {
        /// Destination directory.
        dest: String,
    },
    /// Report only; never acted on.
    Report,
}

impl ActionSpec {
    /// Whether the action can be undone from the journal.
    #[must_use]
    pub const fn reversible(&self) -> bool {
        matches!(self, Self::Trash | Self::Move { .. } | Self::Archive { .. })
    }
}

/// A classification rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleSpec {
    /// Disabled rules never match.
    pub enabled: bool,
    /// Human description.
    pub description: Option<String>,
    /// Grouping category (`"rust"`, `"node"`, `"xcode"`, ...).
    pub category: Option<String>,
    /// Match conditions.
    #[serde(rename = "match")]
    pub matcher: MatchSpec,
    /// Ephemeral classification; `None` marks matches as durable.
    pub ephemeral: Option<EphemeralSpec>,
    /// Staleness overrides.
    pub stale: StaleSpec,
    /// Cleanup action.
    pub action: ActionSpec,
    /// Eligible for automatic cleaning by policies.
    pub auto: bool,
    /// Higher priority wins when several rules match the same entry.
    pub priority: i32,
    /// Detection confidence.
    pub confidence: Confidence,
}

impl Default for RuleSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            description: None,
            category: None,
            matcher: MatchSpec::default(),
            ephemeral: None,
            stale: StaleSpec::default(),
            action: ActionSpec::Trash,
            auto: false,
            priority: 0,
            confidence: Confidence::High,
        }
    }
}

/// When a policy fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TriggerSpec {
    /// Free space on a volume drops below a threshold.
    FreeBelow {
        /// Volume mount point or any path on it; `"*"` = every monitored volume.
        volume: String,
        /// Threshold.
        threshold: Threshold,
    },
    /// Used space on a volume rises above a threshold.
    UsedAbove {
        /// Volume mount point or any path on it; `"*"` = every monitored volume.
        volume: String,
        /// Threshold.
        threshold: Threshold,
    },
    /// The volume is predicted to fill up within a window, based on recent growth.
    TimeToFull {
        /// Volume mount point or any path on it; `"*"` = every monitored volume.
        volume: String,
        /// Prediction window.
        within: DurationSpec,
    },
    /// A directory grows by more than `by` within `within`.
    Growth {
        /// Directory path (with `~` expansion).
        path: String,
        /// Growth amount.
        by: ByteSize,
        /// Window.
        within: DurationSpec,
    },
    /// Cron schedule (`"0 3 * * *"`).
    Schedule {
        /// Cron expression.
        cron: String,
    },
}

/// Whether a policy acts or only reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PolicyMode {
    /// Send alerts only (an alarm).
    NotifyOnly,
    /// Plan and log what would be cleaned, without acting.
    #[default]
    Shadow,
    /// Clean for real.
    Enforce,
}

/// Ordering of auto-clean candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SelectOrder {
    /// Highest score first.
    #[default]
    Score,
    /// Largest first.
    Size,
    /// Longest idle first.
    Oldest,
}

/// Which classified items a policy may clean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SelectSpec {
    /// Require ephemeral (`true`) or durable (`false`) items.
    pub ephemeral: Option<bool>,
    /// Require stale (`true`) or fresh (`false`) items.
    pub stale: Option<bool>,
    /// Only rules marked `auto = true`.
    pub auto_only: bool,
    /// Restrict to these rule ids (empty = any).
    pub rules: Vec<String>,
    /// Restrict to these categories (empty = any).
    pub categories: Vec<String>,
    /// Ignore items smaller than this.
    pub min_size: ByteSize,
    /// Candidate ordering.
    pub order: SelectOrder,
    /// Restrict to items under these roots (empty = any).
    pub roots: Vec<String>,
}

impl Default for SelectSpec {
    fn default() -> Self {
        Self {
            ephemeral: Some(true),
            stale: None,
            auto_only: true,
            rules: Vec::new(),
            categories: Vec::new(),
            min_size: ByteSize(1 << 20),
            order: SelectOrder::Score,
            roots: Vec::new(),
        }
    }
}

/// Limits on a single policy run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BudgetSpec {
    /// Maximum bytes to reclaim per run.
    pub max_bytes: Option<ByteSize>,
    /// Maximum items to act on per run.
    pub max_items: Option<usize>,
}

/// Environmental conditions required before a policy acts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConditionSpec {
    /// Skip while running on battery.
    pub ac_power_only: bool,
    /// Require the user to be idle this long.
    pub idle_for: Option<DurationSpec>,
}

/// Stop condition for a cleaning run.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TargetSpec {
    /// Stop once free space is at least this.
    pub free_above: Threshold,
}

/// An auto-clean or alarm policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicySpec {
    /// Disabled policies never fire.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Human description.
    #[serde(default)]
    pub description: Option<String>,
    /// When the policy fires.
    pub trigger: TriggerSpec,
    /// When to stop cleaning (`None` = clean every selected candidate).
    #[serde(default)]
    pub target: Option<TargetSpec>,
    /// Candidate selection.
    #[serde(default)]
    pub select: SelectSpec,
    /// Act, shadow, or notify only.
    #[serde(default)]
    pub mode: PolicyMode,
    /// Alert ids to notify.
    #[serde(default)]
    pub notify: Vec<String>,
    /// Severity of alerts raised by this policy.
    #[serde(default)]
    pub severity: Severity,
    /// Per-run limits.
    #[serde(default)]
    pub budget: BudgetSpec,
    /// Environmental conditions.
    #[serde(default)]
    pub conditions: ConditionSpec,
    /// Minimum time between firings.
    #[serde(default = "default_cooldown")]
    pub cooldown: DurationSpec,
}

const fn default_true() -> bool {
    true
}

const fn default_cooldown() -> DurationSpec {
    DurationSpec(std::time::Duration::from_hours(1))
}

/// Where an alert is delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AlertSink {
    /// Native desktop notification.
    Desktop,
    /// Run a command with the alert as JSON on stdin.
    Command {
        /// Command line, run via the platform shell.
        run: String,
    },
    /// HTTP request with the alert as a JSON body.
    Webhook {
        /// Target URL.
        url: String,
        /// HTTP method (default `POST`).
        #[serde(default)]
        method: Option<String>,
        /// Extra headers.
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    /// Surface in bmux via the bclean bmux plugin.
    Bmux,
}

/// An alert sink with delivery settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertSpec {
    /// Disabled sinks never deliver.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum severity delivered.
    #[serde(default)]
    pub min_severity: Severity,
    /// Suppress repeats of the same alert within this window.
    #[serde(default = "default_debounce")]
    pub debounce: DurationSpec,
    /// Destination.
    #[serde(flatten)]
    pub sink: AlertSink,
}

const fn default_debounce() -> DurationSpec {
    DurationSpec(std::time::Duration::from_mins(15))
}

/// Disk monitoring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Volumes to monitor (mount points or paths on them); `"*"` = all local volumes.
    pub volumes: Vec<String>,
    /// Volume usage polling interval.
    pub poll_interval: DurationSpec,
    /// Roots the daemon indexes and keeps up to date.
    pub index_roots: Vec<String>,
    /// How often to record history rollups.
    pub history_interval: DurationSpec,
    /// How long to keep history.
    pub history_retention: DurationSpec,
    /// Directory depth recorded in history rollups.
    pub history_depth: usize,
    /// How long incremental index updates may stay unsaved before the snapshot is rewritten.
    /// Full scans and shutdown always save; the watch cursor is saved with the snapshot, so
    /// unsaved changes are replayed after a crash where the backend supports it.
    pub persist_interval: DurationSpec,
    /// Minimum time between re-measurements of one summarized directory (`node_modules`,
    /// `target`, ...) while changes keep arriving inside it, for example during a build.
    pub summary_refresh_interval: DurationSpec,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            volumes: vec!["*".to_owned()],
            poll_interval: DurationSpec(std::time::Duration::from_secs(30)),
            index_roots: vec!["~".to_owned()],
            history_interval: DurationSpec(std::time::Duration::from_secs(3_600)),
            history_retention: DurationSpec::days(90),
            history_depth: 4,
            persist_interval: DurationSpec(std::time::Duration::from_mins(5)),
            summary_refresh_interval: DurationSpec(std::time::Duration::from_mins(1)),
        }
    }
}

/// Daemon lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Start the daemon automatically when a client needs it.
    pub autostart: bool,
    /// Override the IPC socket / named pipe path.
    pub socket: Option<String>,
    /// Exit after this long with no clients and no enabled policies.
    pub idle_exit: Option<DurationSpec>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            socket: None,
            idle_exit: None,
        }
    }
}

/// Default sort in the interactive UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TuiSort {
    /// Largest first.
    #[default]
    Size,
    /// Name ascending.
    Name,
    /// Most items first.
    Items,
    /// Most recently modified first.
    Mtime,
    /// Highest cleanup score first.
    Score,
}

/// Interactive UI defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Default sort.
    pub sort: TuiSort,
    /// Show dotfiles.
    pub show_hidden: bool,
    /// Ask before executing a clean plan.
    pub confirm: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            sort: TuiSort::Size,
            show_hidden: true,
            confirm: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rules_policies_alerts() {
        let config: Config = toml::from_str(
            r#"
[scan]
exclude = ["/Volumes/*"]

[rules.cargo_target]
category = "rust"
match = { name = ["target"], sibling_any = ["Cargo.toml"] }
ephemeral = { kind = "BUILD_ARTIFACT", regen_cost = "CHEAP" }
stale = { project_idle = "30d" }
action = { kind = "COMMAND", run = "cargo clean", fallback = "DELETE" }
auto = true

[policies.low_disk]
trigger = { kind = "FREE_BELOW", volume = "/", threshold = "10%" }
target = { free_above = "20%" }
mode = "ENFORCE"
notify = ["desktop", "ntfy"]

[alerts.desktop]
kind = "DESKTOP"

[alerts.ntfy]
kind = "COMMAND"
run = "curl -sd @- ntfy.sh/disk"
min_severity = "WARNING"
"#,
        )
        .unwrap();
        let rule = &config.rules["cargo_target"];
        assert!(rule.auto);
        assert_eq!(rule.matcher.sibling_any, vec!["Cargo.toml"]);
        assert!(matches!(
            rule.action,
            ActionSpec::Command {
                fallback: FallbackAction::Delete,
                ..
            }
        ));
        let policy = &config.policies["low_disk"];
        assert_eq!(policy.mode, PolicyMode::Enforce);
        assert!(matches!(
            policy.trigger,
            TriggerSpec::FreeBelow {
                threshold: Threshold::Percent(_),
                ..
            }
        ));
        assert_eq!(config.alerts["ntfy"].min_severity, Severity::Warning);
        assert_eq!(config.alerts["desktop"].sink, AlertSink::Desktop);
        assert!(config.scan.one_filesystem);
    }
}
