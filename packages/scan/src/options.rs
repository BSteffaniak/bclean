//! Scan configuration.

use std::path::{Path, PathBuf};

use bclean_model::{ScanBackend, ScanConfig};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::ScanError;

/// Upper bound for the default worker count (explicit [`ScanOptions::threads`] are not capped).
pub const DEFAULT_MAX_THREADS: usize = 64;

/// Options for one scan.
///
/// Construct with [`ScanOptions::new`] or [`ScanOptions::from_config`] and adjust with the
/// `with_*` builder methods or by setting fields directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOptions {
    /// Directory (or file) to scan. Made absolute (without resolving symlinks) at scan start.
    pub root: PathBuf,
    /// Do not descend into directories on another filesystem; they are kept as leaf nodes
    /// flagged [`bclean_tree::NodeFlags::OTHER_FS`].
    pub one_filesystem: bool,
    /// Worker threads (`None` = see [`ScanOptions::effective_threads`]).
    pub threads: Option<usize>,
    /// Glob patterns excluded from scanning.
    ///
    /// Patterns without a path separator (e.g. `*.tmp`, `.git`) are matched against entry
    /// names; patterns containing `/` (e.g. `**/build/cache`, `/Users/*/Library`) are matched
    /// against full absolute paths. Excluded entries are kept as zero-size nodes flagged
    /// [`bclean_tree::NodeFlags::EXCLUDED`] and their contents are never read.
    pub exclude: Vec<String>,
    /// Requested metadata backend (see [`crate::resolve_backend`]).
    pub backend: ScanBackend,
}

impl ScanOptions {
    /// Default options for `root`: one filesystem, automatic threads and backend, no excludes.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            one_filesystem: true,
            threads: None,
            exclude: Vec::new(),
            backend: ScanBackend::Auto,
        }
    }

    /// Options for `root` taken from the user's [`ScanConfig`].
    ///
    /// `size_mode` and `summarize_ephemeral` do not affect the scanner itself: both sizes are
    /// always collected, and summarizing is driven by the caller's [`crate::PruneOracle`].
    #[must_use]
    pub fn from_config(root: impl Into<PathBuf>, config: &ScanConfig) -> Self {
        Self {
            root: root.into(),
            one_filesystem: config.one_filesystem,
            threads: config.threads,
            exclude: config.exclude.clone(),
            backend: config.backend,
        }
    }

    /// Sets [`ScanOptions::one_filesystem`].
    #[must_use]
    pub const fn with_one_filesystem(mut self, value: bool) -> Self {
        self.one_filesystem = value;
        self
    }

    /// Sets [`ScanOptions::threads`].
    #[must_use]
    pub const fn with_threads(mut self, threads: Option<usize>) -> Self {
        self.threads = threads;
        self
    }

    /// Sets [`ScanOptions::exclude`].
    #[must_use]
    pub fn with_exclude(mut self, exclude: Vec<String>) -> Self {
        self.exclude = exclude;
        self
    }

    /// Sets [`ScanOptions::backend`].
    #[must_use]
    pub const fn with_backend(mut self, backend: ScanBackend) -> Self {
        self.backend = backend;
        self
    }

    /// Returns a copy scanning `root` instead.
    #[must_use]
    pub fn with_root(&self, root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ..self.clone()
        }
    }

    /// Effective worker thread count: the explicit value (at least 1), or by default twice the
    /// available parallelism capped at [`DEFAULT_MAX_THREADS`].
    ///
    /// Oversubscription pays off because workers mostly wait inside the kernel (metadata I/O
    /// and filesystem locks) rather than burn user-space CPU; measured on APFS, 2x cores was
    /// ~15% faster than 1x on a warm cache and more on a cold one.
    #[must_use]
    pub fn effective_threads(&self) -> usize {
        self.threads.map_or_else(
            || {
                let cores = std::thread::available_parallelism().map_or(4, usize::from);
                (cores * 2).min(DEFAULT_MAX_THREADS)
            },
            |n| n.max(1),
        )
    }
}

/// Compiled exclude patterns.
#[derive(Debug)]
pub struct Excluder {
    names: GlobSet,
    paths: GlobSet,
}

impl Excluder {
    /// Compiles `patterns`.
    ///
    /// # Errors
    ///
    /// * [`ScanError::InvalidExclude`] if a pattern is not a valid glob
    pub fn new(patterns: &[String]) -> Result<Self, ScanError> {
        let mut names = GlobSetBuilder::new();
        let mut paths = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = Glob::new(pattern).map_err(|source| ScanError::InvalidExclude {
                pattern: pattern.clone(),
                source,
            })?;
            if pattern.contains('/') || pattern.contains(std::path::MAIN_SEPARATOR) {
                paths.add(glob);
            } else {
                names.add(glob);
            }
        }
        let build = |b: GlobSetBuilder| {
            b.build().map_err(|source| ScanError::InvalidExclude {
                pattern: patterns.join(", "),
                source,
            })
        };
        Ok(Self {
            names: build(names)?,
            paths: build(paths)?,
        })
    }

    /// `true` if no pattern is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.paths.is_empty()
    }

    /// `true` if the entry `name` inside `dir` is excluded.
    #[must_use]
    pub fn matches(&self, dir: &Path, name: &std::ffi::OsStr) -> bool {
        (!self.names.is_empty() && self.names.is_match(name))
            || (!self.paths.is_empty() && self.paths.is_match(dir.join(name)))
    }
}
