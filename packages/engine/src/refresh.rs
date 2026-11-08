//! Blocking tree maintenance: the rules prune oracle, scanning, grafting rescanned
//! subtrees, and re-classification of affected subtrees.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// How [`refresh_dirs`] reconciles each path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// The directory's own entries changed: re-list it, update changed entries, drop removed
    /// ones, and scan only new subdirectories. Unchanged subtrees are never read.
    Shallow,
    /// Rescan each subtree completely. With `expand`, summarized directories are
    /// materialized instead of re-summarized.
    Deep {
        /// Materialize summarized directories.
        expand: bool,
    },
}

/// What a refresh did.
#[derive(Debug)]
pub enum Refresh {
    /// The whole root must be rescanned.
    Full,
    /// Subtrees rooted at these paths were replaced or re-summarized.
    Partial {
        /// Paths whose classification may have changed.
        affected: Vec<PathBuf>,
        /// The last rescanned subtree, rooted at its path (for TUI expansion).
        subtree: Option<Box<Tree>>,
        /// Expensive directories whose refresh was postponed by the caller's `allow`
        /// (shallow mode only).
        deferred: Vec<PathBuf>,
        /// Expensive refreshes that ran, with how long each took (shallow mode only).
        costly: Vec<Costly>,
    },
}

/// An expensive directory refresh, which the caller may rate-limit (see `allow` on
/// [`refresh_dirs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Costly {
    /// The directory.
    pub path: PathBuf,
    /// How long it took.
    pub took: Duration,
    /// A summarized directory re-measured, rather than a large directory re-listed.
    pub summarized: bool,
}

/// Re-listing a directory that takes at least this long is reported as [`Costly`]: a temp
/// directory with tens of thousands of entries takes seconds to list, and busy ones change
/// every few seconds.
pub const SLOW_LISTING: Duration = Duration::from_millis(100);

/// Scanner context for refreshes.
pub struct ScanCtx<'a> {
    /// Scan options (the root is replaced per call).
    pub options: &'a ScanOptions,
    /// Prune oracle.
    pub oracle: &'a RulesOracle,
    /// Progress counters.
    pub progress: &'a ScanProgress,
}

/// Reconciles `dirs` inside `tree` according to `mode`.
///
/// A path inside a summarized directory re-measures that directory. Re-measuring it, or
/// re-listing any directory, happens only when `allow` returns `true` for it; otherwise it is
/// reported in [`Refresh::Partial::deferred`] (shallow mode only, so explicit rescans are
/// never postponed). Expensive refreshes are reported in [`Refresh::Partial::costly`], so the
/// caller can postpone the next one.
///
/// # Errors
///
/// * Any non-root [`ScanError`] from the scanner
pub fn refresh_dirs(
    tree: &mut Tree,
    dirs: &[PathBuf],
    mode: RefreshMode,
    ctx: &ScanCtx<'_>,
    allow: &mut dyn FnMut(&Path) -> bool,
) -> Result<Refresh, ScanError> {
    match mode {
        RefreshMode::Shallow => refresh_shallow(tree, dirs, ctx, allow),
        RefreshMode::Deep { expand } => refresh_deep(tree, dirs, expand, ctx),
    }
}

fn refresh_shallow(
    tree: &mut Tree,
    dirs: &[PathBuf],
    ctx: &ScanCtx<'_>,
    allow: &mut dyn FnMut(&Path) -> bool,
) -> Result<Refresh, ScanError> {
    let root = tree.root_path().to_path_buf();
    let mut affected = Vec::new();
    let mut deferred = Vec::new();
    let mut costly = Vec::new();
    let mut summarized_done: Vec<NodeId> = Vec::new();
    let mut listed: BTreeSet<NodeId> = BTreeSet::new();
    for dir in dirs {
        if !dir.starts_with(&root) {
            continue;
        }
        // A directory missing from the tree is new: its nearest known ancestor lists it.
        let Some((mut node, _rest)) = tree.find_nearest(dir) else {
            continue;
        };
        if tree.kind(node) != EntryKind::Dir {
            match tree.parent(node) {
                Some(parent) => node = parent,
                None => continue,
            }
        }
        let summarized = std::iter::once(node)
            .chain(tree.ancestors(node))
            .rfind(|n| tree.flags(*n).contains(NodeFlags::SUMMARIZED));
        if let Some(s) = summarized {
            if summarized_done.contains(&s) {
                continue;
            }
            summarized_done.push(s);
            let path = tree.path(s);
            if !allow(&path) {
                deferred.push(path);
                continue;
            }
            let started = Instant::now();
            match resummarize(tree, s, ctx)? {
                Some((path, _)) => {
                    costly.push(Costly {
                        path: path.clone(),
                        took: started.elapsed(),
                        summarized: true,
                    });
                    affected.push(path);
                }
                None => return Ok(Refresh::Full),
            }
            continue;
        }
        if tree
            .flags(node)
            .intersects(NodeFlags::EXCLUDED.union(NodeFlags::OTHER_FS))
            || !listed.insert(node)
        {
            continue;
        }
        let path = tree.path(node);
        if !allow(&path) {
            deferred.push(path);
            continue;
        }
        let started = Instant::now();
        match reconcile_dir(tree, node, ctx)? {
            Some(changed) => affected.extend(changed),
            None => return Ok(Refresh::Full),
        }
        let took = started.elapsed();
        if took >= SLOW_LISTING {
            costly.push(Costly {
                path,
                took,
                summarized: false,
            });
        }
    }
    Ok(Refresh::Partial {
        affected,
        subtree: None,
        deferred,
        costly,
    })
}

/// Re-lists one directory and applies the difference to its children. Climbs to the parent
/// while the directory no longer exists. Returns the paths of changed entries, or `None` when
/// the root itself is gone (full rescan).
fn reconcile_dir(
    tree: &mut Tree,
    mut node: NodeId,
    ctx: &ScanCtx<'_>,
) -> Result<Option<Vec<PathBuf>>, ScanError> {
    let (path, fresh) = loop {
        let path = tree.path(node);
        match bclean_scan::scan_shallow(&path, ctx.options, ctx.oracle, ctx.progress) {
            Ok(out) => break (path, out.tree),
            Err(ScanError::Root { .. }) => match tree.parent(node) {
                Some(parent) => node = parent,
                None => return Ok(None),
            },
            Err(e) => return Err(e),
        }
    };
    if fresh.kind(NodeId::ROOT) != EntryKind::Dir {
        // Replaced by a file: re-list the parent, which swaps the node.
        return tree
            .parent(node)
            .map_or(Ok(None), |parent| reconcile_dir(tree, parent, ctx));
    }

    let mut existing: BTreeMap<OsString, NodeId> = tree
        .children(node)
        .map(|c| (tree.name(c).to_os_string(), c))
        .collect();
    let mut changed = Vec::new();
    for child in fresh.children(NodeId::ROOT) {
        let name = fresh.name(child);
        let new = fresh.info(child);
        // Shallow listings never measure subdirectories, so every entry counts one item.
        let mut meta = EntryMeta::new(new.kind, new.own_apparent, new.own_allocated, new.mtime)
            .with_flags(new.flags);
        let structural = NodeFlags::SUMMARIZED
            .union(NodeFlags::EXCLUDED)
            .union(NodeFlags::OTHER_FS);
        if let Some(old) = existing.remove(name) {
            let old_info = tree.info(old);
            let same_shape = old_info.kind == new.kind
                && old_info.flags.intersection(structural) == new.flags.intersection(structural);
            if same_shape && new.kind == EntryKind::Dir {
                // Unchanged subtree: its own events cover its contents. Refresh the directory
                // entry itself unless it is summarized (whose own sizes are the summary).
                if !new.flags.contains(NodeFlags::SUMMARIZED) && own_differs(&old_info, &meta) {
                    meta.flags = meta
                        .flags
                        .union(old_info.flags.intersection(NodeFlags::ERROR));
                    tree.update_own(old, meta);
                }
                continue;
            }
            if same_shape {
                // Hard-link dedupe spans the whole scan, not this listing: keep a duplicate's
                // zeroed allocation.
                if old_info.flags.contains(NodeFlags::HARDLINK_DUP) {
                    meta.allocated = 0;
                    meta.flags = meta.flags.union(NodeFlags::HARDLINK_DUP);
                }
                if own_differs(&old_info, &meta) || old_info.flags != meta.flags {
                    tree.update_own(old, meta);
                    changed.push(path.join(name));
                }
                continue;
            }
            tree.remove_subtree(old);
        }
        changed.push(path.join(name));
        add_entry(tree, node, &path, name, meta, ctx)?;
    }
    for (name, old) in existing {
        tree.remove_subtree(old);
        changed.push(path.join(name));
    }
    let own = fresh.info(NodeId::ROOT);
    let meta = EntryMeta::new(
        EntryKind::Dir,
        own.own_apparent,
        own.own_allocated,
        own.mtime,
    )
    .with_flags(tree.flags(node).without(NodeFlags::ERROR));
    tree.update_own(node, meta);
    Ok(Some(changed))
}

/// Whether a listed entry's own metadata differs from the node's.
fn own_differs(old: &bclean_tree::NodeInfo, new: &EntryMeta) -> bool {
    (old.mtime, old.own_apparent, old.own_allocated) != (new.mtime, new.apparent, new.allocated)
}

/// Adds one new entry under `parent`, scanning it if it is a walkable directory.
fn add_entry(
    tree: &mut Tree,
    parent: NodeId,
    parent_path: &Path,
    name: &std::ffi::OsStr,
    meta: EntryMeta,
    ctx: &ScanCtx<'_>,
) -> Result<(), ScanError> {
    let walkable = meta.kind == EntryKind::Dir
        && !meta.flags.intersects(
            NodeFlags::EXCLUDED
                .union(NodeFlags::OTHER_FS)
                .union(NodeFlags::ERROR),
        );
    if !walkable {
        tree.add_child(parent, name, meta);
        return Ok(());
    }
    if meta.flags.contains(NodeFlags::SUMMARIZED) {
        let id = tree.add_child(parent, name, meta);
        if resummarize(tree, id, ctx)?.is_none() {
            tree.remove_subtree(id);
        }
        return Ok(());
    }
    match bclean_scan::scan_subtree(
        &parent_path.join(name),
        ctx.options,
        ctx.oracle,
        ctx.progress,
    ) {
        Ok(out) => {
            tree.graft(parent, &out.tree);
        }
        // Vanished between the listing and the scan; the next event re-lists the parent.
        Err(ScanError::Root { .. }) => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

fn refresh_deep(
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
    Ok(Refresh::Partial {
        affected,
        subtree,
        deferred: Vec::new(),
        costly: Vec::new(),
    })
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
    // Paths order component-wise, so a path's descendants directly follow it.
    let mut outer: Vec<PathBuf> = Vec::new();
    for start in starts {
        if !outer.last().is_some_and(|o| start.starts_with(o)) {
            outer.push(start);
        }
    }
    let staleness = StalenessEngine::with_defaults(unix_now());
    let classifier = Classifier::new(rules, &staleness);
    for start in outer {
        remove_under(classes, &start);
        if let Some(node) = tree.find(&start) {
            for entry in classifier.classify_subtree(tree, node) {
                classes.insert(entry.path, entry.classification);
            }
        }
    }
}

/// Removes `start` and every path below it from `map`.
///
/// Paths order component-wise, so they form one contiguous range: a busy directory changes
/// hundreds of paths per batch, and scanning the whole map for each was quadratic.
fn remove_under<V>(map: &mut BTreeMap<PathBuf, V>, start: &Path) {
    let doomed: Vec<PathBuf> = map
        .range::<Path, _>((Bound::Included(start), Bound::Unbounded))
        .map(|(path, _)| path)
        .take_while(|path| path.starts_with(start))
        .cloned()
        .collect();
    for path in doomed {
        map.remove(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_under_takes_exactly_the_subtree() {
        let paths = [
            "/a", "/a/b", "/a/b/x", "/a/b/x/y", "/a/b ", "/a/bc", "/a/c", "/b",
        ];
        let mut map: BTreeMap<PathBuf, usize> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (PathBuf::from(p), i))
            .collect();
        remove_under(&mut map, Path::new("/a/b"));
        let left: Vec<&str> = map.keys().map(|p| p.to_str().unwrap()).collect();
        assert_eq!(left, ["/a", "/a/b ", "/a/bc", "/a/c", "/b"]);
        remove_under(&mut map, Path::new("/missing"));
        assert_eq!(map.len(), 5);
    }
}
