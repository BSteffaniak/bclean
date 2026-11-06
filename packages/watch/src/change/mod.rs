//! Change sources: debounced batches of directories that need rescanning.
//!
//! A [`ChangeSource`] watches a set of roots with the best native backend and emits
//! [`ChangeBatch`]es over a channel. Each batch lists directories whose subtree must be
//! reconciled with the index (a consumer rescans each one, using directory mtimes to skip
//! unchanged subdirectories unless [`ChangeBatch::must_full_rescan`] is set).
//!
//! | Platform | Backend | Replays downtime? |
//! |----------|---------|-------------------|
//! | macOS    | FSEvents (`FSEventStreamCreate`, directory-level, event-id cursor) | yes |
//! | Linux    | fanotify filesystem marks with `FAN_REPORT_DFID_NAME` (needs `CAP_SYS_ADMIN`) | no |
//! | Linux    | inotify via `notify` (recursive, unprivileged fallback) | no |
//! | Windows  | `ReadDirectoryChangesW` via `notify` | no |
//!
//! Without replay the cursor is always `None`; after downtime the caller should rescan using
//! directory mtimes. Windows USN journal replay is a TODO (it needs a volume handle, which
//! usually requires elevation).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod fanotify;
#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(any(target_os = "linux", windows))]
mod notify_source;

/// Errors from change sources.
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// An OS call failed.
    #[error("watch I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// No roots were given.
    #[error("no roots to watch")]
    NoRoots,
    /// No change source exists for this platform.
    #[error("filesystem watching is not supported on this platform")]
    Unsupported,
    /// The backend failed to start.
    #[error("watch backend error: {0}")]
    Backend(String),
    /// A persisted cursor could not be decoded.
    #[error("invalid watch cursor: {0}")]
    Cursor(#[from] serde_json::Error),
}

/// Which mechanism a [`ChangeSource`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WatchBackend {
    /// macOS FSEvents.
    FsEvents,
    /// Linux fanotify filesystem marks.
    Fanotify,
    /// The `notify` crate (inotify / `ReadDirectoryChangesW`).
    Notify,
}

/// Resumable position in a backend's change history, persisted by the index alongside a
/// snapshot so changes made while the daemon was down can be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum WatchCursor {
    /// FSEvents position.
    FsEvents {
        /// Last event id that has been delivered.
        event_id: u64,
        /// Event-store UUID of each watched root's volume, keyed by root path. Event ids are
        /// only meaningful while the UUID is unchanged.
        volume_uuids: std::collections::BTreeMap<String, String>,
    },
}

impl WatchCursor {
    /// Serializes the cursor to opaque bytes (JSON) for persistence.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    /// Decodes bytes produced by [`WatchCursor::to_bytes`].
    ///
    /// # Errors
    ///
    /// * [`WatchError::Cursor`] if the bytes are not a valid cursor
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WatchError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

/// A batch of directories to rescan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChangeBatch {
    /// Directories whose subtrees changed, collapsed so no entry is inside another and the list
    /// stays below [`WatchConfig::collapse_threshold`] where possible.
    pub dirty: Vec<PathBuf>,
    /// The event history for `dirty` is incomplete (dropped events, queue overflow, root moved,
    /// unusable cursor): rescan those paths fully, without trusting directory mtimes.
    pub must_full_rescan: bool,
    /// Cursor covering everything up to and including this batch. Persist it only after the
    /// batch has been applied to the index.
    pub cursor: Option<WatchCursor>,
}

/// Configuration for [`open_change_source`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchConfig {
    /// Directories to watch recursively.
    pub roots: Vec<PathBuf>,
    /// Cursor to resume from (FSEvents only); `None` starts from "now".
    pub since: Option<WatchCursor>,
    /// Quiet period after the last event before a batch is emitted.
    pub debounce: Duration,
    /// Upper bound on how long events are held back while changes keep arriving.
    pub max_latency: Duration,
    /// Collapse dirty directories to common ancestors while a batch has more entries than this.
    pub collapse_threshold: usize,
    /// Prefer privileged native backends (fanotify) over `notify` where both exist.
    pub prefer_native: bool,
}

impl WatchConfig {
    /// Default configuration for `roots`.
    #[must_use]
    pub const fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            since: None,
            debounce: Duration::from_millis(500),
            max_latency: Duration::from_secs(5),
            collapse_threshold: 256,
            prefer_native: true,
        }
    }
}

/// A running filesystem watcher. Dropping it stops watching.
pub trait ChangeSource: Send {
    /// The backend in use.
    fn backend(&self) -> WatchBackend;

    /// Channel of change batches. It disconnects when the source stops.
    fn batches(&self) -> &Receiver<ChangeBatch>;

    /// Whether this backend can replay changes since a persisted cursor.
    fn supports_replay(&self) -> bool {
        false
    }

    /// A cursor for "now", to persist with a snapshot taken from a fresh scan. Take it
    /// *before* starting the scan so changes made during the scan are replayed.
    fn current_cursor(&self) -> Option<WatchCursor> {
        None
    }
}

/// Starts watching `config.roots` with the best available backend.
///
/// On Linux, fanotify is tried first when [`WatchConfig::prefer_native`] is set and falls back
/// to inotify when unprivileged or unsupported.
///
/// # Errors
///
/// * [`WatchError::NoRoots`] if `config.roots` is empty
/// * [`WatchError::Io`] / [`WatchError::Backend`] if the backend cannot start (missing root,
///   inotify watch limit, ...)
/// * [`WatchError::Unsupported`] on platforms without a backend
pub fn open_change_source(config: &WatchConfig) -> Result<Box<dyn ChangeSource>, WatchError> {
    if config.roots.is_empty() {
        return Err(WatchError::NoRoots);
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(fsevents::FsEventsSource::start(config)?))
    }
    #[cfg(target_os = "linux")]
    {
        if config.prefer_native {
            match fanotify::FanotifySource::start(config) {
                Ok(source) => return Ok(Box::new(source)),
                Err(e) => log::info!("fanotify unavailable ({e}); falling back to inotify"),
            }
        }
        Ok(Box::new(notify_source::NotifySource::start(config)?))
    }
    #[cfg(windows)]
    {
        // TODO: replay the NTFS USN journal (FSCTL_QUERY_USN_JOURNAL / FSCTL_READ_USN_JOURNAL)
        // when the volume handle can be opened (usually requires elevation), emitting a
        // `WatchCursor` variant with the journal id and next USN.
        Ok(Box::new(notify_source::NotifySource::start(config)?))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = config;
        Err(WatchError::Unsupported)
    }
}

/// Collapses dirty directories to a small set of covering directories.
///
/// Entries inside other entries are dropped; then, while more than `threshold` remain, the
/// deepest entries are replaced with their parents (never climbing above the root in `roots`
/// that contains them).
#[must_use]
pub fn collapse_dirty(
    paths: impl IntoIterator<Item = PathBuf>,
    roots: &[PathBuf],
    threshold: usize,
) -> Vec<PathBuf> {
    let root_of = |p: &Path| -> Option<&PathBuf> {
        roots
            .iter()
            .filter(|r| p.starts_with(r))
            .max_by_key(|r| r.components().count())
    };
    let liftable = |p: &Path| root_of(p).is_some_and(|r| r.as_path() != p);
    let mut set: BTreeSet<PathBuf> = paths.into_iter().collect();
    loop {
        // Component-wise ordering places descendants right after their ancestor.
        let mut out: Vec<PathBuf> = Vec::with_capacity(set.len());
        for p in set {
            if out.last().is_some_and(|last| p.starts_with(last)) {
                continue;
            }
            out.push(p);
        }
        if out.len() <= threshold {
            return out;
        }
        let Some(depth) = out
            .iter()
            .filter(|p| liftable(p))
            .map(|p| p.components().count())
            .max()
        else {
            return out;
        };
        set = out
            .into_iter()
            .map(|p| {
                if p.components().count() == depth && liftable(&p) {
                    p.parent().map_or_else(|| p.clone(), Path::to_path_buf)
                } else {
                    p
                }
            })
            .collect();
    }
}

/// Event from a backend thread to the coalescer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // not every backend produces every variant
pub(crate) enum RawEvent {
    /// The subtree at this path changed.
    Dirty(PathBuf),
    /// History for this path is incomplete.
    Rescan(PathBuf),
    /// History for every root is incomplete.
    RescanAll,
    /// New backend position.
    Cursor(WatchCursor),
    /// Emit pending changes immediately (e.g. replay finished).
    Flush,
}

/// Watched roots with their canonical spellings, used to map backend paths (which are
/// usually canonical, e.g. `/private/var/...` on macOS) back to the caller's spelling.
#[derive(Debug)]
pub(crate) struct Roots {
    entries: Vec<(PathBuf, PathBuf)>,
}

impl Roots {
    pub(crate) fn new(roots: &[PathBuf]) -> Self {
        let entries = roots
            .iter()
            .map(|r| {
                (
                    std::fs::canonicalize(r).unwrap_or_else(|_| r.clone()),
                    r.clone(),
                )
            })
            .collect();
        Self { entries }
    }

    /// Caller-supplied roots.
    pub(crate) fn originals(&self) -> Vec<PathBuf> {
        self.entries.iter().map(|(_, o)| o.clone()).collect()
    }

    /// Canonical roots (what backends should be pointed at).
    #[allow(dead_code)] // unused by some backends
    pub(crate) fn canonicals(&self) -> Vec<PathBuf> {
        self.entries.iter().map(|(c, _)| c.clone()).collect()
    }

    /// Maps a backend path to the caller's spelling; `None` if it is outside every root.
    pub(crate) fn map(&self, p: &Path) -> Option<PathBuf> {
        let mut best: Option<(usize, PathBuf)> = None;
        for (canonical, original) in &self.entries {
            for base in [canonical, original] {
                if let Ok(rel) = p.strip_prefix(base) {
                    let len = base.components().count();
                    if best.as_ref().is_none_or(|(l, _)| len > *l) {
                        let mapped = if rel.as_os_str().is_empty() {
                            original.clone()
                        } else {
                            original.join(rel)
                        };
                        best = Some((len, mapped));
                    }
                }
            }
        }
        best.map(|(_, p)| p)
    }
}

/// Coalescer output side, owned by every source.
pub(crate) struct Pipeline {
    pub(crate) rx: Receiver<ChangeBatch>,
}

/// Spawns the coalescer thread. Backends send [`RawEvent`]s into the returned sender; the
/// thread exits when every sender is dropped or the batch receiver is dropped.
pub(crate) fn spawn_coalescer(
    roots: Arc<Roots>,
    config: &WatchConfig,
) -> Result<(Sender<RawEvent>, Pipeline), WatchError> {
    let (raw_tx, raw_rx) = crossbeam_channel::unbounded::<RawEvent>();
    let (out_tx, out_rx) = crossbeam_channel::unbounded::<ChangeBatch>();
    let debounce = config.debounce;
    let max_latency = config.max_latency.max(debounce);
    let threshold = config.collapse_threshold;
    thread::Builder::new()
        .name("bclean-watch-coalesce".into())
        .spawn(move || coalesce(&raw_rx, &out_tx, &roots, debounce, max_latency, threshold))?;
    Ok((raw_tx, Pipeline { rx: out_rx }))
}

#[derive(Default)]
struct Pending {
    dirty: BTreeSet<PathBuf>,
    rescan: BTreeSet<PathBuf>,
    rescan_all: bool,
    cursor: Option<WatchCursor>,
}

impl Pending {
    /// Adds an event; returns `true` if it requests an immediate flush.
    fn add(&mut self, roots: &Roots, ev: RawEvent) -> bool {
        match ev {
            RawEvent::Dirty(p) => {
                if let Some(p) = roots.map(&p) {
                    self.dirty.insert(p);
                }
            }
            RawEvent::Rescan(p) => {
                if let Some(p) = roots.map(&p) {
                    self.rescan.insert(p);
                }
            }
            RawEvent::RescanAll => self.rescan_all = true,
            RawEvent::Cursor(c) => self.cursor = Some(c),
            RawEvent::Flush => return true,
        }
        false
    }

    fn is_empty(&self) -> bool {
        self.dirty.is_empty() && self.rescan.is_empty() && !self.rescan_all && self.cursor.is_none()
    }

    /// Emits up to two batches (full-rescan first). Returns `false` if the consumer is gone.
    fn emit(&mut self, out: &Sender<ChangeBatch>, roots: &Roots, threshold: usize) -> bool {
        let originals = roots.originals();
        let mut rescan = std::mem::take(&mut self.rescan);
        if std::mem::take(&mut self.rescan_all) {
            rescan.extend(originals.iter().cloned());
        }
        let rescan = collapse_dirty(rescan, &originals, threshold);
        let dirty: Vec<PathBuf> = std::mem::take(&mut self.dirty)
            .into_iter()
            .filter(|d| !rescan.iter().any(|r| d.starts_with(r)))
            .collect();
        let dirty = collapse_dirty(dirty, &originals, threshold);
        let mut cursor = self.cursor.take();
        if !rescan.is_empty() {
            let c = if dirty.is_empty() {
                cursor.take()
            } else {
                None
            };
            if out
                .send(ChangeBatch {
                    dirty: rescan,
                    must_full_rescan: true,
                    cursor: c,
                })
                .is_err()
            {
                return false;
            }
        }
        if (!dirty.is_empty() || cursor.is_some())
            && out
                .send(ChangeBatch {
                    dirty,
                    must_full_rescan: false,
                    cursor,
                })
                .is_err()
        {
            return false;
        }
        true
    }
}

fn coalesce(
    raw: &Receiver<RawEvent>,
    out: &Sender<ChangeBatch>,
    roots: &Roots,
    debounce: Duration,
    max_latency: Duration,
    threshold: usize,
) {
    let mut pending = Pending::default();
    while let Ok(first) = raw.recv() {
        let mut flush = pending.add(roots, first);
        let deadline = Instant::now() + max_latency;
        let mut disconnected = false;
        while !flush {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            match raw.recv_timeout(debounce.min(deadline - now)) {
                Ok(ev) => flush = pending.add(roots, ev),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if !pending.is_empty() && !pending.emit(out, roots, threshold) {
            return;
        }
        if disconnected {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn collapse_removes_nested() {
        let out = collapse_dirty(
            [p("/r/a/b"), p("/r/a"), p("/r/ab"), p("/r/a/c/d")],
            &[p("/r")],
            100,
        );
        assert_eq!(out, vec![p("/r/a"), p("/r/ab")]);
    }

    #[test]
    fn collapse_lifts_to_threshold_but_not_above_root() {
        let many: Vec<PathBuf> = (0..50)
            .map(|i| p(&format!("/r/x/y/{i}")))
            .chain([p("/r/z")])
            .collect();
        let out = collapse_dirty(many.clone(), &[p("/r")], 10);
        assert_eq!(out, vec![p("/r/x/y"), p("/r/z")]);
        let out = collapse_dirty(many.clone(), &[p("/r")], 1);
        assert_eq!(out, vec![p("/r")]);
        let out = collapse_dirty(many, &[p("/r")], 0);
        assert_eq!(out, vec![p("/r")]);
        // Paths outside every root are never lifted.
        let out = collapse_dirty([p("/o/1"), p("/o/2")], &[p("/r")], 1);
        assert_eq!(out, vec![p("/o/1"), p("/o/2")]);
    }

    #[test]
    fn roots_map_canonical_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::new(&[dir.path().to_path_buf()]);
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(roots.map(&canonical.join("x")), Some(dir.path().join("x")));
        assert_eq!(roots.map(&canonical), Some(dir.path().to_path_buf()));
        assert_eq!(roots.map(Path::new("/definitely/elsewhere")), None);
    }

    #[test]
    fn cursor_round_trip() {
        let c = WatchCursor::FsEvents {
            event_id: 42,
            volume_uuids: std::iter::once(("/".to_string(), "UUID".to_string())).collect(),
        };
        let bytes = c.to_bytes();
        assert!(
            std::str::from_utf8(&bytes)
                .unwrap()
                .contains("\"FS_EVENTS\"")
        );
        assert_eq!(WatchCursor::from_bytes(&bytes).unwrap(), c);
        assert!(WatchCursor::from_bytes(b"nope").is_err());
    }

    #[test]
    fn coalescer_debounces_and_splits_rescans() {
        let roots = Arc::new(Roots {
            entries: vec![(p("/r"), p("/r"))],
        });
        let config = WatchConfig {
            debounce: Duration::from_millis(20),
            ..WatchConfig::new(vec![p("/r")])
        };
        let (tx, pipe) = spawn_coalescer(roots, &config).unwrap();
        tx.send(RawEvent::Dirty(p("/r/a/b"))).unwrap();
        tx.send(RawEvent::Dirty(p("/r/a"))).unwrap();
        tx.send(RawEvent::Dirty(p("/elsewhere"))).unwrap();
        tx.send(RawEvent::Dirty(p("/r/c/d"))).unwrap();
        tx.send(RawEvent::Rescan(p("/r/c"))).unwrap();
        let cursor = WatchCursor::FsEvents {
            event_id: 7,
            volume_uuids: std::collections::BTreeMap::new(),
        };
        tx.send(RawEvent::Cursor(cursor.clone())).unwrap();
        tx.send(RawEvent::Flush).unwrap();
        let first = pipe.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            first,
            ChangeBatch {
                dirty: vec![p("/r/c")],
                must_full_rescan: true,
                cursor: None
            }
        );
        let second = pipe.rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            second,
            ChangeBatch {
                dirty: vec![p("/r/a")],
                must_full_rescan: false,
                cursor: Some(cursor)
            }
        );
        drop(tx);
        assert!(pipe.rx.recv_timeout(Duration::from_secs(5)).is_err());
    }
}
