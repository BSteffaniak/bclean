#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Fast parallel filesystem scanner with per-OS bulk metadata backends.
//!
//! [`scan`] walks a directory tree with a work-stealing thread pool and produces a
//! [`bclean_tree::Tree`] with apparent and allocated sizes, item counts and modification times.
//!
//! * **Backends** ([`ScanBackend`]): macOS uses `getattrlistbulk` (names + metadata for a whole
//!   batch of entries per syscall), Linux uses `getdents64` + fd-relative `statx` with a minimal
//!   mask, Windows uses `GetFileInformationByHandleEx(FileFullDirectoryInfo)` (sizes come with
//!   the enumeration). [`ScanBackend::Portable`] uses `std::fs` everywhere.
//!   [`ScanBackend::Mft`] is not implemented yet and falls back to the native backend.
//! * **Semantics**: symlinks are never followed; with [`ScanOptions::one_filesystem`] mount
//!   points become leaves flagged [`OTHER_FS`](bclean_tree::NodeFlags::OTHER_FS); hard links
//!   (only entries with `nlink > 1`) are deduplicated by `(dev, ino)`: every copy after the
//!   first gets `allocated = 0` and [`HARDLINK_DUP`](bclean_tree::NodeFlags::HARDLINK_DUP)
//!   (apparent size is kept). Read failures never abort the scan: the affected node gets
//!   [`ERROR`](bclean_tree::NodeFlags::ERROR) and a [`ScanIssue`] is recorded.
//! * **Pruning**: a caller-provided [`PruneOracle`] can summarize directories (e.g.
//!   `node_modules`), which are still walked for exact totals but not materialized.
//! * **Progress**: [`ScanProgress`] exposes live atomic counters, a sampled current path, and
//!   cancellation.
//!
//! ```no_run
//! use bclean_scan::{NoPrune, ScanOptions, ScanProgress, scan};
//! use bclean_tree::{NodeId, SizeMode};
//!
//! let progress = ScanProgress::new();
//! let outcome = scan(&ScanOptions::new("/home/me"), &NoPrune, &progress)?;
//! println!(
//!     "{} bytes in {} items ({} errors)",
//!     outcome.tree.size(NodeId::ROOT, SizeMode::Allocated),
//!     outcome.tree.info(NodeId::ROOT).items,
//!     outcome.issues.len(),
//! );
//! # Ok::<(), bclean_scan::ScanError>(())
//! ```

mod backend;
mod listing;
mod options;
mod oracle;
mod progress;
mod walk;

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use bclean_model::ScanBackend;
use bclean_tree::Tree;

pub use options::{DEFAULT_MAX_THREADS, ScanOptions};
pub use oracle::{ListedEntry, NoPrune, PruneOracle};
pub use progress::{ProgressSnapshot, ScanProgress};

/// Errors that abort a scan. Per-path read failures are reported as [`ScanIssue`]s instead.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The scan root does not exist or cannot be stat'ed.
    #[error("cannot read scan root {path}: {source}")]
    Root {
        /// Root path as given.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// An exclude pattern is not a valid glob.
    #[error("invalid exclude pattern {pattern:?}: {source}")]
    InvalidExclude {
        /// Offending pattern(s).
        pattern: String,
        /// Underlying error.
        #[source]
        source: globset::Error,
    },
    /// A worker thread could not be spawned.
    #[error("failed to spawn scan worker: {0}")]
    Spawn(#[source] io::Error),
    /// [`ScanProgress::cancel`] was called.
    #[error("scan cancelled")]
    Cancelled,
}

/// A path that could not be read completely. The scan continued; the corresponding node (if
/// any) carries [`NodeFlags::ERROR`](bclean_tree::NodeFlags::ERROR).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanIssue {
    /// Absolute path of the directory or entry.
    pub path: PathBuf,
    /// Error kind (e.g. [`io::ErrorKind::PermissionDenied`]).
    pub kind: io::ErrorKind,
    /// Human-readable error message.
    pub message: String,
}

/// Result of a successful scan.
#[derive(Debug)]
pub struct ScanOutcome {
    /// The scanned tree with aggregates computed. Its root path is the absolute scan root.
    pub tree: Tree,
    /// Per-path read failures.
    pub issues: Vec<ScanIssue>,
    /// Wall-clock duration of the scan.
    pub elapsed: Duration,
    /// Backend that actually ran ([`ScanBackend::Native`] or [`ScanBackend::Portable`]).
    pub backend: ScanBackend,
    /// Final progress counters of this scan.
    pub stats: ProgressSnapshot,
}

/// Resolves a requested backend to the one that will run on this platform.
///
/// * `Auto`, `Native` and `Mft` resolve to `Native` on macOS, Linux and Windows (MFT reading is
///   not implemented yet), and to `Portable` elsewhere.
/// * `Portable` always stays `Portable`.
#[must_use]
pub const fn resolve_backend(requested: ScanBackend) -> ScanBackend {
    backend::resolve(requested)
}

/// Scans `options.root` and builds a tree.
///
/// Worker threads list directories in parallel; the calling thread builds the tree. Counters in
/// `progress` are updated live (they are added to, not reset) and `progress.cancel()` stops the
/// scan promptly. Directories the `oracle` selects are summarized (see [`PruneOracle`]); pass
/// [`NoPrune`] to materialize everything.
///
/// If the root is not a directory the tree consists of the root node only.
///
/// # Errors
///
/// * [`ScanError::Root`] if the root cannot be stat'ed (or made absolute)
/// * [`ScanError::InvalidExclude`] if an exclude pattern is invalid
/// * [`ScanError::Spawn`] if a worker thread cannot be spawned
/// * [`ScanError::Cancelled`] if `progress` was cancelled before the scan finished
pub fn scan(
    options: &ScanOptions,
    oracle: &dyn PruneOracle,
    progress: &ScanProgress,
) -> Result<ScanOutcome, ScanError> {
    scan_impl(options, oracle, progress, true)
}

fn scan_impl(
    options: &ScanOptions,
    oracle: &dyn PruneOracle,
    progress: &ScanProgress,
    descend: bool,
) -> Result<ScanOutcome, ScanError> {
    let start = Instant::now();
    let before = progress.snapshot();
    let root_err = |source| ScanError::Root {
        path: options.root.clone(),
        source,
    };
    let root = std::path::absolute(&options.root).map_err(root_err)?;
    let root_stat = backend::stat_root(&root).map_err(root_err)?;
    let excluder = options::Excluder::new(&options.exclude)?;
    let backend = backend::resolve(options.backend);
    let (tree, issues) = walk::walk(walk::WalkParams {
        root,
        root_stat,
        one_filesystem: options.one_filesystem,
        threads: options.effective_threads(),
        backend,
        excluder: &excluder,
        oracle,
        progress,
        descend,
    })?;
    let after = progress.snapshot();
    Ok(ScanOutcome {
        tree,
        issues,
        elapsed: start.elapsed(),
        backend,
        stats: ProgressSnapshot {
            dirs: after.dirs.saturating_sub(before.dirs),
            files: after.files.saturating_sub(before.files),
            bytes: after.bytes.saturating_sub(before.bytes),
            errors: after.errors.saturating_sub(before.errors),
        },
    })
}

/// Rescans one directory and returns a standalone tree rooted at `dir`.
///
/// Used for incremental updates and for lazily expanding a summarized node. The oracle is not
/// consulted for `dir` itself (it is always materialized), only for its descendants. To replace
/// an existing node, remove it and graft the result under its parent:
///
/// ```no_run
/// # use bclean_scan::{NoPrune, ScanOptions, ScanProgress, scan, scan_subtree};
/// # let options = ScanOptions::new("/data");
/// # let progress = ScanProgress::new();
/// # let mut tree = scan(&options, &NoPrune, &progress)?.tree;
/// # let node = tree.find(std::path::Path::new("/data/node_modules")).unwrap();
/// let path = tree.path(node);
/// let parent = tree.parent(node).unwrap();
/// let sub = scan_subtree(&path, &options, &NoPrune, &progress)?;
/// tree.remove_subtree(node);
/// tree.graft(parent, &sub.tree);
/// # Ok::<(), bclean_scan::ScanError>(())
/// ```
///
/// Hard-link deduplication only spans the rescanned subtree, and `one_filesystem` is relative to
/// the device of `dir`.
///
/// # Errors
///
/// * Same as [`scan`]
pub fn scan_subtree(
    dir: &Path,
    options: &ScanOptions,
    oracle: &dyn PruneOracle,
    progress: &ScanProgress,
) -> Result<ScanOutcome, ScanError> {
    scan(&options.with_root(dir), oracle, progress)
}

/// Lists `dir` alone: a tree of `dir` and its direct entries, without reading any
/// subdirectory.
///
/// Excludes, filesystem boundaries and the `oracle` apply to the entries as in [`scan`], so
/// flags match a full scan, but every subdirectory is a leaf carrying only its own (directory
/// entry) sizes. Subdirectories the oracle would summarize are flagged
/// [`bclean_tree::NodeFlags::SUMMARIZED`] without being measured. Hard-link deduplication
/// only spans this one listing.
///
/// Used to reconcile a directory whose entries changed without rescanning its subtree.
///
/// # Errors
///
/// * Same as [`scan`]
pub fn scan_shallow(
    dir: &Path,
    options: &ScanOptions,
    oracle: &dyn PruneOracle,
    progress: &ScanProgress,
) -> Result<ScanOutcome, ScanError> {
    scan_impl(
        &options.with_root(dir).with_threads(Some(1)),
        oracle,
        progress,
        false,
    )
}
