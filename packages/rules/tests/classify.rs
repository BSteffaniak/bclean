//! End-to-end classification over real directories and git repositories.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use bclean_config::Expander;
use bclean_model::{Confidence, Quadrant};
use bclean_rules::{Classifier, RuleSet};
use bclean_staleness::StalenessEngine;
use bclean_tree::{EntryKind, EntryMeta, NodeId, Tree};
use filetime::{FileTime, set_file_mtime, set_symlink_file_times};

const DAY: i64 = 86_400;
const NOW: i64 = 1_800_000_000;

fn git(dir: &Path, args: &[&str], date: Option<i64>) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    if let Some(d) = date {
        let d = format!("{d} +0000");
        cmd.env("GIT_AUTHOR_DATE", &d).env("GIT_COMMITTER_DATE", &d);
    }
    let out = cmd.output().expect("git is installed");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(path: &Path, bytes: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// Sets every atime and mtime under `root` (inclusive), including git metadata.
fn age(root: &Path, secs: i64) {
    let t = FileTime::from_unix_time(secs, 0);
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            for e in fs::read_dir(&p).unwrap().flatten() {
                stack.push(e.path());
            }
        }
        set_symlink_file_times(&p, t, t).unwrap();
    }
}

/// Builds a tree from the filesystem (like a scanner without pruning).
fn scan(root: &Path) -> Tree {
    fn meta(p: &Path) -> EntryMeta {
        let m = fs::symlink_metadata(p).unwrap();
        let kind = if m.file_type().is_symlink() {
            EntryKind::Symlink
        } else if m.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        let mtime = FileTime::from_last_modification_time(&m).unix_seconds();
        let size = if m.is_dir() { 0 } else { m.len() };
        EntryMeta::new(kind, size, size, mtime)
    }
    let mut tree = Tree::new(root, meta(root));
    let mut stack = vec![(NodeId::ROOT, root.to_path_buf())];
    while let Some((node, path)) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&path).unwrap().flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for e in entries {
            let p = e.path();
            let m = meta(&p);
            let child = tree.add_child(node, &e.file_name(), m);
            if m.kind == EntryKind::Dir {
                stack.push((child, p));
            }
        }
    }
    tree.recompute_aggregates();
    tree
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

/// Layout:
///   old/        git repo, last commit and all files 400 days old
///     Cargo.toml, src/main.rs, target/debug/{big, node_modules/x}
///     generated/  (gitignored)
///   new/        git repo, committed and edited a day ago
///     Cargo.toml, src/main.rs, target/debug/big
///   loose/target  (no Cargo.toml: must not match)
///   Downloads/  durable root: huge.iso (old, big), small.txt (old, small), fresh.bin (new, big)
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let old_t = NOW - 400 * DAY;

    let old = root.join("old");
    fs::create_dir_all(&old).unwrap();
    git(&old, &["init", "-q", "-b", "main"], None);
    write(&old.join("Cargo.toml"), 10);
    write(&old.join("src/main.rs"), 10);
    fs::write(old.join(".gitignore"), "/target\n/generated/\n").unwrap();
    git(&old, &["add", "."], None);
    git(&old, &["commit", "-q", "-m", "init"], Some(old_t));
    write(&old.join("target/debug/big"), 50_000);
    write(&old.join("target/debug/node_modules/x"), 10);
    write(&old.join("generated/out.bin"), 7_000);
    age(&old, old_t);

    let new = root.join("new");
    fs::create_dir_all(&new).unwrap();
    git(&new, &["init", "-q", "-b", "main"], None);
    write(&new.join("Cargo.toml"), 10);
    write(&new.join("src/main.rs"), 10);
    git(&new, &["add", "."], None);
    git(&new, &["commit", "-q", "-m", "init"], Some(NOW - DAY));
    write(&new.join("target/debug/big"), 50_000);
    age(&new, NOW - DAY);

    write(&root.join("loose/target/x"), 10);
    age(&root.join("loose"), old_t);

    let dl = root.join("Downloads");
    write(&dl.join("huge.iso"), 300_000);
    write(&dl.join("small.txt"), 10);
    age(&dl, old_t);
    write(&dl.join("fresh.bin"), 300_000);
    set_file_mtime(dl.join("fresh.bin"), FileTime::from_unix_time(NOW - DAY, 0)).unwrap();

    Fixture { _dir: dir, root }
}

fn rules(root: &Path, gitignored: bool) -> RuleSet {
    let text = format!(
        r#"
[classify]
durable_roots = ["{dl}"]
durable_min_size = 100000
durable_stale_after = "180d"
gitignored_heuristic = {gitignored}
signals = ["FS_TIMES", "PROJECT_ACTIVITY", "GIT_STATE"]

[rules.cargo_target]
category = "rust"
match = {{ name = ["target"], sibling_any = ["Cargo.toml"] }}
ephemeral = {{ kind = "BUILD_ARTIFACT", regen_cost = "CHEAP" }}
stale = {{ project_idle = "30d" }}
action = {{ kind = "COMMAND", run = "cargo clean", fallback = "DELETE" }}
auto = true

[rules.node_modules]
match = {{ name = ["node_modules"], sibling_any = ["package.json"] }}
ephemeral = {{ kind = "DEPENDENCIES" }}
"#,
        dl = root.join("Downloads").display()
    );
    let config = bclean_config::parse_str(&text).unwrap();
    RuleSet::compile(&config, &Expander::new(None, BTreeMap::new())).unwrap()
}

#[test]
fn classifies_tree_with_staleness_durable_and_scores() {
    let f = fixture();
    let tree = scan(&f.root);
    let set = rules(&f.root, false);
    let engine = StalenessEngine::with_defaults(NOW);
    let results = Classifier::new(&set, &engine).classify_tree(&tree);
    let by_path: BTreeMap<PathBuf, _> = results
        .iter()
        .map(|e| (e.path.strip_prefix(&f.root).unwrap().to_path_buf(), e))
        .collect();

    let c = &by_path[Path::new("old/target")].classification;
    assert_eq!(c.rule_id.as_deref(), Some("cargo_target"));
    assert_eq!(c.quadrant(), Quadrant::EphemeralStale);
    assert!(c.auto);
    assert!(c.size >= 50_000);
    assert!(
        c.reasons
            .iter()
            .any(|r| r.code == "ANCHOR_FOUND" && r.message.contains("Cargo.toml"))
    );
    assert!(
        c.reasons
            .iter()
            .any(|r| r.code == "SIGNAL" && r.message.contains("ProjectActivity"))
    );
    assert!(c.reasons.iter().any(|r| r.code == "STALE"));

    let new_target = &by_path[Path::new("new/target")].classification;
    assert_eq!(new_target.quadrant(), Quadrant::EphemeralFresh);
    assert!(
        c.score > new_target.score,
        "{} vs {}",
        c.score,
        new_target.score
    );

    // Matched dirs are not descended into; unanchored target ignored; heuristic off.
    assert!(
        !by_path
            .keys()
            .any(|p| p.starts_with("old/target") && p != Path::new("old/target")),
        "{:?}",
        by_path.keys().collect::<Vec<_>>()
    );
    assert!(!by_path.contains_key(Path::new("loose/target")));
    assert!(!by_path.contains_key(Path::new("old/generated")));

    // Durable suggestions: only big + stale items under the durable root.
    let huge = &by_path[Path::new("Downloads/huge.iso")].classification;
    assert_eq!(huge.quadrant(), Quadrant::DurableStale);
    assert!(huge.rule_id.is_none());
    assert!(!by_path.contains_key(Path::new("Downloads/small.txt")));
    assert!(!by_path.contains_key(Path::new("Downloads/fresh.bin")));

    // Sorted by score.
    assert!(
        results
            .windows(2)
            .all(|w| w[0].classification.score >= w[1].classification.score)
    );
}

#[test]
fn gitignored_heuristic_is_low_confidence() {
    let f = fixture();
    let tree = scan(&f.root);
    let set = rules(&f.root, true);
    let engine = StalenessEngine::with_defaults(NOW);
    let results = Classifier::new(&set, &engine).classify_tree(&tree);
    let generated = results
        .iter()
        .find(|e| e.path.ends_with("old/generated"))
        .expect("gitignored dir classified");
    assert_eq!(
        generated.classification.rule_id.as_deref(),
        Some("gitignored")
    );
    assert_eq!(generated.classification.confidence, Confidence::Low);
    assert!(!generated.classification.auto);
    // Real rules still win over the heuristic (old/target is gitignored too).
    let target = results
        .iter()
        .find(|e| e.path.ends_with("old/target"))
        .unwrap();
    assert_eq!(
        target.classification.rule_id.as_deref(),
        Some("cargo_target")
    );
}

#[test]
fn subtree_root_can_itself_match() {
    let f = fixture();
    let tree = scan(&f.root.join("old/target"));
    let set = rules(&f.root, false);
    let engine = StalenessEngine::with_defaults(NOW);
    let results = Classifier::new(&set, &engine).classify_tree(&tree);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node, NodeId::ROOT);
    assert_eq!(
        results[0].classification.rule_id.as_deref(),
        Some("cargo_target")
    );
}

#[test]
fn explain_matched_unmatched_and_enclosed() {
    let f = fixture();
    let set = rules(&f.root, false);
    let engine = StalenessEngine::with_defaults(NOW);
    let classifier = Classifier::new(&set, &engine);

    let e = classifier.explain(&f.root.join("old/target")).unwrap();
    assert_eq!(e.classification.rule_id.as_deref(), Some("cargo_target"));
    assert!(e.classification.staleness.stale);
    assert!(e.classification.size >= 50_000);

    let e = classifier.explain(&f.root.join("loose/target")).unwrap();
    assert!(e.classification.rule_id.is_none());
    assert!(
        e.classification
            .reasons
            .iter()
            .any(|r| r.code == "NO_MATCH")
    );
    let miss = e
        .near_misses
        .iter()
        .find(|m| m.rule_id == "cargo_target")
        .unwrap();
    assert!(miss.reason.contains("Cargo.toml"), "{}", miss.reason);

    let e = classifier
        .explain(&f.root.join("old/target/debug"))
        .unwrap();
    assert_eq!(e.enclosing.as_ref().unwrap().rule_id, "cargo_target");
    assert!(
        e.classification
            .reasons
            .iter()
            .any(|r| r.code == "INSIDE_MATCH")
    );

    let e = classifier
        .explain(&f.root.join("Downloads/huge.iso"))
        .unwrap();
    assert!(e.classification.staleness.stale);
    assert!(
        e.classification
            .reasons
            .iter()
            .any(|r| r.code == "DURABLE_ROOT")
    );

    assert!(classifier.explain(&f.root.join("missing")).is_err());
}

#[test]
fn merged_worktree_forces_stale() {
    let f = fixture();
    let repo = f.root.join("new");
    let wt = f.root.join("new-wt-feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
        ],
        None,
    );
    write(&wt.join("lib.rs"), 1);
    git(&wt, &["add", "lib.rs"], None);
    git(&wt, &["commit", "-q", "-m", "f"], Some(NOW - DAY));
    git(&repo, &["merge", "-q", "--ff-only", "feature"], None);
    write(&repo.join("more.rs"), 1);
    git(&repo, &["add", "more.rs"], None);
    git(&repo, &["commit", "-q", "-m", "m"], Some(NOW - DAY));
    write(&wt.join("target/debug/big"), 1_000);
    age(&wt.join("target"), NOW - DAY);

    let set = rules(&f.root, false);
    let engine = StalenessEngine::with_defaults(NOW);
    let e = Classifier::new(&set, &engine)
        .explain(&wt.join("target"))
        .unwrap();
    assert_eq!(e.classification.rule_id.as_deref(), Some("cargo_target"));
    assert!(e.classification.staleness.stale);
    assert!(
        e.classification
            .staleness
            .signals
            .iter()
            .any(|s| s.forces_stale && s.detail.contains("merged"))
    );
}
