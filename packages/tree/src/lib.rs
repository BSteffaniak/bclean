#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Compact arena-backed filesystem tree with aggregated sizes.
//!
//! The tree stores nodes in struct-of-arrays form addressed by [`NodeId`] (`u32`) with all
//! names interned into a single byte arena. Each node tracks its *own* sizes (the file size
//! for files, the directory entry size for directories, or the whole subtree size for
//! [`NodeFlags::SUMMARIZED`] directories whose children were not materialized) as well as
//! *subtree totals* that are kept consistent either by [`Tree::recompute_aggregates`] after a
//! bulk build or incrementally by the mutation methods.
//!
//! Mutation is single-threaded by design. Parallel scanners build independent subtrees and
//! attach them with [`Tree::graft`], or stream batches into a single builder thread.

use std::cmp::Reverse;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// Sentinel used for "no node" links in the arena.
const NONE: u32 = u32::MAX;

/// Identifier of a node within a [`Tree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// The root node of every tree.
    pub const ROOT: Self = Self(0);

    /// Creates a node id from a raw index.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw arena index.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Kind of filesystem entry a node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EntryKind {
    /// Regular file.
    File = 0,
    /// Directory.
    Dir = 1,
    /// Symbolic link (never followed).
    Symlink = 2,
    /// Anything else (sockets, FIFOs, devices).
    Other = 3,
}

impl EntryKind {
    const fn from_u8(value: u8) -> Self {
        match value & 0b11 {
            0 => Self::File,
            1 => Self::Dir,
            2 => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// Per-node status flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeFlags(u8);

impl NodeFlags {
    /// No flags set.
    pub const EMPTY: Self = Self(0);
    /// Directory whose children were not materialized; its own sizes are the subtree totals.
    pub const SUMMARIZED: Self = Self(1);
    /// Reading this entry or directory failed (permissions, races, I/O errors).
    pub const ERROR: Self = Self(1 << 1);
    /// Mount point of another filesystem that was not descended into.
    pub const OTHER_FS: Self = Self(1 << 2);
    /// Hard link whose data is already counted by another node (allocated size is zero).
    pub const HARDLINK_DUP: Self = Self(1 << 3);
    /// Excluded by configuration; contents not scanned.
    pub const EXCLUDED: Self = Self(1 << 4);
    /// Node was removed and its slot is awaiting reuse or compaction.
    pub const REMOVED: Self = Self(1 << 7);

    /// Returns `true` if every flag in `other` is set in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns the union of both flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns `self` with the flags in `other` cleared.
    #[must_use]
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Returns the raw bit representation.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Creates flags from raw bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
}

/// Metadata for an entry being inserted into a tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryMeta {
    /// Entry kind.
    pub kind: EntryKind,
    /// Apparent (logical) size in bytes.
    pub apparent: u64,
    /// Allocated (on-disk) size in bytes.
    pub allocated: u64,
    /// Modification time in seconds since the Unix epoch (0 if unknown).
    pub mtime: i64,
    /// Number of entries this node accounts for itself (1, or the item count of a
    /// summarized directory's whole subtree).
    pub items: u64,
    /// Status flags.
    pub flags: NodeFlags,
}

impl EntryMeta {
    /// Metadata for a plain entry with the given kind and sizes.
    #[must_use]
    pub const fn new(kind: EntryKind, apparent: u64, allocated: u64, mtime: i64) -> Self {
        Self {
            kind,
            apparent,
            allocated,
            mtime,
            items: 1,
            flags: NodeFlags::EMPTY,
        }
    }

    /// Metadata for an empty directory placeholder.
    #[must_use]
    pub const fn dir() -> Self {
        Self::new(EntryKind::Dir, 0, 0, 0)
    }

    /// Returns a copy with additional flags set.
    #[must_use]
    pub const fn with_flags(mut self, flags: NodeFlags) -> Self {
        self.flags = self.flags.union(flags);
        self
    }
}

/// Read-only snapshot of a node's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeInfo {
    /// Entry kind.
    pub kind: EntryKind,
    /// Status flags.
    pub flags: NodeFlags,
    /// Own apparent size.
    pub own_apparent: u64,
    /// Own allocated size.
    pub own_allocated: u64,
    /// Subtree apparent size (including own).
    pub total_apparent: u64,
    /// Subtree allocated size (including own).
    pub total_allocated: u64,
    /// Number of entries in the subtree (including self).
    pub items: u64,
    /// Own modification time.
    pub mtime: i64,
    /// Newest modification time anywhere in the subtree.
    pub max_mtime: i64,
}

/// Which size measure to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeMode {
    /// Allocated (on-disk) bytes; what deleting actually frees.
    #[default]
    Allocated,
    /// Apparent (logical) bytes.
    Apparent,
}

/// Ordering for [`Tree::sorted_children`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Largest subtree first.
    Size(SizeMode),
    /// Name ascending (byte order).
    Name,
    /// Most items first.
    Items,
    /// Most recently modified subtree first.
    Mtime,
}

/// Raw struct-of-arrays representation of a tree, for persistence.
///
/// Every vector has one element per node except `names`, which is the interned byte arena.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeParts {
    /// Absolute path of the root node.
    pub root_path: PathBuf,
    /// Interned name bytes.
    pub names: Vec<u8>,
    /// Start offset of each node's name in `names`.
    pub name_off: Vec<u64>,
    /// Byte length of each node's name.
    pub name_len: Vec<u32>,
    /// Parent index (`u32::MAX` for the root).
    pub parent: Vec<u32>,
    /// First child index (`u32::MAX` if none).
    pub first_child: Vec<u32>,
    /// Next sibling index (`u32::MAX` if none).
    pub next_sibling: Vec<u32>,
    /// [`EntryKind`] discriminant.
    pub kind: Vec<u8>,
    /// [`NodeFlags`] bits.
    pub flags: Vec<u8>,
    /// Own apparent size.
    pub own_apparent: Vec<u64>,
    /// Own allocated size.
    pub own_allocated: Vec<u64>,
    /// Own item count.
    pub own_items: Vec<u64>,
    /// Own modification time.
    pub mtime: Vec<i64>,
}

/// Error returned by [`Tree::from_parts`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartsError {
    /// Per-node vectors have differing lengths.
    #[error("tree part vectors have inconsistent lengths")]
    LengthMismatch,
    /// The tree has no root node.
    #[error("tree has no root node")]
    Empty,
    /// A link or name range points outside the arena.
    #[error("tree node {0} has an out-of-range link or name")]
    OutOfRange(u32),
    /// The tree has more nodes than `u32` can address.
    #[error("tree has too many nodes")]
    TooLarge,
}

/// Compact arena-backed filesystem tree.
#[derive(Debug, Clone)]
pub struct Tree {
    root_path: PathBuf,
    names: Vec<u8>,
    name_off: Vec<u64>,
    name_len: Vec<u32>,
    parent: Vec<u32>,
    first_child: Vec<u32>,
    next_sibling: Vec<u32>,
    kind: Vec<u8>,
    flags: Vec<u8>,
    own_apparent: Vec<u64>,
    own_allocated: Vec<u64>,
    own_items: Vec<u64>,
    mtime: Vec<i64>,
    total_apparent: Vec<u64>,
    total_allocated: Vec<u64>,
    items: Vec<u64>,
    max_mtime: Vec<i64>,
    free: Vec<u32>,
}

impl Tree {
    /// Creates a tree containing only a root node for `root_path`.
    #[must_use]
    pub fn new(root_path: impl Into<PathBuf>, root: EntryMeta) -> Self {
        let root_path = root_path.into();
        let mut tree = Self {
            root_path,
            names: Vec::new(),
            name_off: Vec::new(),
            name_len: Vec::new(),
            parent: Vec::new(),
            first_child: Vec::new(),
            next_sibling: Vec::new(),
            kind: Vec::new(),
            flags: Vec::new(),
            own_apparent: Vec::new(),
            own_allocated: Vec::new(),
            own_items: Vec::new(),
            mtime: Vec::new(),
            total_apparent: Vec::new(),
            total_allocated: Vec::new(),
            items: Vec::new(),
            max_mtime: Vec::new(),
            free: Vec::new(),
        };
        let name = tree.root_path.file_name().map_or_else(
            || tree.root_path.clone().into_os_string(),
            ToOwned::to_owned,
        );
        tree.push_node(NONE, &name, root);
        tree
    }

    /// Creates a tree with capacity reserved for `nodes` nodes and `name_bytes` name bytes.
    #[must_use]
    pub fn with_capacity(
        root_path: impl Into<PathBuf>,
        root: EntryMeta,
        nodes: usize,
        name_bytes: usize,
    ) -> Self {
        let mut tree = Self::new(root_path, root);
        tree.reserve(nodes, name_bytes);
        tree
    }

    /// Reserves capacity for additional nodes and name bytes.
    pub fn reserve(&mut self, nodes: usize, name_bytes: usize) {
        self.names.reserve(name_bytes);
        self.name_off.reserve(nodes);
        self.name_len.reserve(nodes);
        self.parent.reserve(nodes);
        self.first_child.reserve(nodes);
        self.next_sibling.reserve(nodes);
        self.kind.reserve(nodes);
        self.flags.reserve(nodes);
        self.own_apparent.reserve(nodes);
        self.own_allocated.reserve(nodes);
        self.own_items.reserve(nodes);
        self.mtime.reserve(nodes);
        self.total_apparent.reserve(nodes);
        self.total_allocated.reserve(nodes);
        self.items.reserve(nodes);
        self.max_mtime.reserve(nodes);
    }

    /// Absolute path of the root node.
    #[must_use]
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Number of arena slots, including removed nodes awaiting reuse.
    #[must_use]
    pub const fn capacity_nodes(&self) -> usize {
        self.parent.len()
    }

    /// Number of live nodes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.parent.len() - self.free.len()
    }

    /// Always `false`: a tree has at least a root.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Returns `true` if `id` refers to a live node.
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        id.index() < self.parent.len() && !self.flags(id).contains(NodeFlags::REMOVED)
    }

    fn push_node(&mut self, parent: u32, name: &OsStr, meta: EntryMeta) -> NodeId {
        let bytes = name_bytes(name);
        let name_len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let off = self.names.len() as u64;
        self.names.extend_from_slice(&bytes[..name_len as usize]);

        if let Some(slot) = self.free.pop() {
            let i = slot as usize;
            self.name_off[i] = off;
            self.name_len[i] = name_len;
            self.parent[i] = parent;
            self.first_child[i] = NONE;
            self.next_sibling[i] = NONE;
            self.kind[i] = meta.kind as u8;
            self.flags[i] = meta.flags.bits();
            self.own_apparent[i] = meta.apparent;
            self.own_allocated[i] = meta.allocated;
            self.own_items[i] = meta.items;
            self.mtime[i] = meta.mtime;
            self.total_apparent[i] = meta.apparent;
            self.total_allocated[i] = meta.allocated;
            self.items[i] = meta.items;
            self.max_mtime[i] = meta.mtime;
            return NodeId(slot);
        }

        let id = u32::try_from(self.parent.len()).expect("tree exceeds u32::MAX nodes");
        assert!(id != NONE, "tree exceeds u32::MAX nodes");
        self.name_off.push(off);
        self.name_len.push(name_len);
        self.parent.push(parent);
        self.first_child.push(NONE);
        self.next_sibling.push(NONE);
        self.kind.push(meta.kind as u8);
        self.flags.push(meta.flags.bits());
        self.own_apparent.push(meta.apparent);
        self.own_allocated.push(meta.allocated);
        self.own_items.push(meta.items);
        self.mtime.push(meta.mtime);
        self.total_apparent.push(meta.apparent);
        self.total_allocated.push(meta.allocated);
        self.items.push(meta.items);
        self.max_mtime.push(meta.mtime);
        NodeId(id)
    }

    /// Adds a child without updating ancestor aggregates.
    ///
    /// Use during bulk builds and call [`Tree::recompute_aggregates`] once at the end.
    ///
    /// # Panics
    ///
    /// * If `parent` is not a live node
    /// * If the tree would exceed `u32::MAX - 1` nodes
    pub fn add_child_raw(&mut self, parent: NodeId, name: &OsStr, meta: EntryMeta) -> NodeId {
        assert!(self.contains(parent), "parent {parent:?} is not live");
        let id = self.push_node(parent.0, name, meta);
        self.next_sibling[id.index()] = self.first_child[parent.index()];
        self.first_child[parent.index()] = id.0;
        id
    }

    /// Adds a child and incrementally updates every ancestor's aggregates.
    ///
    /// # Panics
    ///
    /// * If `parent` is not a live node
    pub fn add_child(&mut self, parent: NodeId, name: &OsStr, meta: EntryMeta) -> NodeId {
        let id = self.add_child_raw(parent, name, meta);
        self.propagate_up(
            parent,
            i128::from(meta.apparent),
            i128::from(meta.allocated),
            i128::from(meta.items),
            meta.mtime,
        );
        id
    }

    /// Copies `sub` (all of its live nodes) under `parent`, returning the new id of `sub`'s root.
    /// Ancestor aggregates are updated incrementally; `sub`'s own aggregates are trusted.
    ///
    /// # Panics
    ///
    /// * If `parent` is not a live node
    pub fn graft(&mut self, parent: NodeId, sub: &Self) -> NodeId {
        self.reserve(sub.len(), sub.names.len());
        let root_meta = sub.entry_meta(NodeId::ROOT);
        let new_root = self.add_child_raw(parent, sub.name(NodeId::ROOT), root_meta);
        self.copy_aggregates_from(new_root, sub, NodeId::ROOT);

        let mut stack = vec![(NodeId::ROOT, new_root)];
        while let Some((src, dst)) = stack.pop() {
            for child in sub.children(src) {
                let id = self.add_child_raw(dst, sub.name(child), sub.entry_meta(child));
                self.copy_aggregates_from(id, sub, child);
                stack.push((child, id));
            }
        }

        let info = sub.info(NodeId::ROOT);
        self.propagate_up(
            parent,
            i128::from(info.total_apparent),
            i128::from(info.total_allocated),
            i128::from(info.items),
            info.max_mtime,
        );
        new_root
    }

    fn copy_aggregates_from(&mut self, dst: NodeId, sub: &Self, src: NodeId) {
        let (d, s) = (dst.index(), src.index());
        self.total_apparent[d] = sub.total_apparent[s];
        self.total_allocated[d] = sub.total_allocated[s];
        self.items[d] = sub.items[s];
        self.max_mtime[d] = sub.max_mtime[s];
    }

    fn entry_meta(&self, id: NodeId) -> EntryMeta {
        let i = id.index();
        EntryMeta {
            kind: EntryKind::from_u8(self.kind[i]),
            apparent: self.own_apparent[i],
            allocated: self.own_allocated[i],
            mtime: self.mtime[i],
            items: self.own_items[i],
            flags: NodeFlags(self.flags[i]),
        }
    }

    /// Applies a size/item delta to `start` and every ancestor, and raises `max_mtime`.
    ///
    /// `max_mtime` only grows here; call [`Tree::recompute_aggregates`] to lower it after removals.
    fn propagate_up(
        &mut self,
        start: NodeId,
        d_app: i128,
        d_alloc: i128,
        d_items: i128,
        mtime: i64,
    ) {
        let mut cur = start.0;
        while cur != NONE {
            let i = cur as usize;
            self.total_apparent[i] = apply_delta(self.total_apparent[i], d_app);
            self.total_allocated[i] = apply_delta(self.total_allocated[i], d_alloc);
            self.items[i] = apply_delta(self.items[i], d_items);
            if mtime > self.max_mtime[i] {
                self.max_mtime[i] = mtime;
            }
            cur = self.parent[i];
        }
    }

    /// Updates a node's own metadata, propagating the size delta to ancestors.
    ///
    /// # Panics
    ///
    /// * If `id` is not a live node
    pub fn update_own(&mut self, id: NodeId, meta: EntryMeta) {
        assert!(self.contains(id), "node {id:?} is not live");
        let i = id.index();
        let d_app = i128::from(meta.apparent) - i128::from(self.own_apparent[i]);
        let d_alloc = i128::from(meta.allocated) - i128::from(self.own_allocated[i]);
        let d_items = i128::from(meta.items) - i128::from(self.own_items[i]);
        self.kind[i] = meta.kind as u8;
        self.flags[i] = meta.flags.bits();
        self.own_apparent[i] = meta.apparent;
        self.own_allocated[i] = meta.allocated;
        self.own_items[i] = meta.items;
        self.mtime[i] = meta.mtime;
        self.propagate_up(id, d_app, d_alloc, d_items, meta.mtime);
    }

    /// Sets flags on a node without touching sizes.
    pub fn insert_flags(&mut self, id: NodeId, flags: NodeFlags) {
        let i = id.index();
        self.flags[i] = NodeFlags(self.flags[i]).union(flags).bits();
    }

    /// Clears flags on a node without touching sizes.
    pub fn remove_flags(&mut self, id: NodeId, flags: NodeFlags) {
        let i = id.index();
        self.flags[i] = NodeFlags(self.flags[i]).without(flags).bits();
    }

    /// Removes `id` and its whole subtree, updating ancestor aggregates.
    ///
    /// Slots are recycled by later insertions; call [`Tree::compact`] to reclaim name bytes.
    ///
    /// # Panics
    ///
    /// * If `id` is the root or not a live node
    pub fn remove_subtree(&mut self, id: NodeId) {
        assert!(id != NodeId::ROOT, "cannot remove the root node");
        assert!(self.contains(id), "node {id:?} is not live");
        let info = self.info(id);
        let parent = NodeId(self.parent[id.index()]);
        self.unlink(parent, id);
        self.propagate_up(
            parent,
            -i128::from(info.total_apparent),
            -i128::from(info.total_allocated),
            -i128::from(info.items),
            i64::MIN,
        );
        self.free_subtree(id);
    }

    /// Removes all children of `id` (keeping `id`), updating ancestor aggregates.
    pub fn clear_children(&mut self, id: NodeId) {
        let children: Vec<NodeId> = self.children(id).collect();
        for child in children {
            self.remove_subtree(child);
        }
    }

    fn unlink(&mut self, parent: NodeId, id: NodeId) {
        let p = parent.index();
        if self.first_child[p] == id.0 {
            self.first_child[p] = self.next_sibling[id.index()];
            return;
        }
        let mut cur = self.first_child[p];
        while cur != NONE {
            let next = self.next_sibling[cur as usize];
            if next == id.0 {
                self.next_sibling[cur as usize] = self.next_sibling[id.index()];
                return;
            }
            cur = next;
        }
    }

    fn free_subtree(&mut self, id: NodeId) {
        let mut stack = vec![id.0];
        while let Some(cur) = stack.pop() {
            let i = cur as usize;
            let mut child = self.first_child[i];
            while child != NONE {
                stack.push(child);
                child = self.next_sibling[child as usize];
            }
            self.flags[i] = NodeFlags::REMOVED.bits();
            self.parent[i] = NONE;
            self.first_child[i] = NONE;
            self.next_sibling[i] = NONE;
            self.name_len[i] = 0;
            self.free.push(cur);
        }
    }

    /// Recomputes every node's subtree aggregates from own values (iterative post-order).
    pub fn recompute_aggregates(&mut self) {
        let order = self.post_order(NodeId::ROOT);
        for id in order {
            let i = id.index();
            let mut app = self.own_apparent[i];
            let mut alloc = self.own_allocated[i];
            let mut items = self.own_items[i];
            let mut mtime = self.mtime[i];
            let mut child = self.first_child[i];
            while child != NONE {
                let c = child as usize;
                app = app.saturating_add(self.total_apparent[c]);
                alloc = alloc.saturating_add(self.total_allocated[c]);
                items = items.saturating_add(self.items[c]);
                mtime = mtime.max(self.max_mtime[c]);
                child = self.next_sibling[c];
            }
            self.total_apparent[i] = app;
            self.total_allocated[i] = alloc;
            self.items[i] = items;
            self.max_mtime[i] = mtime;
        }
    }

    /// Returns `start` and its descendants in post-order (children before parents).
    #[must_use]
    pub fn post_order(&self, start: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![(start.0, false)];
        while let Some((cur, expanded)) = stack.pop() {
            if expanded {
                out.push(NodeId(cur));
                continue;
            }
            stack.push((cur, true));
            let mut child = self.first_child[cur as usize];
            while child != NONE {
                stack.push((child, false));
                child = self.next_sibling[child as usize];
            }
        }
        out
    }

    /// Iterates `start` and its descendants in pre-order (depth-first).
    #[must_use]
    pub fn descendants(&self, start: NodeId) -> Descendants<'_> {
        Descendants {
            tree: self,
            stack: vec![start.0],
        }
    }

    /// Iterates the direct children of `id` (unspecified order).
    #[must_use]
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            tree: self,
            next: self.first_child[id.index()],
        }
    }

    /// Returns `true` if `id` has at least one materialized child.
    #[must_use]
    pub fn has_children(&self, id: NodeId) -> bool {
        self.first_child[id.index()] != NONE
    }

    /// Returns the direct children of `id` sorted by `key`.
    #[must_use]
    pub fn sorted_children(&self, id: NodeId, key: SortKey) -> Vec<NodeId> {
        let mut children: Vec<NodeId> = self.children(id).collect();
        match key {
            SortKey::Size(SizeMode::Allocated) => {
                children.sort_by_key(|c| (Reverse(self.total_allocated[c.index()]), c.0));
            }
            SortKey::Size(SizeMode::Apparent) => {
                children.sort_by_key(|c| (Reverse(self.total_apparent[c.index()]), c.0));
            }
            SortKey::Items => children.sort_by_key(|c| (Reverse(self.items[c.index()]), c.0)),
            SortKey::Mtime => children.sort_by_key(|c| (Reverse(self.max_mtime[c.index()]), c.0)),
            SortKey::Name => children.sort_by(|a, b| self.name_raw(*a).cmp(self.name_raw(*b))),
        }
        children
    }

    /// Parent of `id`, or `None` for the root.
    #[must_use]
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.parent[id.index()];
        (p != NONE).then_some(NodeId(p))
    }

    /// Ancestors of `id` from its parent up to the root.
    #[must_use]
    pub fn ancestors(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut cur = self.parent(id);
        while let Some(p) = cur {
            out.push(p);
            cur = self.parent(p);
        }
        out
    }

    fn name_raw(&self, id: NodeId) -> &[u8] {
        let i = id.index();
        let start = usize::try_from(self.name_off[i]).unwrap_or(usize::MAX);
        let end = start.saturating_add(self.name_len[i] as usize);
        self.names.get(start..end).unwrap_or_default()
    }

    /// Name of the node (the root's name is the last component of the root path).
    #[must_use]
    pub fn name(&self, id: NodeId) -> &OsStr {
        bytes_to_os_str(self.name_raw(id))
    }

    /// Absolute path of the node.
    #[must_use]
    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while let Some(parent) = self.parent(cur) {
            parts.push(cur);
            cur = parent;
        }
        let mut path = self.root_path.clone();
        for part in parts.iter().rev() {
            path.push(self.name(*part));
        }
        path
    }

    /// Path of the node relative to the root.
    #[must_use]
    pub fn relative_path(&self, id: NodeId) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        while let Some(parent) = self.parent(cur) {
            parts.push(cur);
            cur = parent;
        }
        parts.iter().rev().map(|p| self.name(*p)).collect()
    }

    /// Finds a direct child of `parent` by name.
    #[must_use]
    pub fn child_by_name(&self, parent: NodeId, name: &OsStr) -> Option<NodeId> {
        let wanted = name_bytes(name);
        self.children(parent)
            .find(|c| self.name_raw(*c) == wanted.as_ref())
    }

    /// Finds the node for an absolute path (or a path relative to the root).
    #[must_use]
    pub fn find(&self, path: &Path) -> Option<NodeId> {
        let rel = if path.is_absolute() {
            path.strip_prefix(&self.root_path).ok()?
        } else {
            path
        };
        let mut cur = NodeId::ROOT;
        for component in rel.components() {
            match component {
                Component::Normal(name) => cur = self.child_by_name(cur, name)?,
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(cur)
    }

    /// Finds the deepest existing node on the way to `path`, plus the remaining components.
    #[must_use]
    pub fn find_nearest(&self, path: &Path) -> Option<(NodeId, PathBuf)> {
        let rel = path.strip_prefix(&self.root_path).ok()?;
        let mut cur = NodeId::ROOT;
        let mut components = rel.components();
        while let Some(component) = components.next() {
            if let Component::Normal(name) = component {
                if let Some(next) = self.child_by_name(cur, name) {
                    cur = next;
                } else {
                    let mut rest = PathBuf::from(name);
                    rest.extend(components);
                    return Some((cur, rest));
                }
            }
        }
        Some((cur, PathBuf::new()))
    }

    /// Entry kind of a node.
    #[must_use]
    pub fn kind(&self, id: NodeId) -> EntryKind {
        EntryKind::from_u8(self.kind[id.index()])
    }

    /// Flags of a node.
    #[must_use]
    pub fn flags(&self, id: NodeId) -> NodeFlags {
        NodeFlags(self.flags[id.index()])
    }

    /// Subtree size of a node in the requested mode.
    #[must_use]
    pub fn size(&self, id: NodeId, mode: SizeMode) -> u64 {
        match mode {
            SizeMode::Allocated => self.total_allocated[id.index()],
            SizeMode::Apparent => self.total_apparent[id.index()],
        }
    }

    /// Full data snapshot of a node.
    #[must_use]
    pub fn info(&self, id: NodeId) -> NodeInfo {
        let i = id.index();
        NodeInfo {
            kind: EntryKind::from_u8(self.kind[i]),
            flags: NodeFlags(self.flags[i]),
            own_apparent: self.own_apparent[i],
            own_allocated: self.own_allocated[i],
            total_apparent: self.total_apparent[i],
            total_allocated: self.total_allocated[i],
            items: self.items[i],
            mtime: self.mtime[i],
            max_mtime: self.max_mtime[i],
        }
    }

    /// Returns a copy without removed slots or orphaned name bytes. Node ids change.
    #[must_use]
    pub fn compact(&self) -> Self {
        let root = self.entry_meta(NodeId::ROOT);
        let mut out =
            Self::with_capacity(self.root_path.clone(), root, self.len(), self.names.len());
        out.copy_aggregates_from(NodeId::ROOT, self, NodeId::ROOT);
        let mut stack = vec![(NodeId::ROOT, NodeId::ROOT)];
        while let Some((src, dst)) = stack.pop() {
            // Preserve sibling order: children are prepended, so insert in reverse.
            let mut children: Vec<NodeId> = self.children(src).collect();
            children.reverse();
            for child in children {
                let id = out.add_child_raw(dst, self.name(child), self.entry_meta(child));
                out.copy_aggregates_from(id, self, child);
                stack.push((child, id));
            }
        }
        out
    }

    /// Converts the tree into its raw parts (compacting first if it has removed slots).
    #[must_use]
    pub fn into_parts(self) -> TreeParts {
        let tree = if self.free.is_empty() {
            self
        } else {
            self.compact()
        };
        TreeParts {
            root_path: tree.root_path,
            names: tree.names,
            name_off: tree.name_off,
            name_len: tree.name_len,
            parent: tree.parent,
            first_child: tree.first_child,
            next_sibling: tree.next_sibling,
            kind: tree.kind,
            flags: tree.flags,
            own_apparent: tree.own_apparent,
            own_allocated: tree.own_allocated,
            own_items: tree.own_items,
            mtime: tree.mtime,
        }
    }

    /// Rebuilds a tree from raw parts, validating links and recomputing aggregates.
    ///
    /// # Errors
    ///
    /// * [`PartsError::LengthMismatch`] if per-node vectors differ in length
    /// * [`PartsError::Empty`] if there are no nodes
    /// * [`PartsError::TooLarge`] if there are more nodes than `u32` can address
    /// * [`PartsError::OutOfRange`] if a link or name range is invalid, or the links contain a cycle
    pub fn from_parts(parts: TreeParts) -> Result<Self, PartsError> {
        let n = parts.parent.len();
        let lens = [
            parts.name_off.len(),
            parts.name_len.len(),
            parts.first_child.len(),
            parts.next_sibling.len(),
            parts.kind.len(),
            parts.flags.len(),
            parts.own_apparent.len(),
            parts.own_allocated.len(),
            parts.own_items.len(),
            parts.mtime.len(),
        ];
        if lens.iter().any(|l| *l != n) {
            return Err(PartsError::LengthMismatch);
        }
        if n == 0 {
            return Err(PartsError::Empty);
        }
        if u32::try_from(n).map_or(true, |n| n == NONE) {
            return Err(PartsError::TooLarge);
        }
        let in_range = |v: u32| v == NONE || (v as usize) < n;
        for i in 0..n {
            let idx = u32::try_from(i).unwrap_or(NONE);
            let end = parts.name_off[i].checked_add(u64::from(parts.name_len[i]));
            let name_ok = end.is_some_and(|end| end <= parts.names.len() as u64);
            if !name_ok
                || !in_range(parts.parent[i])
                || !in_range(parts.first_child[i])
                || !in_range(parts.next_sibling[i])
            {
                return Err(PartsError::OutOfRange(idx));
            }
        }
        if parts.parent[0] != NONE {
            return Err(PartsError::OutOfRange(0));
        }
        // Every node must be reached exactly once from the root via child links.
        let mut seen = vec![false; n];
        let mut stack = vec![0_u32];
        seen[0] = true;
        while let Some(cur) = stack.pop() {
            let mut child = parts.first_child[cur as usize];
            while child != NONE {
                let c = child as usize;
                if seen[c] || parts.parent[c] != cur {
                    return Err(PartsError::OutOfRange(child));
                }
                seen[c] = true;
                stack.push(child);
                child = parts.next_sibling[c];
            }
        }
        let free: Vec<u32> = (0..n)
            .filter(|i| !seen[*i])
            .map(|i| u32::try_from(i).unwrap_or(NONE))
            .collect();

        let mut tree = Self {
            root_path: parts.root_path,
            names: parts.names,
            name_off: parts.name_off,
            name_len: parts.name_len,
            parent: parts.parent,
            first_child: parts.first_child,
            next_sibling: parts.next_sibling,
            kind: parts.kind,
            flags: parts.flags,
            total_apparent: parts.own_apparent.clone(),
            total_allocated: parts.own_allocated.clone(),
            items: parts.own_items.clone(),
            max_mtime: parts.mtime.clone(),
            own_apparent: parts.own_apparent,
            own_allocated: parts.own_allocated,
            own_items: parts.own_items,
            mtime: parts.mtime,
            free,
        };
        for slot in tree.free.clone() {
            tree.flags[slot as usize] = NodeFlags::REMOVED.bits();
        }
        tree.recompute_aggregates();
        Ok(tree)
    }
}

/// Iterator over a node's direct children.
pub struct Children<'a> {
    tree: &'a Tree,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let id = NodeId(self.next);
        self.next = self.tree.next_sibling[id.index()];
        Some(id)
    }
}

/// Pre-order iterator over a subtree.
pub struct Descendants<'a> {
    tree: &'a Tree,
    stack: Vec<u32>,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        let cur = self.stack.pop()?;
        let mut child = self.tree.first_child[cur as usize];
        while child != NONE {
            self.stack.push(child);
            child = self.tree.next_sibling[child as usize];
        }
        Some(NodeId(cur))
    }
}

fn apply_delta(value: u64, delta: i128) -> u64 {
    let next = i128::from(value) + delta;
    u64::try_from(next.max(0)).unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn name_bytes(name: &OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt as _;
    std::borrow::Cow::Borrowed(name.as_bytes())
}

#[cfg(unix)]
fn bytes_to_os_str(bytes: &[u8]) -> &OsStr {
    use std::os::unix::ffi::OsStrExt as _;
    OsStr::from_bytes(bytes)
}

// Non-Unix names are stored as UTF-8; unpaired UTF-16 surrogates are replaced lossily.
#[cfg(not(unix))]
fn name_bytes(name: &OsStr) -> std::borrow::Cow<'_, [u8]> {
    match name.to_string_lossy() {
        std::borrow::Cow::Borrowed(s) => std::borrow::Cow::Borrowed(s.as_bytes()),
        std::borrow::Cow::Owned(s) => std::borrow::Cow::Owned(s.into_bytes()),
    }
}

#[cfg(not(unix))]
fn bytes_to_os_str(bytes: &[u8]) -> &OsStr {
    OsStr::new(std::str::from_utf8(bytes).unwrap_or("\u{FFFD}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64, mtime: i64) -> EntryMeta {
        EntryMeta::new(EntryKind::File, size, size, mtime)
    }

    fn sample() -> (Tree, NodeId, NodeId, NodeId) {
        let mut tree = Tree::new("/root", EntryMeta::dir());
        let a = tree.add_child_raw(NodeId::ROOT, OsStr::new("a"), EntryMeta::dir());
        let f1 = tree.add_child_raw(a, OsStr::new("f1"), file(100, 10));
        let f2 = tree.add_child_raw(NodeId::ROOT, OsStr::new("f2"), file(50, 20));
        tree.recompute_aggregates();
        (tree, a, f1, f2)
    }

    #[test]
    fn aggregates_after_bulk_build() {
        let (tree, a, _, _) = sample();
        assert_eq!(tree.size(NodeId::ROOT, SizeMode::Allocated), 150);
        assert_eq!(tree.size(a, SizeMode::Apparent), 100);
        assert_eq!(tree.info(NodeId::ROOT).items, 4);
        assert_eq!(tree.info(NodeId::ROOT).max_mtime, 20);
    }

    #[test]
    fn incremental_add_and_remove() {
        let (mut tree, a, f1, _) = sample();
        tree.add_child(a, OsStr::new("f3"), file(25, 30));
        assert_eq!(tree.size(NodeId::ROOT, SizeMode::Allocated), 175);
        assert_eq!(tree.info(a).max_mtime, 30);
        tree.remove_subtree(f1);
        assert_eq!(tree.size(a, SizeMode::Allocated), 25);
        assert_eq!(tree.len(), 4);
        tree.remove_subtree(a);
        assert_eq!(tree.size(NodeId::ROOT, SizeMode::Allocated), 50);
        assert_eq!(tree.len(), 2);
    }

    #[test]
    fn update_own_propagates() {
        let (mut tree, a, f1, _) = sample();
        tree.update_own(f1, file(10, 10));
        assert_eq!(tree.size(a, SizeMode::Allocated), 10);
        assert_eq!(tree.size(NodeId::ROOT, SizeMode::Allocated), 60);
    }

    #[test]
    fn paths_and_find() {
        let (tree, a, f1, _) = sample();
        assert_eq!(tree.path(f1), PathBuf::from("/root/a/f1"));
        assert_eq!(tree.relative_path(f1), PathBuf::from("a/f1"));
        assert_eq!(tree.find(Path::new("/root/a/f1")), Some(f1));
        assert_eq!(tree.find(Path::new("a")), Some(a));
        assert_eq!(tree.find(Path::new("/root/missing")), None);
        assert_eq!(
            tree.find_nearest(Path::new("/root/a/x/y")),
            Some((a, PathBuf::from("x/y")))
        );
    }

    #[test]
    fn sorted_children_by_size_and_name() {
        let (tree, a, _, f2) = sample();
        assert_eq!(
            tree.sorted_children(NodeId::ROOT, SortKey::Size(SizeMode::Allocated)),
            vec![a, f2]
        );
        assert_eq!(
            tree.sorted_children(NodeId::ROOT, SortKey::Name),
            vec![a, f2]
        );
        assert_eq!(
            tree.sorted_children(NodeId::ROOT, SortKey::Mtime),
            vec![f2, a]
        );
    }

    #[test]
    fn graft_subtree() {
        let (mut tree, a, _, _) = sample();
        let mut sub = Tree::new("/elsewhere/sub", EntryMeta::dir());
        let inner = sub.add_child_raw(NodeId::ROOT, OsStr::new("inner"), EntryMeta::dir());
        sub.add_child_raw(inner, OsStr::new("big"), file(1000, 5));
        sub.recompute_aggregates();
        let grafted = tree.graft(a, &sub);
        assert_eq!(tree.path(grafted), PathBuf::from("/root/a/sub"));
        assert_eq!(tree.size(NodeId::ROOT, SizeMode::Allocated), 1150);
        assert!(tree.find(Path::new("/root/a/sub/inner/big")).is_some());
    }

    #[test]
    fn slots_are_reused() {
        let (mut tree, _, f1, _) = sample();
        let before = tree.capacity_nodes();
        tree.remove_subtree(f1);
        tree.add_child(NodeId::ROOT, OsStr::new("new"), file(1, 1));
        assert_eq!(tree.capacity_nodes(), before);
    }

    #[test]
    fn parts_round_trip_and_compaction() {
        let (mut tree, _, f1, _) = sample();
        tree.remove_subtree(f1);
        let parts = tree.clone().into_parts();
        assert_eq!(parts.parent.len(), 3);
        let restored = Tree::from_parts(parts).unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(restored.size(NodeId::ROOT, SizeMode::Allocated), 50);
        assert!(restored.find(Path::new("/root/f2")).is_some());
        assert!(restored.find(Path::new("/root/a")).is_some());
    }

    #[test]
    fn from_parts_rejects_cycles() {
        let (tree, _, _, _) = sample();
        let mut parts = tree.into_parts();
        parts.next_sibling[1] = 1;
        assert!(Tree::from_parts(parts).is_err());
    }

    #[test]
    fn descendants_and_post_order() {
        let (tree, a, f1, _) = sample();
        let pre: Vec<_> = tree.descendants(a).collect();
        assert_eq!(pre, vec![a, f1]);
        let post = tree.post_order(NodeId::ROOT);
        assert_eq!(post.last(), Some(&NodeId::ROOT));
        assert_eq!(post.len(), 4);
    }
}
