//! Non-rendering data logic: classification index, sorting, lenses, and listing caches.
//!
//! Nothing here clones the tree. Listings are `Vec<NodeId>` computed on demand for one
//! directory (or one lens) and cached until the tree, the classifications, or the view options
//! change.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bclean_model::{Classification, EphemeralKind};
use bclean_tree::{NodeId, SizeMode, SortKey, Tree};

/// Column the listing is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SortBy {
    /// Largest first.
    #[default]
    Size,
    /// Name ascending.
    Name,
    /// Most items first.
    Items,
    /// Most recently modified first.
    Mtime,
    /// Highest cleanup score first.
    Score,
}

impl SortBy {
    /// Label for the status bar.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Name => "name",
            Self::Items => "items",
            Self::Mtime => "mtime",
            Self::Score => "score",
        }
    }
}

/// Sort column plus direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SortSpec {
    /// Column.
    pub by: SortBy,
    /// Reverse the column's natural order.
    pub reverse: bool,
}

impl SortSpec {
    /// Selects `by`; selecting the active column again flips the direction.
    #[must_use]
    pub const fn toggled(self, by: SortBy) -> Self {
        if self.by as u8 == by as u8 {
            Self {
                by,
                reverse: !self.reverse,
            }
        } else {
            Self { by, reverse: false }
        }
    }
}

/// What the main list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Lens {
    /// The plain directory browser.
    #[default]
    All,
    /// Ephemeral (regenerable) items across the whole tree.
    Ephemeral,
    /// Stale (unused) items across the whole tree.
    Stale,
    /// Everything actionable across the whole tree, by score.
    Reclaimable,
    /// Items that grew since a baseline. Reserved: needs index history from the backend.
    GrewSince,
}

impl Lens {
    /// All lenses in tab order.
    pub const ALL: [Self; 5] = [
        Self::All,
        Self::Ephemeral,
        Self::Stale,
        Self::Reclaimable,
        Self::GrewSince,
    ];

    /// Stable id.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Ephemeral => "ephemeral",
            Self::Stale => "stale",
            Self::Reclaimable => "reclaimable",
            Self::GrewSince => "grew",
        }
    }

    /// Tab label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "1 All",
            Self::Ephemeral => "2 Ephemeral",
            Self::Stale => "3 Stale",
            Self::Reclaimable => "4 Reclaimable",
            Self::GrewSince => "5 Grew since…",
        }
    }

    /// Whether the lens is implemented (the "grew since" hook is not yet).
    #[must_use]
    pub const fn available(self) -> bool {
        !matches!(self, Self::GrewSince)
    }

    /// Whether this lens shows a flattened cross-tree list instead of a directory.
    #[must_use]
    pub const fn is_flat(self) -> bool {
        !matches!(self, Self::All)
    }

    /// Whether `class` belongs in this lens.
    #[must_use]
    pub fn matches(self, class: &Classification) -> bool {
        match self {
            Self::All => true,
            Self::Ephemeral => class.ephemeral.is_some(),
            Self::Stale => class.staleness.stale,
            Self::Reclaimable => is_reclaimable(class),
            Self::GrewSince => false,
        }
    }

    /// Next/previous available lens.
    #[must_use]
    pub fn cycle(self, backwards: bool) -> Self {
        let available: Vec<Self> = Self::ALL.into_iter().filter(|l| l.available()).collect();
        let pos = available.iter().position(|l| *l == self).unwrap_or(0);
        let len = available.len();
        available[if backwards {
            (pos + len - 1) % len
        } else {
            (pos + 1) % len
        }]
    }
}

/// Whether a classification describes something worth cleaning.
#[must_use]
pub fn is_reclaimable(class: &Classification) -> bool {
    class.is_reclaimable()
}

/// Classifications keyed by absolute path, with a node index for the current tree.
#[derive(Debug, Default)]
pub struct ClassStore {
    by_path: BTreeMap<PathBuf, Arc<Classification>>,
    by_node: BTreeMap<NodeId, Arc<Classification>>,
    /// Reclaimable bytes contained strictly inside each directory (top-most matches only).
    reclaimable_inside: BTreeMap<NodeId, u64>,
    generation: u64,
}

impl ClassStore {
    /// Bumped on every change; part of listing cache keys.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of known classifications.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// Whether no classification is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// Classification of a node in the current tree.
    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&Arc<Classification>> {
        self.by_node.get(&id)
    }

    /// Classification by absolute path.
    #[must_use]
    pub fn get_path(&self, path: &Path) -> Option<&Arc<Classification>> {
        self.by_path.get(path)
    }

    /// Reclaimable bytes inside a directory (excluding the node's own classification).
    #[must_use]
    pub fn reclaimable_inside(&self, id: NodeId) -> u64 {
        self.reclaimable_inside.get(&id).copied().unwrap_or(0)
    }

    /// Classified nodes of the current tree.
    pub fn nodes(&self) -> impl Iterator<Item = (NodeId, &Arc<Classification>)> {
        self.by_node.iter().map(|(id, c)| (*id, c))
    }

    /// Adds or replaces classifications and indexes them against `tree`.
    pub fn insert(&mut self, tree: Option<&Tree>, items: Vec<(PathBuf, Classification)>) {
        for (path, class) in items {
            let class = Arc::new(class);
            if let Some(id) = tree.and_then(|t| t.find(&path)) {
                self.by_node.insert(id, Arc::clone(&class));
            }
            self.by_path.insert(path, class);
        }
        if let Some(tree) = tree {
            self.rebuild_aggregates(tree);
        }
        self.generation += 1;
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.by_path.clear();
        self.by_node.clear();
        self.reclaimable_inside.clear();
        self.generation += 1;
    }

    /// Re-resolves every path against a new (or mutated) tree.
    pub fn reindex(&mut self, tree: &Tree) {
        self.by_node.clear();
        for (path, class) in &self.by_path {
            if let Some(id) = tree.find(path) {
                self.by_node.insert(id, Arc::clone(class));
            }
        }
        self.rebuild_aggregates(tree);
        self.generation += 1;
    }

    fn rebuild_aggregates(&mut self, tree: &Tree) {
        self.reclaimable_inside.clear();
        for (id, class) in &self.by_node {
            if !tree.contains(*id) || !is_reclaimable(class) {
                continue;
            }
            let ancestors = tree.ancestors(*id);
            // Only count top-most reclaimable matches so nested matches aren't double counted.
            if ancestors
                .iter()
                .any(|a| self.by_node.get(a).is_some_and(|c| is_reclaimable(c)))
            {
                continue;
            }
            let bytes = class.size;
            for ancestor in ancestors {
                let entry = self.reclaimable_inside.entry(ancestor).or_insert(0);
                *entry = entry.saturating_add(bytes);
            }
        }
    }
}

/// Restricts a flat lens to one ephemeral kind or rule category (a dashboard drill-down).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LensFilter {
    /// Only this ephemeral kind (`None` = durable items).
    Kind(Option<EphemeralKind>),
    /// Only this rule category (`None` = uncategorized).
    Category(Option<String>),
}

impl LensFilter {
    /// Whether `class` passes the filter.
    #[must_use]
    pub fn matches(&self, class: &Classification) -> bool {
        match self {
            Self::Kind(kind) => class.ephemeral.as_ref().map(|e| e.kind) == *kind,
            Self::Category(category) => class.category == *category,
        }
    }

    /// Short label for headers.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Kind(kind) => bclean_model::kind_label(*kind).to_owned(),
            Self::Category(category) => category
                .clone()
                .unwrap_or_else(|| "uncategorized".to_owned()),
        }
    }
}

/// Inputs that determine a listing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ListingKey {
    /// Directory (ignored for flat lenses).
    pub dir: NodeId,
    /// Lens.
    pub lens: Lens,
    /// Sort.
    pub sort: SortSpec,
    /// Size mode used for size sorting.
    pub apparent: bool,
    /// Whether dotfiles are shown.
    pub show_hidden: bool,
    /// Case-insensitive name filter.
    pub filter: String,
    /// Kind/category restriction of flat lenses.
    pub class_filter: Option<LensFilter>,
    /// Tree generation.
    pub tree_gen: u64,
    /// Classification generation (only relevant for score sort and flat lenses).
    pub class_gen: u64,
}

/// Bounded cache of computed listings.
#[derive(Debug, Default)]
pub struct ListingCache {
    entries: BTreeMap<ListingKey, Arc<Vec<NodeId>>>,
    order: Vec<ListingKey>,
    /// Number of listings computed (for tests and diagnostics).
    pub computed: usize,
}

const CACHE_CAPACITY: usize = 32;

impl ListingCache {
    /// Returns the cached listing for `key`, computing it if needed.
    pub fn get(&mut self, key: &ListingKey, tree: &Tree, classes: &ClassStore) -> Arc<Vec<NodeId>> {
        if let Some(found) = self.entries.get(key) {
            return Arc::clone(found);
        }
        let listing = Arc::new(compute_listing(key, tree, classes));
        self.computed += 1;
        // Entries for an older tree/class generation can never be hit again.
        self.entries
            .retain(|k, _| k.tree_gen == key.tree_gen && k.class_gen == key.class_gen);
        self.order.retain(|k| self.entries.contains_key(k));
        if self.order.len() >= CACHE_CAPACITY {
            let oldest = self.order.remove(0);
            self.entries.remove(&oldest);
        }
        self.order.push(key.clone());
        self.entries.insert(key.clone(), Arc::clone(&listing));
        listing
    }

    /// Drops everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

fn is_hidden(tree: &Tree, id: NodeId) -> bool {
    tree.name(id).as_encoded_bytes().first() == Some(&b'.')
}

fn name_matches(tree: &Tree, id: NodeId, filter_lower: &str) -> bool {
    filter_lower.is_empty()
        || tree
            .name(id)
            .to_string_lossy()
            .to_lowercase()
            .contains(filter_lower)
}

/// Computes one listing (uncached).
#[must_use]
pub fn compute_listing(key: &ListingKey, tree: &Tree, classes: &ClassStore) -> Vec<NodeId> {
    let mode = if key.apparent {
        SizeMode::Apparent
    } else {
        SizeMode::Allocated
    };
    let filter = key.filter.to_lowercase();
    let mut rows: Vec<NodeId> = if key.lens.is_flat() {
        flat_lens(tree, classes, key.lens)
            .into_iter()
            .filter(|id| name_matches(tree, *id, &filter))
            .filter(|id| {
                key.class_filter
                    .as_ref()
                    .is_none_or(|f| classes.get(*id).is_some_and(|c| f.matches(c)))
            })
            .collect()
    } else {
        if !tree.contains(key.dir) {
            return Vec::new();
        }
        let tree_key = match key.sort.by {
            SortBy::Size | SortBy::Score => SortKey::Size(mode),
            SortBy::Name => SortKey::Name,
            SortBy::Items => SortKey::Items,
            SortBy::Mtime => SortKey::Mtime,
        };
        let mut children = tree.sorted_children(key.dir, tree_key);
        children.retain(|id| {
            (key.show_hidden || !is_hidden(tree, *id)) && name_matches(tree, *id, &filter)
        });
        if key.sort.by != SortBy::Score {
            if key.sort.reverse {
                children.reverse();
            }
            return children;
        }
        children
    };
    if key.lens.is_flat() || key.sort.by == SortBy::Score {
        sort_rows(&mut rows, tree, classes, key.sort, mode);
    }
    rows
}

/// Sorts rows in place by `sort` (used for flat lenses and score sorting).
pub fn sort_rows(
    rows: &mut [NodeId],
    tree: &Tree,
    classes: &ClassStore,
    sort: SortSpec,
    mode: SizeMode,
) {
    let score = |id: NodeId| classes.get(id).map_or(f64::NEG_INFINITY, |c| c.score);
    let size_desc = |a: NodeId, b: NodeId| tree.size(b, mode).cmp(&tree.size(a, mode));
    rows.sort_by(|&a, &b| {
        let primary = match sort.by {
            SortBy::Size => size_desc(a, b),
            SortBy::Name => tree
                .name(a)
                .as_encoded_bytes()
                .cmp(tree.name(b).as_encoded_bytes()),
            SortBy::Items => tree.info(b).items.cmp(&tree.info(a).items),
            SortBy::Mtime => tree.info(b).max_mtime.cmp(&tree.info(a).max_mtime),
            SortBy::Score => score(b).partial_cmp(&score(a)).unwrap_or(Ordering::Equal),
        };
        let primary = if sort.reverse {
            primary.reverse()
        } else {
            primary
        };
        primary.then_with(|| size_desc(a, b)).then(a.cmp(&b))
    });
}

/// Classified nodes matching `lens`, excluding nodes nested inside another match.
#[must_use]
pub fn flat_lens(tree: &Tree, classes: &ClassStore, lens: Lens) -> Vec<NodeId> {
    let matched = |id: NodeId| classes.get(id).is_some_and(|c| lens.matches(c));
    classes
        .nodes()
        .filter(|(id, class)| tree.contains(*id) && lens.matches(class))
        .filter(|(id, _)| !tree.ancestors(*id).into_iter().any(matched))
        .map(|(id, _)| id)
        .collect()
}

/// Drops paths that are descendants of other paths in the set (so a plan never acts twice).
#[must_use]
pub fn top_level_paths(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = paths.into_iter().collect();
    sorted.sort();
    sorted.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for path in sorted {
        if out.last().is_some_and(|last| path.starts_with(last)) {
            continue;
        }
        out.push(path);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsStr;

    use bclean_model::{Confidence, EphemeralKind, EphemeralSpec, RegenCost, Staleness};
    use bclean_tree::{EntryKind, EntryMeta};

    use super::*;
    use bclean_model::ActionSpec;

    pub fn file(size: u64, mtime: i64) -> EntryMeta {
        EntryMeta::new(EntryKind::File, size, size, mtime)
    }

    /// /r: a(dir: x 100, y 50), b 300, .hidden 10, c(dir: target(dir: o 500))
    pub fn sample_tree() -> Tree {
        let mut tree = Tree::new("/r", EntryMeta::dir());
        let a = tree.add_child_raw(NodeId::ROOT, OsStr::new("a"), EntryMeta::dir());
        tree.add_child_raw(a, OsStr::new("x"), file(100, 10));
        tree.add_child_raw(a, OsStr::new("y"), file(50, 20));
        tree.add_child_raw(NodeId::ROOT, OsStr::new("b"), file(300, 5));
        tree.add_child_raw(NodeId::ROOT, OsStr::new(".hidden"), file(10, 1));
        let c = tree.add_child_raw(NodeId::ROOT, OsStr::new("c"), EntryMeta::dir());
        let t = tree.add_child_raw(c, OsStr::new("target"), EntryMeta::dir());
        tree.add_child_raw(t, OsStr::new("o"), file(500, 30));
        tree.recompute_aggregates();
        tree
    }

    pub fn class(ephemeral: bool, stale: bool, score: f64, size: u64) -> Classification {
        Classification {
            rule_id: Some("test".into()),
            category: None,
            ephemeral: ephemeral.then_some(EphemeralSpec {
                kind: EphemeralKind::BuildArtifact,
                regen_cost: RegenCost::Cheap,
            }),
            confidence: Confidence::High,
            staleness: Staleness {
                stale,
                idle_secs: stale.then_some(86_400 * 40),
                ..Staleness::default()
            },
            size,
            score,
            action: ActionSpec::Trash,
            auto: false,
            reasons: Vec::new(),
        }
    }

    fn key(dir: NodeId, lens: Lens, sort: SortSpec) -> ListingKey {
        ListingKey {
            dir,
            lens,
            sort,
            apparent: false,
            show_hidden: false,
            filter: String::new(),
            class_filter: None,
            tree_gen: 0,
            class_gen: 0,
        }
    }

    fn names(tree: &Tree, rows: &[NodeId]) -> Vec<String> {
        rows.iter()
            .map(|id| tree.name(*id).to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn sorts_directory_children() {
        let tree = sample_tree();
        let store = ClassStore::default();
        let size = compute_listing(
            &key(NodeId::ROOT, Lens::All, SortSpec::default()),
            &tree,
            &store,
        );
        assert_eq!(names(&tree, &size), ["c", "b", "a"]);
        let rev = SortSpec::default().toggled(SortBy::Size);
        assert!(rev.reverse);
        let size_rev = compute_listing(&key(NodeId::ROOT, Lens::All, rev), &tree, &store);
        assert_eq!(names(&tree, &size_rev), ["a", "b", "c"]);
        let by_name = SortSpec::default().toggled(SortBy::Name);
        let name = compute_listing(&key(NodeId::ROOT, Lens::All, by_name), &tree, &store);
        assert_eq!(names(&tree, &name), ["a", "b", "c"]);
        let by_mtime = SortSpec::default().toggled(SortBy::Mtime);
        let mtime = compute_listing(&key(NodeId::ROOT, Lens::All, by_mtime), &tree, &store);
        assert_eq!(names(&tree, &mtime), ["c", "a", "b"]);
    }

    #[test]
    fn hidden_and_filter() {
        let tree = sample_tree();
        let store = ClassStore::default();
        let mut k = key(NodeId::ROOT, Lens::All, SortSpec::default());
        k.show_hidden = true;
        assert_eq!(compute_listing(&k, &tree, &store).len(), 4);
        k.filter = "B".into();
        assert_eq!(names(&tree, &compute_listing(&k, &tree, &store)), ["b"]);
    }

    #[test]
    fn score_sort_and_lenses() {
        let tree = sample_tree();
        let mut store = ClassStore::default();
        store.insert(
            Some(&tree),
            vec![
                (PathBuf::from("/r/c/target"), class(true, true, 9.0, 500)),
                (PathBuf::from("/r/c/target/o"), class(true, false, 1.0, 500)),
                (PathBuf::from("/r/b"), class(false, true, 3.0, 300)),
            ],
        );
        let by_score = SortSpec::default().toggled(SortBy::Score);
        let rows = compute_listing(&key(NodeId::ROOT, Lens::All, by_score), &tree, &store);
        assert_eq!(names(&tree, &rows)[0], "b");

        let eph = compute_listing(&key(NodeId::ROOT, Lens::Ephemeral, by_score), &tree, &store);
        // `o` is nested inside `target`, which also matches.
        assert_eq!(names(&tree, &eph), ["target"]);
        let stale = compute_listing(&key(NodeId::ROOT, Lens::Stale, by_score), &tree, &store);
        assert_eq!(names(&tree, &stale), ["target", "b"]);
        let recl = compute_listing(
            &key(NodeId::ROOT, Lens::Reclaimable, by_score),
            &tree,
            &store,
        );
        assert_eq!(names(&tree, &recl), ["target", "b"]);

        let c = tree.find(Path::new("/r/c")).unwrap();
        assert_eq!(store.reclaimable_inside(c), 500);
        assert_eq!(store.reclaimable_inside(NodeId::ROOT), 800);
    }

    #[test]
    fn cache_reuses_and_invalidates() {
        let tree = sample_tree();
        let store = ClassStore::default();
        let mut cache = ListingCache::default();
        let mut k = key(NodeId::ROOT, Lens::All, SortSpec::default());
        let first = cache.get(&k, &tree, &store);
        let second = cache.get(&k, &tree, &store);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.computed, 1);
        k.tree_gen = 1;
        let _ = cache.get(&k, &tree, &store);
        assert_eq!(cache.computed, 2);
        assert_eq!(cache.entries.len(), 1, "stale generations are evicted");
    }

    #[test]
    fn lens_cycle_skips_unavailable() {
        assert_eq!(Lens::Reclaimable.cycle(false), Lens::All);
        assert_eq!(Lens::All.cycle(true), Lens::Reclaimable);
    }

    #[test]
    fn top_level_dedupes_nested() {
        let out = top_level_paths([
            PathBuf::from("/a/b/c"),
            PathBuf::from("/a/b"),
            PathBuf::from("/a/bc"),
            PathBuf::from("/a/b"),
        ]);
        assert_eq!(out, [PathBuf::from("/a/b"), PathBuf::from("/a/bc")]);
    }
}
