//! Parallel work-stealing traversal and the single-threaded tree builder.
//!
//! Design:
//!
//! * Each job is one directory. Workers own a LIFO `crossbeam-deque` (depth-first locality,
//!   bounded memory) and steal from each other / a global injector when idle.
//! * A worker lists a directory with its backend [`Reader`], classifies entries (excludes,
//!   other filesystems, hard-link duplicates, oracle summaries), assigns every materialized
//!   subdirectory a *token* from a global counter, sends the whole listing as one message to the
//!   builder, and only then pushes the subdirectory jobs. Because a child's listing can only be
//!   produced after its job was pushed, the channel delivers every parent before its children,
//!   so the builder can resolve `token -> NodeId` with a flat vector and never waits.
//! * The builder (the calling thread) appends nodes with [`Tree::add_child_raw`] while the
//!   workers keep scanning, then fixes up summarized nodes and recomputes aggregates once.
//! * Summarized subtrees are walked by the same pool, but their jobs only add into a shared
//!   atomic accumulator; nothing is sent to the builder.
//! * On unix native backends a directory's open descriptor is shared (up to a process-wide
//!   budget) with its child jobs so children are opened with `openat(parent, name)` instead of
//!   re-resolving the absolute path; measured ~10% faster on wide parallel scans.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use bclean_model::ScanBackend;
use bclean_tree::{EntryKind, EntryMeta, NodeFlags, NodeId, Tree};
use crossbeam_channel::{Receiver, Sender};
use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use parking_lot::Mutex;

use crate::backend::{DirHandle, Reader, RootStat};
use crate::listing::{Listing, NO_TOKEN, RawEntry, os_str};
use crate::options::Excluder;
use crate::oracle::{ListedEntry, PruneOracle};
use crate::progress::ScanProgress;
use crate::{ScanError, ScanIssue};

/// Listings with more entries than this are not recycled (avoids pinning huge buffers).
const RECYCLE_MAX_ENTRIES: usize = 16 * 1024;
/// Record a progress path sample every this many directories per worker.
const SAMPLE_EVERY: u32 = 64;

/// Subtree totals of a summarized directory, accumulated by many workers.
struct Summary {
    apparent: AtomicU64,
    allocated: AtomicU64,
    items: AtomicU64,
    max_mtime: AtomicI64,
    error: AtomicBool,
}

impl Summary {
    const fn new() -> Self {
        Self {
            apparent: AtomicU64::new(0),
            allocated: AtomicU64::new(0),
            items: AtomicU64::new(0),
            max_mtime: AtomicI64::new(i64::MIN),
            error: AtomicBool::new(false),
        }
    }
}

enum Target {
    Node(u32),
    Summary(Arc<Summary>),
}

struct Job {
    path: PathBuf,
    /// Open handle of the parent directory (shared by siblings), if retained.
    parent: Option<Arc<Retained>>,
    target: Target,
}

/// Process-wide number of directory handles currently retained for child jobs.
static RETAINED: AtomicUsize = AtomicUsize::new(0);
/// Upper bound on retained handles (keeps well below typical `RLIMIT_NOFILE` soft limits).
const MAX_RETAINED: usize = 128;

/// A directory handle kept open so its children can be opened relative to it.
struct Retained(DirHandle);

impl Retained {
    /// Wraps `handle` if the process-wide budget allows, else drops (closes) it.
    fn try_new(handle: Option<DirHandle>) -> Option<Arc<Self>> {
        let handle = handle?;
        if RETAINED.fetch_add(1, Ordering::Relaxed) >= MAX_RETAINED {
            RETAINED.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        Some(Arc::new(Self(handle)))
    }
}

impl Drop for Retained {
    fn drop(&mut self) {
        RETAINED.fetch_sub(1, Ordering::Relaxed);
    }
}

struct DirMsg {
    token: u32,
    error: bool,
    listing: Listing,
    summaries: Vec<(usize, Arc<Summary>)>,
}

/// Inputs of one walk.
pub struct WalkParams<'a> {
    pub root: PathBuf,
    pub root_stat: RootStat,
    pub one_filesystem: bool,
    pub threads: usize,
    pub backend: ScanBackend,
    pub excluder: &'a Excluder,
    pub oracle: &'a dyn PruneOracle,
    pub progress: &'a ScanProgress,
}

struct Shared<'a> {
    excluder: &'a Excluder,
    oracle: &'a dyn PruneOracle,
    progress: &'a ScanProgress,
    one_filesystem: bool,
    root_dev: u64,
    backend: ScanBackend,
    injector: Injector<Job>,
    stealers: Vec<Stealer<Job>>,
    pending: AtomicUsize,
    next_token: AtomicU32,
    hardlinks: Mutex<BTreeSet<(u64, u64)>>,
    issues: Mutex<Vec<ScanIssue>>,
}

struct WorkerState {
    reader: Reader,
    local: Worker<Job>,
    tx: Sender<DirMsg>,
    recycled: Receiver<Listing>,
    spare: Listing,
    jobs: Vec<Job>,
    visited: u32,
}

/// Runs a full walk and returns the built tree plus per-path issues.
///
/// # Errors
///
/// * [`ScanError::Spawn`] if a worker thread cannot be spawned
/// * [`ScanError::Cancelled`] if the progress cancel flag was raised
pub fn walk(params: WalkParams<'_>) -> Result<(Tree, Vec<ScanIssue>), ScanError> {
    let root_meta = EntryMeta::new(
        params.root_stat.kind,
        params.root_stat.apparent,
        params.root_stat.allocated,
        params.root_stat.mtime,
    );
    let mut tree = Tree::with_capacity(params.root.clone(), root_meta, 1 << 12, 1 << 16);
    // The root directory itself is counted as a dir when its listing is processed.
    params.progress.add(
        0,
        u64::from(params.root_stat.kind != EntryKind::Dir),
        params.root_stat.allocated,
        0,
    );
    if params.root_stat.kind != EntryKind::Dir {
        return Ok((tree, Vec::new()));
    }

    let workers: Vec<Worker<Job>> = (0..params.threads).map(|_| Worker::new_lifo()).collect();
    let shared = Shared {
        excluder: params.excluder,
        oracle: params.oracle,
        progress: params.progress,
        one_filesystem: params.one_filesystem,
        root_dev: params.root_stat.dev,
        backend: params.backend,
        injector: Injector::new(),
        stealers: workers.iter().map(Worker::stealer).collect(),
        pending: AtomicUsize::new(1),
        next_token: AtomicU32::new(1),
        hardlinks: Mutex::new(BTreeSet::new()),
        issues: Mutex::new(Vec::new()),
    };
    shared.injector.push(Job {
        path: params.root,
        parent: None,
        target: Target::Node(0),
    });

    let (tx, rx) = crossbeam_channel::unbounded::<DirMsg>();
    let (recycle_tx, recycle_rx) = crossbeam_channel::bounded::<Listing>(params.threads * 4);
    let summaries = std::thread::scope(|scope| -> Result<_, ScanError> {
        for (i, local) in workers.into_iter().enumerate() {
            let state = WorkerState {
                reader: Reader::new(shared.backend),
                local,
                tx: tx.clone(),
                recycled: recycle_rx.clone(),
                spare: Listing::default(),
                jobs: Vec::new(),
                visited: 0,
            };
            let shared = &shared;
            let spawned = std::thread::Builder::new()
                .name(format!("bclean-scan-{i}"))
                .spawn_scoped(scope, move || shared.run_worker(state));
            if let Err(e) = spawned {
                // Let already-running workers drain quickly, then report.
                shared.progress.cancel();
                return Err(ScanError::Spawn(e));
            }
        }
        drop(tx);
        drop(recycle_rx);
        Ok(build(&mut tree, &rx, &recycle_tx))
    })?;

    if params.progress.is_cancelled() {
        return Err(ScanError::Cancelled);
    }
    finish_summaries(&mut tree, summaries);
    tree.recompute_aggregates();
    let issues = std::mem::take(&mut *shared.issues.lock());
    Ok((tree, issues))
}

/// Consumes listings and appends nodes until every worker has exited.
fn build(
    tree: &mut Tree,
    rx: &Receiver<DirMsg>,
    recycle: &Sender<Listing>,
) -> Vec<(NodeId, Arc<Summary>)> {
    let mut map: Vec<u32> = vec![NodeId::ROOT.raw()];
    let mut summaries = Vec::new();
    for msg in rx {
        let parent = NodeId::from_raw(map[msg.token as usize]);
        if msg.error {
            tree.insert_flags(parent, NodeFlags::ERROR);
        }
        let listing = msg.listing;
        tree.reserve(listing.entries.len(), listing.names.len());
        let mut next_summary = msg.summaries.into_iter().peekable();
        for (i, e) in listing.entries.iter().enumerate() {
            let meta = EntryMeta {
                kind: e.kind,
                apparent: e.apparent,
                allocated: e.allocated,
                mtime: e.mtime,
                items: 1,
                flags: e.flags,
            };
            let id = tree.add_child_raw(parent, listing.name(e), meta);
            if e.child != NO_TOKEN {
                let t = e.child as usize;
                if t >= map.len() {
                    map.resize(t + 1, NO_TOKEN);
                }
                map[t] = id.raw();
            }
            if let Some((_, acc)) = next_summary.next_if(|(j, _)| *j == i) {
                summaries.push((id, acc));
            }
        }
        if listing.entries.capacity() <= RECYCLE_MAX_ENTRIES {
            let _ = recycle.try_send(listing);
        }
    }
    summaries
}

/// Turns each summarized node's accumulator into its own sizes.
fn finish_summaries(tree: &mut Tree, summaries: Vec<(NodeId, Arc<Summary>)>) {
    for (id, acc) in summaries {
        let info = tree.info(id);
        let mut flags = info.flags;
        if acc.error.load(Ordering::Relaxed) {
            flags = flags.union(NodeFlags::ERROR);
        }
        let meta = EntryMeta {
            kind: info.kind,
            apparent: info
                .own_apparent
                .saturating_add(acc.apparent.load(Ordering::Relaxed)),
            allocated: info
                .own_allocated
                .saturating_add(acc.allocated.load(Ordering::Relaxed)),
            mtime: info.mtime.max(acc.max_mtime.load(Ordering::Relaxed)),
            items: 1 + acc.items.load(Ordering::Relaxed),
            flags,
        };
        tree.update_own(id, meta);
    }
}

impl Shared<'_> {
    fn run_worker(&self, mut state: WorkerState) {
        let mut idle = 0u32;
        loop {
            if let Some(job) = self.find_job(&state.local) {
                idle = 0;
                if !self.progress.is_cancelled() {
                    let parent = job.parent.as_deref().map(|r| &r.0);
                    match job.target {
                        Target::Node(token) => {
                            self.process_node(&mut state, &job.path, parent, token);
                        }
                        Target::Summary(acc) => {
                            self.process_summary(&mut state, &job.path, parent, &acc);
                        }
                    }
                }
                self.pending.fetch_sub(1, Ordering::AcqRel);
            } else if self.pending.load(Ordering::Acquire) == 0 {
                break;
            } else {
                idle = idle.saturating_add(1);
                if idle < 32 {
                    std::hint::spin_loop();
                } else if idle < 96 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(50));
                }
            }
        }
    }

    fn find_job(&self, local: &Worker<Job>) -> Option<Job> {
        local.pop().or_else(|| {
            std::iter::repeat_with(|| {
                self.injector
                    .steal_batch_and_pop(local)
                    .or_else(|| self.stealers.iter().map(Stealer::steal).collect())
            })
            .find(|s| !s.is_retry())
            .and_then(Steal::success)
        })
    }

    fn push_jobs(&self, state: &mut WorkerState) {
        if state.jobs.is_empty() {
            return;
        }
        self.pending.fetch_add(state.jobs.len(), Ordering::AcqRel);
        for job in state.jobs.drain(..) {
            state.local.push(job);
        }
    }

    fn sample(&self, state: &mut WorkerState, path: &Path) {
        state.visited = state.visited.wrapping_add(1);
        if state.visited % SAMPLE_EVERY == 1 {
            self.progress.sample(path);
        }
    }

    fn issue(&self, path: PathBuf, err: &std::io::Error) {
        self.issues.lock().push(ScanIssue {
            path,
            kind: err.kind(),
            message: err.to_string(),
        });
    }

    fn entry_issue(&self, path: PathBuf) {
        self.issues.lock().push(ScanIssue {
            path,
            kind: std::io::ErrorKind::Other,
            message: "failed to read entry metadata".to_owned(),
        });
    }

    /// Returns `true` if this (dev, ino) pair was not seen before.
    fn first_link(&self, e: &RawEntry) -> bool {
        self.hardlinks.lock().insert((e.dev, e.ino))
    }

    const fn other_fs(&self, e: &RawEntry) -> bool {
        self.one_filesystem && (e.mount_point || e.dev != self.root_dev)
    }

    /// Applies excludes, filesystem boundaries and hard-link dedupe to a materialized listing.
    /// Pushes indices of walkable subdirectories to `subdirs`; returns `(files, bytes, errors)`.
    fn classify(
        &self,
        path: &Path,
        listing: &mut Listing,
        subdirs: &mut Vec<usize>,
    ) -> (u64, u64, u64) {
        let (mut files, mut bytes, mut errors) = (0u64, 0u64, 0u64);
        let Listing { names, entries } = listing;
        for (i, e) in entries.iter_mut().enumerate() {
            let start = e.name_off as usize;
            let name = os_str(&names[start..start + e.name_len as usize]);
            if e.flags.contains(NodeFlags::ERROR) {
                errors += 1;
                self.entry_issue(path.join(name));
            }
            if !self.excluder.is_empty() && self.excluder.matches(path, name) {
                e.flags = e.flags.union(NodeFlags::EXCLUDED);
                e.apparent = 0;
                e.allocated = 0;
                continue;
            }
            if e.kind == EntryKind::Dir {
                if self.other_fs(e) {
                    e.flags = e.flags.union(NodeFlags::OTHER_FS);
                } else if !e.flags.contains(NodeFlags::ERROR) {
                    subdirs.push(i);
                }
            } else {
                files += 1;
                if e.nlink > 1 && !self.first_link(e) {
                    e.allocated = 0;
                    e.flags = e.flags.union(NodeFlags::HARDLINK_DUP);
                }
            }
            bytes += e.allocated;
        }
        (files, bytes, errors)
    }

    /// Sorted, deduplicated entry indices the oracle wants summarized.
    fn oracle_picks(&self, path: &Path, listing: &Listing, subdirs: &[usize]) -> Vec<usize> {
        if subdirs.is_empty() || self.oracle.is_noop() {
            return Vec::new();
        }
        let listed: Vec<ListedEntry<'_>> = listing
            .entries
            .iter()
            .map(|e| ListedEntry {
                name: listing.name(e),
                kind: e.kind,
            })
            .collect();
        let mut picks = self.oracle.summarize(path, &listed);
        picks.sort_unstable();
        picks.dedup();
        picks
    }

    fn process_node(
        &self,
        state: &mut WorkerState,
        path: &Path,
        parent: Option<&DirHandle>,
        token: u32,
    ) {
        self.sample(state, path);
        let mut listing = state.recycled.try_recv().unwrap_or_default();
        listing.clear();
        let mut dir_error = false;
        let mut errors = 0u64;
        let mut handle = None;
        match state.reader.list(path, parent, &mut listing) {
            Ok(h) => handle = h,
            Err(e) => {
                self.issue(path.to_path_buf(), &e);
                dir_error = true;
                errors += 1;
            }
        }

        let mut subdirs: Vec<usize> = Vec::new();
        let (files, bytes, entry_errors) = self.classify(path, &mut listing, &mut subdirs);
        errors += entry_errors;
        let picks = self.oracle_picks(path, &listing, &subdirs);

        let materialized = subdirs.len()
            - picks
                .iter()
                .filter(|p| subdirs.binary_search(p).is_ok())
                .count();
        let mut token_next = if materialized == 0 {
            NO_TOKEN
        } else {
            self.next_token.fetch_add(
                u32::try_from(materialized).unwrap_or(u32::MAX),
                Ordering::Relaxed,
            )
        };
        let mut summaries = Vec::new();
        let retained = if subdirs.is_empty() {
            None
        } else {
            Retained::try_new(handle)
        };
        for &i in &subdirs {
            let child_path = path.join(listing.name(&listing.entries[i]));
            let e = &mut listing.entries[i];
            let target = if picks.binary_search(&i).is_ok() {
                let acc = Arc::new(Summary::new());
                e.flags = e.flags.union(NodeFlags::SUMMARIZED);
                summaries.push((i, Arc::clone(&acc)));
                Target::Summary(acc)
            } else {
                e.child = token_next;
                token_next += 1;
                Target::Node(e.child)
            };
            state.jobs.push(Job {
                path: child_path,
                parent: retained.clone(),
                target,
            });
        }

        self.progress.add(1, files, bytes, errors);
        // The receiver lives until every worker has exited, so sending cannot fail.
        let _ = state.tx.send(DirMsg {
            token,
            error: dir_error,
            listing,
            summaries,
        });
        self.push_jobs(state);
    }

    fn process_summary(
        &self,
        state: &mut WorkerState,
        path: &Path,
        parent: Option<&DirHandle>,
        acc: &Arc<Summary>,
    ) {
        self.sample(state, path);
        let mut listing = std::mem::take(&mut state.spare);
        listing.clear();
        let mut errors = 0u64;
        let mut handle = None;
        match state.reader.list(path, parent, &mut listing) {
            Ok(h) => handle = h,
            Err(e) => {
                self.issue(path.to_path_buf(), &e);
                acc.error.store(true, Ordering::Relaxed);
                errors += 1;
            }
        }
        let mut retained: Option<Option<Arc<Retained>>> = None;
        let (mut apparent, mut allocated, mut items, mut files) = (0u64, 0u64, 0u64, 0u64);
        let mut max_mtime = i64::MIN;
        for e in &listing.entries {
            let name = listing.name(e);
            if !self.excluder.is_empty() && self.excluder.matches(path, name) {
                continue;
            }
            if e.flags.contains(NodeFlags::ERROR) {
                errors += 1;
                acc.error.store(true, Ordering::Relaxed);
                self.entry_issue(path.join(name));
            }
            items += 1;
            max_mtime = max_mtime.max(e.mtime);
            apparent += e.apparent;
            if e.kind == EntryKind::Dir {
                allocated += e.allocated;
                if !self.other_fs(e) && !e.flags.contains(NodeFlags::ERROR) {
                    let parent = retained
                        .get_or_insert_with(|| Retained::try_new(handle.take()))
                        .clone();
                    state.jobs.push(Job {
                        path: path.join(name),
                        parent,
                        target: Target::Summary(Arc::clone(acc)),
                    });
                }
            } else {
                files += 1;
                if e.nlink <= 1 || self.first_link(e) {
                    allocated += e.allocated;
                }
            }
        }
        acc.apparent.fetch_add(apparent, Ordering::Relaxed);
        acc.allocated.fetch_add(allocated, Ordering::Relaxed);
        acc.items.fetch_add(items, Ordering::Relaxed);
        acc.max_mtime.fetch_max(max_mtime, Ordering::Relaxed);
        self.progress.add(1, files, allocated, errors);
        state.spare = listing;
        self.push_jobs(state);
    }
}
