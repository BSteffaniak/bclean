//! Live progress counters and cancellation shared between a scan and its observers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;

/// Progress of a running scan, updated by worker threads and readable from any thread.
///
/// Share it with `Arc<ScanProgress>` (or a reference inside `std::thread::scope`). Counters are
/// cumulative: reuse across scans requires [`ScanProgress::reset`].
#[derive(Debug, Default)]
pub struct ScanProgress {
    dirs: AtomicU64,
    files: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    cancelled: AtomicBool,
    current: Mutex<PathBuf>,
}

/// Point-in-time copy of the [`ScanProgress`] counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressSnapshot {
    /// Directories listed so far.
    pub dirs: u64,
    /// Non-directory entries seen so far.
    pub files: u64,
    /// Allocated bytes seen so far (hard-link duplicates excluded).
    pub bytes: u64,
    /// Read errors encountered so far.
    pub errors: u64,
}

impl ScanProgress {
    /// Creates zeroed progress.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current counters.
    #[must_use]
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            dirs: self.dirs.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    /// A recently visited directory (sampled, not every directory is recorded).
    #[must_use]
    pub fn current_path(&self) -> PathBuf {
        self.current.lock().clone()
    }

    /// Requests cancellation; the scan stops promptly and returns
    /// [`crate::ScanError::Cancelled`].
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// `true` once [`ScanProgress::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Zeroes all counters and clears the cancel flag.
    pub fn reset(&self) {
        self.dirs.store(0, Ordering::Relaxed);
        self.files.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.cancelled.store(false, Ordering::Relaxed);
        self.current.lock().clear();
    }

    pub(crate) fn add(&self, dirs: u64, files: u64, bytes: u64, errors: u64) {
        if dirs != 0 {
            self.dirs.fetch_add(dirs, Ordering::Relaxed);
        }
        if files != 0 {
            self.files.fetch_add(files, Ordering::Relaxed);
        }
        if bytes != 0 {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        if errors != 0 {
            self.errors.fetch_add(errors, Ordering::Relaxed);
        }
    }

    /// Records `path` as the current sample if nobody else holds the lock.
    pub(crate) fn sample(&self, path: &std::path::Path) {
        if let Some(mut cur) = self.current.try_lock() {
            cur.clear();
            cur.push(path);
        }
    }
}
