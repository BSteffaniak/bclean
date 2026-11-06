//! Blocking tree maintenance: the rules prune oracle, scanning, grafting rescanned
//! subtrees, and re-classification of affected subtrees.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bclean_model::Classification;
use bclean_rules::{Classifier, RuleSet};
use bclean_scan::{ListedEntry, NoPrune, PruneOracle, ScanError, ScanOptions, ScanProgress};
use bclean_staleness::{StalenessEngine, unix_now};
use bclean_tree::{EntryKind, EntryMeta, NodeFlags, NodeId, Tree};

/// [`PruneOracle`] backed by a compiled [`RuleSet`]: summarizes directories whose winning rule
/// is ephemeral. Inactive unless `scan.summarize_ephemeral` is set.
#[derive(Debug, Clone)]
pub struct RulesOracle {
    rules: Arc<RuleSet>,
    active: bool,
}

impl RulesOracle {
    /// Creates an oracle; `active` should be `scan.summarize_ephemeral`.
    #[must_use]
    pub const fn new(rules: Arc<RuleSet>, active: bool) -> Self {
        Self { rules, active }
    }
}

impl PruneOracle for RulesOracle {
    fn summarize(&self, dir: &Path, entries: &[ListedEntry<'_>]) -> Vec<usize> {
        let mut out = Vec::new();
        if self.active {
            self.rules
                .ephemeral_children_by(dir, entries, |e| e.name, |e| e.kind, &mut out);
        }
        out
    }

    fn is_noop(&self) -> bool {
        !self.active
    }
}

/// What a refresh did.
#[derive(Debug)]
pub enum Refresh {
    /// The whole root must be rescanned.
    Full,
    /// Subtrees rooted at these paths were replaced or re-summarized.
    Partial {
        /// Paths of replaced subtrees.
        affected: Vec<PathBuf>,
        /// The last rescanned subtree, rooted at its path (for TUI expansion).
        subtree: Option<Box<Tree>>,
    },
}

/// Scanner context for refreshes.
pub struct ScanCtx<'a> {
    /// Scan options (the root is replaced per call).
    pub options: &'a ScanOptions,
    /// Prune oracle.
    pub oracle: &'a RulesOracle,
    /// Progress counters.
    pub progress: &'a ScanProgress,
}

/// Rescans `dirs` inside `tree`, grafting the results. With `expand`, summarized directories
/// are materialized instead of re-summarized.
///
/// # Errors
///
/// * Any non-root [`ScanError`] from the scanner
pub fn refresh_dirs(
    tree: &mut Tree,
    dirs: &[PathBuf],
    expand: bool,
    ctx: &ScanCtx<'_>,
) -> Result<Refresh, ScanError> {
    let root = tree.root_path().to_path_buf();
    let mut affected = Vec::new();
    let mut subtree = None;
    for dir in dirs {
        if !dir.starts_with(&root) {
            continue;
        }
        if dir == &root {
            return Ok(Refresh::Full);
        }
        let Some((node, _rest)) = tree.find_nearest(dir) else {
            return Ok(Refresh::Full);
        };
        if !tree.contains(node) {
            continue;
        }
        let summarized = std::iter::once(node)
            .chain(tree.ancestors(node))
            .rfind(|n| tree.flags(*n).contains(NodeFlags::SUMMARIZED));
        let done = match summarized {
            Some(s) if !expand => resummarize(tree, s, ctx)?,
            _ => rescan_node(tree, node, ctx)?,
        };
        match done {
            None => return Ok(Refresh::Full),
            Some((path, sub)) => {
                affected.push(path);
                if sub.is_some() {
                    subtree = sub;
                }
            }
        }
    }
    Ok(Refresh::Partial { affected, subtree })
}

type Done = Option<(PathBuf, Option<Box<Tree>>)>;

/// Rescans `node` (climbing to the parent while it no longer exists). `None` = full rescan.
fn rescan_node(tree: &mut Tree, mut node: NodeId, ctx: &ScanCtx<'_>) -> Result<Done, ScanError> {
    loop {
        let Some(parent) = tree.parent(node) else {
            return Ok(None);
        };
        let path = tree.path(node);
        match bclean_scan::scan_subtree(&path, ctx.options, ctx.oracle, ctx.progress) {
            Ok(out) => {
                tree.remove_subtree(node);
                tree.graft(parent, &out.tree);
                return Ok(Some((path, Some(Box::new(out.tree)))));
            }
            Err(ScanError::Root { .. }) => node = parent,
            Err(e) => return Err(e),
        }
    }
}

/// Re-measures a summarized directory without materializing it.
fn resummarize(tree: &mut Tree, node: NodeId, ctx: &ScanCtx<'_>) -> Result<Done, ScanError> {
    let path = tree.path(node);
    match bclean_scan::scan_subtree(&path, ctx.options, &NoPrune, ctx.progress) {
        Ok(out) => {
            let info = out.tree.info(NodeId::ROOT);
            let mut meta = EntryMeta::new(
                EntryKind::Dir,
                info.total_apparent,
                info.total_allocated,
                info.max_mtime,
            )
            .with_flags(NodeFlags::SUMMARIZED);
            meta.items = info.items;
            tree.update_own(node, meta);
            Ok(Some((path, None)))
        }
        Err(ScanError::Root { .. }) => tree
            .parent(node)
            .map_or(Ok(None), |parent| rescan_node(tree, parent, ctx)),
        Err(e) => Err(e),
    }
}

/// Classifies the whole tree.
#[must_use]
pub fn classify_all(rules: &RuleSet, tree: &Tree) -> BTreeMap<PathBuf, Classification> {
    let staleness = StalenessEngine::with_defaults(unix_now());
    Classifier::new(rules, &staleness)
        .classify_tree(tree)
        .into_iter()
        .map(|e| (e.path, e.classification))
        .collect()
}

/// Re-classifies the subtrees around `affected` paths, replacing their entries in `classes`.
///
/// Each affected path is widened to its nearest classified ancestor (whose size changed).
pub fn reclassify(
    rules: &RuleSet,
    tree: &Tree,
    classes: &mut BTreeMap<PathBuf, Classification>,
    affected: &[PathBuf],
) {
    let mut starts: Vec<PathBuf> = affected
        .iter()
        .map(|path| {
            path.ancestors()
                .find(|a| classes.contains_key(*a))
                .map_or_else(|| path.clone(), Path::to_path_buf)
        })
        .collect();
    starts.sort();
    starts.dedup();
    let mut outer: Vec<PathBuf> = Vec::new();
    for start in starts {
        if !outer.iter().any(|o| start.starts_with(o)) {
            outer.push(start);
        }
    }
    let staleness = StalenessEngine::with_defaults(unix_now());
    let classifier = Classifier::new(rules, &staleness);
    for start in outer {
        classes.retain(|path, _| !path.starts_with(&start));
        if let Some(node) = tree.find(&start) {
            for entry in classifier.classify_subtree(tree, node) {
                classes.insert(entry.path, entry.classification);
            }
        }
    }
}
