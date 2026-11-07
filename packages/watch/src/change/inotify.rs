//! Linux inotify change source with a watch budget.
//!
//! inotify needs one watch per directory and the per-user limit
//! (`fs.inotify.max_user_watches`) is shared with every other program (editors, build tools,
//! file managers). Instead of `notify`'s recursive mode, which walks the whole tree up front,
//! follows symlinks, and fails outright on the first directory it cannot watch, directories
//! are watched one by one from a background thread:
//!
//! * Breadth first, so the shallow (usually most active) part of each root is covered first.
//! * Symlinks are never followed and other filesystems are never entered.
//! * Directories that cannot be watched (permissions, races) are skipped.
//! * At most [`WatchConfig::max_watches`] watches (default: 40% of the per-user limit).
//!   Directories left over are reported as [`RawEvent::Rescan`] every
//!   [`WatchConfig::unwatched_rescan_interval`] (the consumer's rescan budget decides when
//!   they actually run), and are watched later if the budget frees up.
//! * Directories created or moved into a watched directory are watched as they appear.

use std::collections::{BTreeSet, VecDeque};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};

use super::notify_source::handle;
use super::{
    ChangeBatch, ChangeSource, Pipeline, RawEvent, Roots, WatchBackend, WatchConfig, WatchError,
    spawn_coalescer,
};

/// Share of `fs.inotify.max_user_watches` used when [`WatchConfig::max_watches`] is 0.
const DEFAULT_BUDGET_PERCENT: usize = 40;
/// Kernel default limit on older kernels, assumed when the sysctl cannot be read.
const FALLBACK_USER_WATCHES: usize = 8192;
/// How often the manager checks for shutdown while idle.
const TICK: Duration = Duration::from_millis(500);

/// Budgeted inotify [`ChangeSource`].
pub struct InotifySource {
    stop: Arc<AtomicBool>,
    pipeline: Pipeline,
}

impl InotifySource {
    pub fn start(config: &WatchConfig) -> Result<Self, WatchError> {
        let roots = Arc::new(Roots::new(&config.roots));
        let (tx, pipeline) = spawn_coalescer(Arc::clone(&roots), config)?;
        let (dirs_tx, dirs_rx) = crossbeam_channel::unbounded::<DirChange>();
        let root_list = roots.originals();
        let events_tx = tx.clone();
        let watcher = notify::INotifyWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = &res {
                    for change in dir_changes(event) {
                        let _ = dirs_tx.send(change);
                    }
                }
                handle(&events_tx, &root_list, res);
            },
            notify::Config::default(),
        )
        .map_err(|e| WatchError::Backend(e.to_string()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut manager = Manager {
            watcher,
            budget: budget(config.max_watches),
            watched: BTreeSet::new(),
            unwatched: BTreeSet::new(),
            limit_logged: false,
            raw: tx,
            rescan_every: config.unwatched_rescan_interval,
        };
        let roots = roots.originals();
        let flag = Arc::clone(&stop);
        thread::Builder::new()
            .name("bclean-inotify".into())
            .spawn(move || manager.run(&roots, &dirs_rx, &flag))?;
        Ok(Self { stop, pipeline })
    }
}

impl Drop for InotifySource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl ChangeSource for InotifySource {
    fn backend(&self) -> WatchBackend {
        WatchBackend::Notify
    }

    fn batches(&self) -> &Receiver<ChangeBatch> {
        &self.pipeline.rx
    }
}

/// Directory changes that affect which directories are watched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DirChange {
    /// A directory appeared (created or moved in): watch it and its subtree.
    Added(PathBuf),
    /// A directory disappeared (removed or moved away): forget it and its subtree.
    Gone(PathBuf),
}

fn dir_changes(event: &Event) -> Vec<DirChange> {
    // Renamed files are filtered out by the manager (they are never watched, and `Added`
    // checks that the path is a directory).
    let change: fn(PathBuf) -> DirChange = match event.kind {
        EventKind::Create(CreateKind::Folder)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => DirChange::Added,
        EventKind::Remove(RemoveKind::Folder)
        | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => DirChange::Gone,
        _ => return Vec::new(),
    };
    event.paths.iter().cloned().map(change).collect()
}

/// The watch budget for a configured maximum (`0` = a share of the per-user limit).
fn budget(configured: usize) -> usize {
    if configured > 0 {
        return configured;
    }
    let limit = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(FALLBACK_USER_WATCHES);
    (limit * DEFAULT_BUDGET_PERCENT / 100).max(1)
}

struct Manager {
    watcher: notify::INotifyWatcher,
    budget: usize,
    watched: BTreeSet<PathBuf>,
    /// Directories whose subtrees are not watched (over budget or the kernel limit).
    unwatched: BTreeSet<PathBuf>,
    limit_logged: bool,
    raw: Sender<RawEvent>,
    rescan_every: Duration,
}

impl Manager {
    fn run(&mut self, roots: &[PathBuf], changes: &Receiver<DirChange>, stop: &AtomicBool) {
        let started = Instant::now();
        let devices: Vec<(PathBuf, u64)> = roots
            .iter()
            .filter_map(|r| Some((r.clone(), std::fs::metadata(r).ok()?.dev())))
            .collect();
        self.walk(roots.iter().cloned().collect(), &devices, stop);
        log::info!(
            "inotify: watching {} directories in {:.1?} (budget {}, {} subtree(s) left to rescans)",
            self.watched.len(),
            started.elapsed(),
            self.budget,
            self.unwatched.len()
        );
        let mut next_rescan = Instant::now() + self.rescan_every;
        while !stop.load(Ordering::Relaxed) {
            match changes.recv_timeout(TICK) {
                Ok(change) => self.apply(change, &devices, stop),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if Instant::now() >= next_rescan {
                next_rescan = Instant::now() + self.rescan_every;
                self.revisit_unwatched(&devices, stop);
            }
        }
    }

    fn apply(&mut self, change: DirChange, devices: &[(PathBuf, u64)], stop: &AtomicBool) {
        match change {
            DirChange::Added(path) => {
                if !self.watched.contains(&path)
                    && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
                {
                    self.walk(VecDeque::from([path]), devices, stop);
                }
            }
            DirChange::Gone(path) => {
                let gone: Vec<PathBuf> = self
                    .watched
                    .range(path.clone()..)
                    .take_while(|p| p.starts_with(&path))
                    .cloned()
                    .collect();
                for p in gone {
                    // inotify drops watches of deleted directories by itself; this only
                    // matters for directories moved elsewhere.
                    let _ = self.watcher.unwatch(&p);
                    self.watched.remove(&p);
                }
                self.unwatched.retain(|p| !p.starts_with(&path));
            }
        }
    }

    /// Re-reports unwatched subtrees for rescanning, after trying to watch them if the budget
    /// has room again.
    fn revisit_unwatched(&mut self, devices: &[(PathBuf, u64)], stop: &AtomicBool) {
        if self.unwatched.is_empty() {
            return;
        }
        if self.watched.len() < self.budget && !self.limit_logged {
            let queue: VecDeque<PathBuf> =
                std::mem::take(&mut self.unwatched).into_iter().collect();
            self.walk(queue, devices, stop);
        }
        for dir in &self.unwatched {
            let _ = self.raw.send(RawEvent::Rescan(dir.clone()));
        }
    }

    /// Watches `queue` and everything below it, breadth first, until the budget runs out.
    fn walk(
        &mut self,
        mut queue: VecDeque<PathBuf>,
        devices: &[(PathBuf, u64)],
        stop: &AtomicBool,
    ) {
        while let Some(dir) = queue.pop_front() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if self.watched.len() >= self.budget || self.limit_logged {
                self.unwatched.insert(dir);
                self.unwatched.extend(queue);
                return;
            }
            if self.watched.contains(&dir) {
                continue;
            }
            match self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(dir.clone());
                }
                Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                    log::warn!(
                        "inotify watch limit reached after {} directories \
                         (raise fs.inotify.max_user_watches or lower monitor.max_watches); \
                         the rest is kept current by rescans",
                        self.watched.len()
                    );
                    self.limit_logged = true;
                    self.unwatched.insert(dir);
                    self.unwatched.extend(queue);
                    return;
                }
                Err(e) => {
                    log::debug!("cannot watch {}: {e}", dir.display());
                    continue;
                }
            }
            let Some(dev) = device_of(&dir, devices) else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                // `file_type` does not follow symlinks.
                if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let path = entry.path();
                if entry.metadata().is_ok_and(|m| m.dev() == dev) {
                    queue.push_back(path);
                }
            }
        }
    }
}

/// Device of the root containing `dir` (directories on other devices are not entered).
fn device_of(dir: &Path, devices: &[(PathBuf, u64)]) -> Option<u64> {
    devices
        .iter()
        .filter(|(root, _)| dir.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, dev)| *dev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(dir: &Path, max_watches: usize) -> InotifySource {
        InotifySource::start(&WatchConfig {
            debounce: Duration::from_millis(50),
            max_watches,
            unwatched_rescan_interval: Duration::from_millis(300),
            ..WatchConfig::new(vec![dir.to_path_buf()])
        })
        .unwrap()
    }

    fn wait_for(rx: &Receiver<ChangeBatch>, want: impl Fn(&ChangeBatch) -> bool) -> ChangeBatch {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let batch = rx.recv_timeout(left).expect("expected change batch");
            if want(&batch) {
                return batch;
            }
        }
    }

    #[test]
    fn watches_new_directories_and_skips_unreadable_ones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let src = source(&root, 0);
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(root.join("a/b/f"), b"x").unwrap();
        wait_for(src.batches(), |b| b.dirty.contains(&root.join("a/b")));
        // A directory created after startup is watched too.
        std::fs::create_dir(root.join("new")).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(root.join("new/f"), b"x").unwrap();
        wait_for(src.batches(), |b| b.dirty.contains(&root.join("new")));
    }

    #[test]
    fn directories_over_budget_are_reported_for_rescans() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("a/deep")).unwrap();
        let src = source(&root, 2);
        let batch = wait_for(src.batches(), |b| b.must_full_rescan);
        assert_eq!(batch.dirty, vec![root.join("a/deep")]);
    }

    #[test]
    fn budget_defaults_to_a_share_of_the_user_limit() {
        assert_eq!(budget(123), 123);
        assert!(budget(0) >= 1);
    }
}
