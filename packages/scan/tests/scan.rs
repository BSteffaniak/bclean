//! End-to-end scanner tests on temporary fixtures, comparing native and portable backends.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use bclean_scan::{
    ListedEntry, NoPrune, PruneOracle, ScanBackend, ScanError, ScanOptions, ScanOutcome,
    ScanProgress, scan, scan_subtree,
};
use bclean_tree::{EntryKind, NodeFlags, NodeId, NodeInfo, SizeMode, Tree};

/// Builds a fixture and returns its root (inside `tmp`).
fn fixture(tmp: &Path) -> PathBuf {
    let root = tmp.join("root");
    let write = |rel: &str, len: usize| {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, vec![b'x'; len]).unwrap();
    };
    write("a.txt", 10);
    write("empty", 0);
    write("big.bin", 300_000);
    write("src/main.rs", 1234);
    write("src/lib/mod.rs", 4096);
    write("src/lib/deep/x/y/z.txt", 77);
    write("node_modules/pkg/index.js", 5000);
    write("node_modules/pkg/lib/util.js", 6000);
    write("node_modules/other/readme.md", 700);
    write("skip/me/too.txt", 999);
    write("cache.tmp", 50);
    fs::create_dir_all(root.join("emptydir")).unwrap();
    write("links/original", 20_000);
    fs::hard_link(root.join("links/original"), root.join("links/hardlink")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a.txt", root.join("link_to_file")).unwrap();
        std::os::unix::fs::symlink("src", root.join("link_to_dir")).unwrap();
    }
    root
}

fn run(root: &Path, backend: ScanBackend, oracle: &dyn PruneOracle) -> ScanOutcome {
    let options = ScanOptions::new(root).with_backend(backend);
    scan(&options, oracle, &ScanProgress::new()).unwrap()
}

/// Relative path -> node info, for every node.
fn index(tree: &Tree) -> BTreeMap<PathBuf, NodeInfo> {
    tree.descendants(NodeId::ROOT)
        .map(|id| (tree.relative_path(id), tree.info(id)))
        .collect()
}

#[test]
fn native_matches_portable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let native = run(&root, ScanBackend::Native, &NoPrune);
    let portable = run(&root, ScanBackend::Portable, &NoPrune);
    assert_eq!(
        native.backend,
        bclean_scan::resolve_backend(ScanBackend::Native)
    );
    assert_eq!(portable.backend, ScanBackend::Portable);
    assert!(native.issues.is_empty(), "{:?}", native.issues);

    let (n, p) = (index(&native.tree), index(&portable.tree));
    assert_eq!(
        n.keys().collect::<Vec<_>>(),
        p.keys().collect::<Vec<_>>(),
        "same set of paths"
    );
    for (path, ni) in &n {
        let pi = &p[path];
        assert_eq!(ni.kind, pi.kind, "kind of {path:?}");
        assert_eq!(ni.mtime, pi.mtime, "mtime of {path:?}");
        assert_eq!(ni.items, pi.items, "items of {path:?}");
        if ni.kind != EntryKind::Dir {
            assert_eq!(ni.own_apparent, pi.own_apparent, "apparent of {path:?}");
            assert_eq!(ni.own_allocated, pi.own_allocated, "allocated of {path:?}");
        }
        assert_eq!(
            ni.flags.contains(NodeFlags::HARDLINK_DUP),
            pi.flags.contains(NodeFlags::HARDLINK_DUP),
            "hardlink flag of {path:?}"
        );
    }
    // Directory own sizes may legitimately differ between metadata APIs, but file totals and
    // item counts must agree.
    let file_total = |t: &Tree| -> u64 {
        t.descendants(NodeId::ROOT)
            .filter(|&id| t.kind(id) != EntryKind::Dir)
            .map(|id| t.info(id).own_allocated)
            .sum()
    };
    assert_eq!(file_total(&native.tree), file_total(&portable.tree));
    assert_eq!(native.stats.dirs, portable.stats.dirs);
    assert_eq!(native.stats.files, portable.stats.files);
}

#[test]
fn sizes_kinds_and_links() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    for backend in [ScanBackend::Native, ScanBackend::Portable] {
        let out = run(&root, backend, &NoPrune);
        let t = &out.tree;
        assert_eq!(t.root_path(), root.as_path());
        let a = t.find(Path::new("a.txt")).unwrap();
        assert_eq!(t.kind(a), EntryKind::File);
        assert_eq!(t.info(a).own_apparent, 10);
        let big = t.find(Path::new("big.bin")).unwrap();
        assert_eq!(t.info(big).own_apparent, 300_000);
        assert!(t.info(big).own_allocated >= 300_000);
        assert_eq!(
            t.kind(t.find(Path::new("emptydir")).unwrap()),
            EntryKind::Dir
        );
        assert_eq!(t.info(t.find(Path::new("src")).unwrap()).items, 8);

        #[cfg(unix)]
        {
            let l = t.find(Path::new("link_to_dir")).unwrap();
            assert_eq!(t.kind(l), EntryKind::Symlink);
            assert!(!t.has_children(l), "symlinks are not followed");
            let lf = t.find(Path::new("link_to_file")).unwrap();
            assert_eq!(t.kind(lf), EntryKind::Symlink);
        }

        let orig = t.find(Path::new("links/original")).unwrap();
        let hard = t.find(Path::new("links/hardlink")).unwrap();
        if cfg!(windows) && backend == ScanBackend::Native {
            // Link counts are not available from the enumeration on Windows.
            continue;
        }
        let dups: Vec<NodeId> = [orig, hard]
            .into_iter()
            .filter(|&id| t.flags(id).contains(NodeFlags::HARDLINK_DUP))
            .collect();
        assert_eq!(
            dups.len(),
            1,
            "exactly one hard link is a duplicate ({backend:?})"
        );
        assert_eq!(t.info(dups[0]).own_allocated, 0);
        assert_eq!(t.info(dups[0]).own_apparent, 20_000);
        let links = t.find(Path::new("links")).unwrap();
        let info = t.info(links);
        assert!(info.total_allocated - info.own_allocated >= 20_000);
        assert!(info.total_allocated - info.own_allocated < 40_000);
    }
}

#[test]
fn totals_are_consistent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let out = run(&root, ScanBackend::Auto, &NoPrune);
    let t = &out.tree;
    let own_sum: u64 = t
        .descendants(NodeId::ROOT)
        .map(|id| t.info(id).own_allocated)
        .sum();
    assert_eq!(own_sum, t.size(NodeId::ROOT, SizeMode::Allocated));
    assert_eq!(t.info(NodeId::ROOT).items, t.len() as u64);
    assert_eq!(out.stats.bytes, t.size(NodeId::ROOT, SizeMode::Allocated));
    assert_eq!(out.stats.dirs + out.stats.files, t.len() as u64);
}

#[test]
fn oracle_summarizes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let oracle = |_dir: &Path, entries: &[ListedEntry<'_>]| -> Vec<usize> {
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name == "node_modules")
            .map(|(i, _)| i)
            .collect()
    };
    for backend in [ScanBackend::Native, ScanBackend::Portable] {
        let full = run(&root, backend, &NoPrune);
        let pruned = run(&root, backend, &oracle);
        let nm_full = full.tree.find(Path::new("node_modules")).unwrap();
        let nm = pruned.tree.find(Path::new("node_modules")).unwrap();
        let (fi, pi) = (full.tree.info(nm_full), pruned.tree.info(nm));
        assert!(pi.flags.contains(NodeFlags::SUMMARIZED));
        assert!(!pruned.tree.has_children(nm));
        assert_eq!(pi.items, fi.items);
        assert_eq!(pi.own_apparent, fi.total_apparent);
        assert_eq!(pi.own_allocated, fi.total_allocated);
        assert_eq!(pi.max_mtime, fi.max_mtime);
        assert_eq!(
            pruned.tree.info(NodeId::ROOT).items,
            full.tree.info(NodeId::ROOT).items
        );
        assert_eq!(
            pruned.tree.size(NodeId::ROOT, SizeMode::Allocated),
            full.tree.size(NodeId::ROOT, SizeMode::Allocated)
        );
        assert_eq!(pruned.tree.len() + 6, full.tree.len());
    }
}

#[test]
fn excludes_by_name_and_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let options = ScanOptions::new(&root)
        .with_exclude(vec!["*.tmp".to_owned(), format!("{}/skip", root.display())]);
    let out = scan(&options, &NoPrune, &ScanProgress::new()).unwrap();
    let t = &out.tree;
    let tmpf = t.find(Path::new("cache.tmp")).unwrap();
    assert!(t.flags(tmpf).contains(NodeFlags::EXCLUDED));
    assert_eq!(t.info(tmpf).own_apparent, 0);
    let skip = t.find(Path::new("skip")).unwrap();
    assert!(t.flags(skip).contains(NodeFlags::EXCLUDED));
    assert!(!t.has_children(skip));

    let bad = ScanOptions::new(&root).with_exclude(vec!["a[".to_owned()]);
    assert!(matches!(
        scan(&bad, &NoPrune, &ScanProgress::new()),
        Err(ScanError::InvalidExclude { .. })
    ));
}

#[test]
fn subtree_rescan_grafts_back() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let options = ScanOptions::new(&root);
    let full = scan(&options, &NoPrune, &ScanProgress::new()).unwrap();
    let mut tree = full.tree.clone();
    let node = tree.find(Path::new("src")).unwrap();
    let path = tree.path(node);
    let parent = tree.parent(node).unwrap();
    let sub = scan_subtree(&path, &options, &NoPrune, &ScanProgress::new()).unwrap();
    assert_eq!(sub.tree.root_path(), path.as_path());
    assert_eq!(sub.tree.info(NodeId::ROOT).items, 8);
    tree.remove_subtree(node);
    tree.graft(parent, &sub.tree);
    assert_eq!(index(&tree.compact()), index(&full.tree));
}

#[test]
fn file_root_and_missing_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let out = run(&root.join("big.bin"), ScanBackend::Auto, &NoPrune);
    assert_eq!(out.tree.len(), 1);
    assert_eq!(out.tree.kind(NodeId::ROOT), EntryKind::File);
    assert_eq!(out.tree.size(NodeId::ROOT, SizeMode::Apparent), 300_000);

    let missing = ScanOptions::new(root.join("nope"));
    assert!(matches!(
        scan(&missing, &NoPrune, &ScanProgress::new()),
        Err(ScanError::Root { .. })
    ));
}

#[test]
fn cancel_aborts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let progress = ScanProgress::new();
    progress.cancel();
    assert!(matches!(
        scan(&ScanOptions::new(&root), &NoPrune, &progress),
        Err(ScanError::Cancelled)
    ));
    progress.reset();
    assert!(scan(&ScanOptions::new(&root), &NoPrune, &progress).is_ok());
    assert!(progress.snapshot().files > 0);
}

#[test]
fn single_thread_matches_parallel() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let one = scan(
        &ScanOptions::new(&root).with_threads(Some(1)),
        &NoPrune,
        &ScanProgress::new(),
    )
    .unwrap();
    let many = scan(
        &ScanOptions::new(&root).with_threads(Some(8)),
        &NoPrune,
        &ScanProgress::new(),
    )
    .unwrap();
    let strip = |m: BTreeMap<PathBuf, NodeInfo>| -> BTreeMap<PathBuf, (EntryKind, u64, u64)> {
        m.into_iter()
            .map(|(k, v)| (k, (v.kind, v.items, v.total_apparent)))
            .collect()
    };
    assert_eq!(strip(index(&one.tree)), strip(index(&many.tree)));
}

#[cfg(unix)]
#[test]
fn permission_denied_is_flagged_not_fatal() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = fixture(tmp.path());
    let locked = root.join("locked");
    fs::create_dir_all(locked.join("inner")).unwrap();
    fs::write(locked.join("secret"), b"s").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    // Running as root bypasses permission checks; nothing to test then.
    let readable = fs::read_dir(&locked).is_ok();

    for backend in [ScanBackend::Native, ScanBackend::Portable] {
        let out = run(&root, backend, &NoPrune);
        if readable {
            continue;
        }
        let t = &out.tree;
        let id = t.find(Path::new("locked")).unwrap();
        assert!(t.flags(id).contains(NodeFlags::ERROR), "{backend:?}");
        assert!(!t.has_children(id));
        assert!(
            out.issues
                .iter()
                .any(|i| i.path == locked && i.kind == std::io::ErrorKind::PermissionDenied),
            "{:?}",
            out.issues
        );
        assert!(t.find(Path::new("src/main.rs")).is_some());
        assert_eq!(out.stats.errors, 1);
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
}
