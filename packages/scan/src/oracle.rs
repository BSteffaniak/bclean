//! Caller-injected pruning decisions.

use std::ffi::OsStr;
use std::path::Path;

use bclean_tree::EntryKind;

/// One entry of a freshly listed directory, as shown to a [`PruneOracle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedEntry<'a> {
    /// Entry name.
    pub name: &'a OsStr,
    /// Entry kind (symlinks are reported as [`EntryKind::Symlink`], never as their target).
    pub kind: EntryKind,
}

/// Decides which child directories are *summarized* instead of materialized.
///
/// A summarized directory is still walked completely (sizes, item counts and newest mtime are
/// exact) but its descendants are not added to the tree: it becomes a single node flagged
/// [`bclean_tree::NodeFlags::SUMMARIZED`] whose own sizes and item count are the subtree totals
/// and whose own mtime is the newest mtime in the subtree. This keeps memory small for huge
/// regenerable trees such as `node_modules/` or `target/`; callers expand one lazily with
/// [`crate::scan_subtree`] and [`bclean_tree::Tree::graft`].
///
/// The oracle is consulted once per materialized directory, from worker threads, and never
/// inside summarized subtrees. It must be cheap: it sits on the scan's hot path.
pub trait PruneOracle: Send + Sync {
    /// Given the directory `dir` and all of its `entries` (files included, so decisions can
    /// depend on siblings such as `Cargo.toml` next to `target/`), returns indices into
    /// `entries` of the directories to summarize. Indices of non-directories are ignored.
    fn summarize(&self, dir: &Path, entries: &[ListedEntry<'_>]) -> Vec<usize>;

    /// `true` if this oracle never summarizes anything (lets the scanner skip building the
    /// entry slice). Defaults to `false`.
    fn is_noop(&self) -> bool {
        false
    }
}

/// Oracle that never summarizes: every directory is materialized.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPrune;

impl PruneOracle for NoPrune {
    fn summarize(&self, _dir: &Path, _entries: &[ListedEntry<'_>]) -> Vec<usize> {
        Vec::new()
    }

    fn is_noop(&self) -> bool {
        true
    }
}

impl<F> PruneOracle for F
where
    F: Fn(&Path, &[ListedEntry<'_>]) -> Vec<usize> + Send + Sync,
{
    fn summarize(&self, dir: &Path, entries: &[ListedEntry<'_>]) -> Vec<usize> {
        self(dir, entries)
    }
}
