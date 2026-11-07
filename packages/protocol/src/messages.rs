//! Request, response, and event messages.

use std::path::PathBuf;

use bclean_model::{AlertEvent, Classification, DashboardSummary, VolumeStats};
use serde::{Deserialize, Serialize};

/// Frames sent by clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientFrame {
    /// First frame on every connection.
    Hello(ClientHello),
    /// A request expecting exactly one [`ServerFrame::Response`] with the same id.
    Request {
        /// Client-chosen correlation id.
        id: u64,
        /// The request.
        request: Request,
    },
}

/// Frames sent by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServerFrame {
    /// Reply to [`ClientFrame::Hello`].
    Hello(ServerHello),
    /// Reply to a request.
    Response {
        /// Correlation id of the request.
        id: u64,
        /// Result.
        result: Result<Response, RemoteError>,
    },
    /// Unsolicited event for subscribed topics.
    Event(Event),
}

/// Client handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    /// Client's [`crate::PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Client package version.
    pub client_version: String,
    /// Client executable fingerprint ([`crate::executable_build_id`]), if it cares about
    /// running the same build as the daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

/// Daemon handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    /// Daemon's [`crate::PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Daemon package version.
    pub daemon_version: String,
    /// Daemon process id.
    pub pid: u32,
    /// Daemon executable fingerprint ([`crate::executable_build_id`]); absent on older daemons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
}

/// Error returned by the daemon for a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RemoteError {
    /// Stable machine code (`"NOT_FOUND"`, `"INVALID"`, `"INTERNAL"`, ...).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl RemoteError {
    /// Creates an error.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Event subscription topics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Topic {
    /// Scan progress and tree updates.
    Scans,
    /// Volume usage samples.
    Volumes,
    /// Raised alerts.
    Alerts,
    /// Action execution progress.
    Actions,
    /// Policy firings and plans.
    Policies,
}

/// Requests a client can make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Request {
    /// Daemon status.
    Status,
    /// Current volume usage.
    Volumes,
    /// Active alerts.
    Alerts,
    /// Acknowledge (dismiss) an active alert.
    AckAlert {
        /// Alert key.
        key: String,
    },
    /// Start (or join) a scan of `root`. With `wait`, reply after it completes.
    Scan {
        /// Absolute root path.
        root: PathBuf,
        /// Rescan even if an up-to-date snapshot exists.
        force: bool,
        /// Reply only when the scan finishes.
        wait: bool,
    },
    /// Latest persisted snapshot for `root`.
    Snapshot {
        /// Absolute root path.
        root: PathBuf,
    },
    /// Rescan one directory (for example to expand a summarized directory).
    Rescan {
        /// Absolute directory path.
        path: PathBuf,
        /// Materialize children of summarized directories.
        expand: bool,
    },
    /// Classified entries under `root`.
    Classifications {
        /// Absolute root path.
        root: PathBuf,
    },
    /// Classify and explain one path.
    Explain {
        /// Absolute path.
        path: PathBuf,
    },
    /// Build a clean plan for the given paths (or for policy-style selection when empty).
    Plan {
        /// Paths to include.
        paths: Vec<PathBuf>,
    },
    /// Execute a plan previously returned by [`Request::Plan`].
    Execute {
        /// Opaque plan as returned by the daemon (JSON-encoded `bclean_actions::Plan`).
        plan: serde_json::Value,
        /// Dry run only.
        dry_run: bool,
    },
    /// Undo a journaled action.
    Undo {
        /// Journal entry id.
        id: String,
    },
    /// Recent action journal entries.
    Journal {
        /// Maximum entries.
        limit: usize,
    },
    /// Volume usage history.
    VolumeHistory {
        /// Mount point.
        mount_point: String,
        /// Unix seconds lower bound.
        since: i64,
    },
    /// Directories that grew the most under `root` since `since`.
    TopGrowth {
        /// Absolute root path.
        root: PathBuf,
        /// Unix seconds lower bound.
        since: i64,
        /// Maximum entries.
        limit: usize,
    },
    /// Dashboard summary over the index roots plus `roots` (for example the root a client is
    /// browsing).
    Dashboard {
        /// Extra roots to include, if loaded.
        #[serde(default)]
        roots: Vec<PathBuf>,
    },
    /// Subscribe this connection to event topics.
    Subscribe {
        /// Topics.
        topics: Vec<Topic>,
    },
    /// Reload configuration from disk.
    ReloadConfig,
    /// Ask the daemon to exit.
    Shutdown,
}

/// State of an indexed root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootStatus {
    /// Absolute root path.
    pub root: PathBuf,
    /// Currently scanning.
    pub scanning: bool,
    /// Progress of the current scan, if any.
    pub progress: Option<ScanProgressInfo>,
    /// Snapshot generation (increments on every tree update).
    pub generation: u64,
    /// Last completed scan (Unix seconds).
    pub scanned_at: Option<i64>,
    /// Live nodes in the tree.
    pub nodes: u64,
    /// Total allocated bytes.
    pub total_allocated: u64,
    /// Full scans completed since the daemon started. A steadily climbing count while the
    /// filesystem is quiet means change handling is escalating to rescans.
    #[serde(default)]
    pub full_scans: u64,
    /// Incremental (watcher-driven) refreshes applied since the daemon started.
    #[serde(default)]
    pub refreshes: u64,
}

/// Scan progress counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ScanProgressInfo {
    /// Directories read.
    pub dirs: u64,
    /// Files seen.
    pub files: u64,
    /// Allocated bytes seen.
    pub bytes: u64,
    /// Errors encountered.
    pub errors: u64,
}

/// Daemon status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    /// Process id.
    pub pid: u32,
    /// Package version.
    pub version: String,
    /// Start time (Unix seconds).
    pub started_at: i64,
    /// Indexed roots.
    pub roots: Vec<RootStatus>,
    /// Enabled policies.
    pub policies: usize,
    /// Active (unacknowledged) alerts.
    pub active_alerts: usize,
    /// Name of the active change-watching backend per root.
    pub watchers: Vec<String>,
}

/// Reference to a snapshot file on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    /// Absolute root path.
    pub root: PathBuf,
    /// Snapshot file path (load with `bclean_index`).
    pub path: PathBuf,
    /// Generation.
    pub generation: u64,
}

/// A classified entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifiedPath {
    /// Absolute path.
    pub path: PathBuf,
    /// Classification.
    pub classification: Classification,
}

/// Growth of one directory over a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthEntry {
    /// Directory path.
    pub path: PathBuf,
    /// Size at the start of the window.
    pub from: u64,
    /// Size at the end of the window.
    pub to: u64,
}

/// Successful responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Response {
    /// Generic acknowledgement.
    Ok,
    /// Reply to [`Request::Status`].
    Status(DaemonStatus),
    /// Reply to [`Request::Volumes`] and [`Request::VolumeHistory`].
    Volumes {
        /// Volume samples.
        volumes: Vec<VolumeStats>,
    },
    /// Reply to [`Request::Alerts`].
    Alerts {
        /// Active alerts.
        alerts: Vec<AlertEvent>,
    },
    /// Reply to [`Request::Dashboard`].
    Dashboard(Box<DashboardSummary>),
    /// Reply to [`Request::Scan`], [`Request::Snapshot`], and [`Request::Rescan`] without
    /// `expand`.
    Snapshot(SnapshotRef),
    /// Reply to [`Request::Rescan`] with `expand`: the expanded directory alone in its own
    /// file, sent before the root's snapshot is saved, so a client can graft it right away
    /// instead of reloading the whole root. The root's `TREE_UPDATED` follows. The client
    /// deletes `subtree.path` once it has read it.
    Expanded {
        /// The expanded directory's subtree (`root` is the directory, `generation` the
        /// containing root's).
        subtree: SnapshotRef,
    },
    /// Reply to [`Request::Classifications`].
    Classifications {
        /// Classified entries.
        items: Vec<ClassifiedPath>,
    },
    /// Reply to [`Request::Explain`].
    Explain(ClassifiedPath),
    /// Reply to [`Request::Plan`] (JSON-encoded `bclean_actions::Plan`).
    Plan {
        /// Plan.
        plan: serde_json::Value,
    },
    /// Reply to [`Request::Execute`], [`Request::Undo`], and [`Request::Journal`]
    /// (JSON-encoded `bclean_actions` report / journal types).
    Actions {
        /// Report or journal entries.
        value: serde_json::Value,
    },
    /// Reply to [`Request::TopGrowth`].
    Growth {
        /// Growth entries, largest first.
        entries: Vec<GrowthEntry>,
    },
}

/// Events pushed to subscribers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
    /// Scan progress for a root.
    ScanProgress {
        /// Root path.
        root: PathBuf,
        /// Counters.
        progress: ScanProgressInfo,
    },
    /// A root's tree changed; reload the snapshot.
    TreeUpdated(SnapshotRef),
    /// New volume samples.
    Volumes {
        /// Samples.
        volumes: Vec<VolumeStats>,
    },
    /// An alert was raised.
    Alert(AlertEvent),
    /// Action progress (JSON-encoded `bclean_actions` progress).
    ActionProgress {
        /// Progress payload.
        value: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_serialize_with_tags() {
        let frame = ClientFrame::Request {
            id: 7,
            request: Request::Scan {
                root: PathBuf::from("/tmp"),
                force: false,
                wait: true,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"REQUEST\""));
        assert!(json.contains("\"kind\":\"SCAN\""));
        let back: ClientFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);

        let reply = ServerFrame::Response {
            id: 7,
            result: Err(RemoteError::new("NOT_FOUND", "no such root")),
        };
        let back: ServerFrame =
            serde_json::from_str(&serde_json::to_string(&reply).unwrap()).unwrap();
        assert_eq!(back, reply);
    }
}
