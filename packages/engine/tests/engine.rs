//! Engine integration tests: scan + classify, incremental grafting, expansion, policies.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use bclean_config::{Expander, LoadOptions};
use bclean_engine::{Engine, EngineOptions};
use bclean_tree::NodeFlags;

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![1_u8; bytes]).unwrap();
}

async fn setup(config: &str) -> (tempfile::TempDir, PathBuf, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let root = base.join("root");
    write(&root.join("app/Cargo.toml"), 10);
    write(&root.join("app/target/debug/bin"), 40_000);
    write(&root.join("web/package.json"), 2);
    write(&root.join("web/node_modules/a/index.js"), 10_000);
    write(&home.join(".config/bclean/config.toml"), 0);
    std::fs::write(home.join(".config/bclean/config.toml"), config).unwrap();
    let expander = Expander::new(Some(home.clone()), BTreeMap::new());
    let engine = Engine::open(EngineOptions {
        load: LoadOptions::new(root.clone(), expander),
        data_dir: Some(home.join("data")),
        one_filesystem: true,
    })
    .await
    .unwrap();
    (dir, root, engine)
}

#[tokio::test(flavor = "multi_thread")]
async fn scan_classify_and_incremental_updates() {
    let (_dir, root, engine) = setup("").await;
    let report = engine.scan(&root).await.unwrap();
    assert_eq!(report.snapshot.generation, 1);
    assert!(report.snapshot.path.exists());
    let tree = engine.tree(&root).unwrap();
    let nm = tree.find(&root.join("web/node_modules")).unwrap();
    assert!(tree.flags(nm).contains(NodeFlags::SUMMARIZED));
    assert_eq!(engine.classifications_under(&root).len(), 2);

    // A new Rust project appears: rescan its parent and graft.
    write(&root.join("lib/Cargo.toml"), 10);
    write(&root.join("lib/target/x"), 20_000);
    engine.apply_dirty(vec![root.join("lib")]).await.unwrap();
    let classes = engine.classifications_under(&root);
    assert!(
        classes.iter().any(|c| c.path == root.join("lib/target")),
        "{classes:?}"
    );
    assert_eq!(engine.snapshot_ref(&root).unwrap().generation, 2);

    // Growth inside a summarized directory re-summarizes it (still no children).
    let before = engine.tree(&root).unwrap().info(nm).total_allocated;
    write(&root.join("web/node_modules/b/big.js"), 100_000);
    engine
        .apply_dirty(vec![root.join("web/node_modules/b")])
        .await
        .unwrap();
    let tree = engine.tree(&root).unwrap();
    let nm = tree.find(&root.join("web/node_modules")).unwrap();
    assert!(tree.flags(nm).contains(NodeFlags::SUMMARIZED));
    assert!(tree.info(nm).total_allocated > before);
    let class = engine
        .classifications_under(&root.join("web/node_modules"))
        .pop()
        .unwrap();
    assert!(class.classification.size > before);

    // Expansion materializes it.
    let (_, subtree) = engine
        .rescan(&root.join("web/node_modules"), true)
        .await
        .unwrap();
    assert!(subtree.is_some());
    let tree = engine.tree(&root).unwrap();
    assert!(tree.find(&root.join("web/node_modules/b/big.js")).is_some());

    // Deletion is reflected.
    std::fs::remove_dir_all(root.join("lib")).unwrap();
    engine.apply_dirty(vec![root.join("lib")]).await.unwrap();
    assert!(
        engine
            .tree(&root)
            .unwrap()
            .find(&root.join("lib"))
            .is_none()
    );
    assert!(
        engine
            .classifications_under(&root)
            .iter()
            .all(|c| !c.path.starts_with(root.join("lib")))
    );

    // Snapshot round trip in a fresh engine.
    let fresh = engine.clone();
    let meta = fresh.load_snapshot(&root).await.unwrap();
    assert!(meta.is_some());

    // Rollups + history queries work.
    engine.record_rollups().await.unwrap();
    let growth = engine.top_growth(&root, 0, 10).await.unwrap();
    assert!(growth.is_empty(), "only one rollup so far");
}

#[tokio::test(flavor = "multi_thread")]
async fn shadow_policy_fires_plans_and_alerts() {
    let config = r#"
[policies.always]
trigger = { kind = "USED_ABOVE", volume = "*", threshold = "0%" }
mode = "SHADOW"
# The fixture's projects were just created, so their build output is fresh: ask for it.
select = { ephemeral = true, stale = false, auto_only = false, min_size = "1B" }
"#;
    let (_dir, root, engine) = setup(config).await;
    engine.scan(&root).await.unwrap();
    let volumes = engine.sample_volumes().await.unwrap();
    assert!(!volumes.is_empty());
    let outcomes = engine.evaluate_policies(&volumes).await.unwrap();
    assert!(!outcomes.is_empty());
    // One firing per volume; candidates are restricted to the firing's volume.
    let outcome = outcomes
        .iter()
        .find(|o| o.plan.as_ref().is_some_and(|p| !p.is_empty()))
        .unwrap_or_else(|| panic!("no plan with items: {outcomes:#?}"));
    let plan = outcome.plan.as_ref().unwrap();
    assert!(plan.shadow);
    assert!(!plan.items.is_empty());
    assert!(outcome.report.as_ref().unwrap().dry_run);
    assert!(root.join("app/target").exists(), "shadow never acts");
    assert!(!engine.alerts().is_empty());
    let key = engine.alerts()[0].key.clone();
    assert!(engine.ack_alert(&key) > 0);
    assert!(engine.alerts().is_empty());
    assert_eq!(engine.status("t").policies, 1);
}

/// The daemon's CPU regression: a change directly in the root (a dotfile saved in `~`) used to
/// rescan the whole root. It must only re-list the root.
#[tokio::test(flavor = "multi_thread")]
async fn root_level_changes_never_rescan_unchanged_subtrees() {
    let (_dir, root, engine) = setup("").await;
    write(&root.join("old.txt"), 100);
    let report = engine.scan(&root).await.unwrap();
    let snapshot = report.snapshot.path.clone();
    let saved = std::fs::metadata(&snapshot).unwrap().modified().unwrap();
    // Bytes in files (directory entries' own sizes vary by filesystem).
    let file_bytes = |engine: &Engine| {
        let tree = engine.tree(&root).unwrap();
        tree.descendants(bclean_tree::NodeId::ROOT)
            .filter(|id| tree.kind(*id) != bclean_tree::EntryKind::Dir)
            .map(|id| tree.info(id).own_apparent)
            .sum::<u64>()
    };
    let before = file_bytes(&engine);

    // Unreported change deep inside an existing subtree: any subtree rescan would find it.
    write(&root.join("app/src/sentinel.rs"), 7);
    // Reported changes in the root itself.
    write(&root.join("new.txt"), 5_000);
    std::fs::remove_file(root.join("old.txt")).unwrap();
    engine.apply_dirty(vec![root.clone()]).await.unwrap();

    let tree = engine.tree(&root).unwrap();
    assert!(tree.find(&root.join("new.txt")).is_some());
    assert!(tree.find(&root.join("old.txt")).is_none());
    assert!(
        tree.find(&root.join("app/src/sentinel.rs")).is_none(),
        "the unchanged app/ subtree was rescanned"
    );
    assert_eq!(file_bytes(&engine), before + 5_000 - 100);
    assert_eq!(engine.snapshot_ref(&root).unwrap().generation, 2);

    // Incremental updates are saved later, not on every change.
    assert_eq!(
        std::fs::metadata(&snapshot).unwrap().modified().unwrap(),
        saved
    );
    engine
        .persist_unsaved(std::time::Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(
        std::fs::metadata(&snapshot).unwrap().modified().unwrap(),
        saved
    );
    engine.persist_all().await.unwrap();
    assert_ne!(
        std::fs::metadata(&snapshot).unwrap().modified().unwrap(),
        saved
    );

    // The deep change is picked up once its own directory is reported.
    engine
        .apply_dirty(vec![root.join("app/src")])
        .await
        .unwrap();
    let tree = engine.tree(&root).unwrap();
    assert!(tree.find(&root.join("app/src/sentinel.rs")).is_some());

    // A report with no actual change does not bump the generation.
    engine
        .apply_dirty(vec![root.join("app/src")])
        .await
        .unwrap();
    assert_eq!(engine.snapshot_ref(&root).unwrap().generation, 3);

    // Status counts the one full scan and every incremental refresh.
    let status = &engine.root_statuses()[0];
    assert_eq!((status.full_scans, status.refreshes), (1, 3));
}

/// Build output of a project still being worked on is never picked automatically: by default
/// only once the project has been idle for the rule's `stale.project_idle`.
#[tokio::test(flavor = "multi_thread")]
async fn automatic_cleaning_skips_build_output_of_active_projects() {
    let (_dir, root, engine) = setup("").await;
    engine.scan(&root).await.unwrap();
    let target = root.join("app/target");
    let class = engine
        .classifications_under(&root)
        .into_iter()
        .find(|c| c.path == target)
        .expect("app/target is classified")
        .classification;
    assert!(class.staleness.project_scoped && !class.staleness.stale);
    assert!(
        engine.plan_auto().items.iter().all(|i| i.path != target),
        "fresh build output is not planned automatically"
    );
    let summary = engine
        .dashboard(std::slice::from_ref(&root), false)
        .await
        .unwrap();
    assert!(summary.reclaimable.active >= class.size);
    assert!(!summary.reclaimable.safe_paths.contains(&target));
}

/// Losing change history for a whole root over and over (dropped events under heavy load)
/// must not turn into back-to-back full scans.
#[tokio::test(flavor = "multi_thread")]
async fn lost_history_rescans_are_rate_limited() {
    let (_dir, root, engine) = setup("[monitor]\nrescan_budget_percent = 1\n").await;
    engine.scan(&root).await.unwrap();
    let lost = || bclean_watch::ChangeBatch {
        dirty: vec![root.clone()],
        must_full_rescan: true,
        cursor: None,
    };
    let full_scans = || engine.root_statuses()[0].full_scans;

    engine.apply_batch(lost()).await.unwrap();
    assert_eq!(full_scans(), 2, "the first loss is rescanned right away");
    for _ in 0..5 {
        engine.apply_batch(lost()).await.unwrap();
        engine.refresh_deferred().await.unwrap();
    }
    assert_eq!(full_scans(), 2, "later losses wait for the rescan budget");

    // The queued losses run once, together, when the budget allows.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while full_scans() == 2 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        engine.refresh_deferred().await.unwrap();
    }
    assert_eq!(full_scans(), 3);

    // Explicit rescans are never limited.
    engine.rescan(&root, false).await.unwrap();
    assert_eq!(full_scans(), 4);
}

/// A loss whose rescan waits for the rescan budget must survive a restart: until it is
/// reconciled, snapshots keep the cursor from before the loss, so it is replayed.
#[tokio::test(flavor = "multi_thread")]
async fn snapshots_keep_the_cursor_from_before_unreconciled_losses() {
    let (dir, root, engine) = setup("[monitor]\nrescan_budget_percent = 1\n").await;
    engine.scan(&root).await.unwrap();
    let cursor = |event_id| bclean_watch::WatchCursor::FsEvents {
        event_id,
        volume_uuids: BTreeMap::new(),
    };
    let lost = |event_id| bclean_watch::ChangeBatch {
        dirty: vec![root.clone()],
        must_full_rescan: true,
        cursor: Some(cursor(event_id)),
    };
    // Read through a second engine: loading replaces the first one's in-memory state.
    let saved = || async {
        let home = std::fs::canonicalize(dir.path()).unwrap().join("home");
        let reader = Engine::open(EngineOptions {
            load: LoadOptions::new(
                root.clone(),
                Expander::new(Some(home.clone()), BTreeMap::new()),
            ),
            data_dir: Some(home.join("data")),
            one_filesystem: true,
        })
        .await
        .unwrap();
        let meta = reader.load_snapshot(&root).await.unwrap().unwrap();
        bclean_watch::WatchCursor::from_bytes(&meta.cursor.unwrap()).unwrap()
    };

    // Rescanned at once: the snapshot moves on to the loss's cursor.
    engine.apply_batch(lost(1)).await.unwrap();
    assert_eq!(saved().await, cursor(1));

    // Deferred by the budget: later saves keep the cursor from before the loss.
    engine.apply_batch(lost(2)).await.unwrap();
    write(&root.join("app/new.rs"), 10);
    engine
        .apply_batch(bclean_watch::ChangeBatch {
            dirty: vec![root.join("app")],
            must_full_rescan: false,
            cursor: Some(cursor(3)),
        })
        .await
        .unwrap();
    engine.persist_all().await.unwrap();
    assert_eq!(saved().await, cursor(1));

    // Once the deferred rescan ran, the current cursor is saved again.
    let full_scans = || engine.root_statuses()[0].full_scans;
    let before = full_scans();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while full_scans() == before && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        engine.refresh_deferred().await.unwrap();
    }
    assert_eq!(saved().await, cursor(3));
}

/// A root nothing watches (one opened ad hoc, like `bclean /`) is shown from its snapshot at
/// once, then rescanned: the snapshot may be days old.
#[tokio::test(flavor = "multi_thread")]
async fn unwatched_roots_opened_from_snapshots_are_rescanned() {
    let (dir, root, engine) = setup("").await;
    engine.scan(&root).await.unwrap();
    std::fs::remove_dir_all(root.join("web")).unwrap();

    let home = std::fs::canonicalize(dir.path()).unwrap().join("home");
    let reopened = Engine::open(EngineOptions {
        load: LoadOptions::new(
            root.clone(),
            Expander::new(Some(home.clone()), BTreeMap::new()),
        ),
        data_dir: Some(home.join("data")),
        one_filesystem: true,
    })
    .await
    .unwrap();
    reopened.ensure_scanned(&root, false).await.unwrap();
    // The rescan races any check of the snapshot itself, so only its outcome is asserted.
    let has_web = || {
        reopened
            .tree(&root)
            .unwrap()
            .find(&root.join("web"))
            .is_some()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while has_web() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!has_web(), "the stale snapshot is replaced by a rescan");
}

/// Expanding a directory indexed by nested roots (`/` and `~`) must reach the outer root's
/// tree too: a client browsing `/` waits for that tree to change.
#[tokio::test(flavor = "multi_thread")]
async fn expand_updates_every_root_containing_the_path() {
    let (_dir, root, engine) = setup("").await;
    let inner = root.join("web");
    engine.scan(&root).await.unwrap();
    engine.scan(&inner).await.unwrap();
    let nm = inner.join("node_modules");
    let expanded = |r: &Path| {
        let tree = engine.tree(r).unwrap();
        let id = tree.find(&nm).unwrap();
        tree.has_children(id)
    };
    assert!(!expanded(&root) && !expanded(&inner));

    let mut events = engine.subscribe();
    let (snapshot, subtree) = engine.rescan(&nm, true).await.unwrap();
    assert_eq!(
        snapshot.root, inner,
        "the deepest root's result is returned"
    );
    assert!(subtree.is_some());
    assert!(expanded(&inner));
    // The outer root catches up in the background and announces it.
    let mut updated = Vec::new();
    while !updated.contains(&root) {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .expect("outer root never updated")
            .unwrap();
        if let bclean_protocol::Event::TreeUpdated(s) = event {
            updated.push(s.root);
        }
    }
    assert!(updated.contains(&inner), "{updated:?}");
    assert!(expanded(&root), "the outer root was not updated");
}

/// The dashboard summarizes reclaimable data by kind, category, and project, counting nested
/// classifications once.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_summarizes_reclaimable_data() {
    let (_dir, root, engine) = setup("").await;
    engine.scan(&root).await.unwrap();
    let summary = engine
        .dashboard(std::slice::from_ref(&root), false)
        .await
        .unwrap();

    assert_eq!(summary.roots.len(), 1);
    assert_eq!(summary.roots[0].path, root);
    let reclaim = &summary.reclaimable;
    assert_eq!(reclaim.items, 2, "{summary:#?}");
    assert!(reclaim.total >= 50_000, "{reclaim:?}");
    assert!(reclaim.safe <= reclaim.total);
    assert_eq!(
        reclaim.ephemeral_stale + reclaim.ephemeral_fresh + reclaim.durable_stale,
        reclaim.total
    );

    let kinds: Vec<_> = summary.by_kind.iter().map(|b| b.kind).collect();
    assert!(
        kinds.contains(&Some(bclean_model::EphemeralKind::BuildArtifact)),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&Some(bclean_model::EphemeralKind::Dependencies)),
        "{kinds:?}"
    );
    assert_eq!(
        summary.by_kind.iter().map(|b| b.bytes).sum::<u64>(),
        reclaim.total
    );
    assert_eq!(
        summary.by_category.iter().map(|b| b.bytes).sum::<u64>(),
        reclaim.total
    );
    assert_eq!(summary.top_items.len(), 2);
    assert!(summary.top_items[0].score >= summary.top_items[1].score);

    // Each item is attributed to the directory holding its project marker.
    let projects: Vec<_> = summary.projects.iter().map(|p| p.path.clone()).collect();
    assert!(projects.contains(&root.join("app")), "{projects:?}");
    assert!(projects.contains(&root.join("web")), "{projects:?}");
    let app = summary
        .projects
        .iter()
        .find(|p| p.path == root.join("app"))
        .unwrap();
    assert_eq!(app.entries, ["target"]);
}

/// bclean's own snapshot and database writes must never feed back into the index.
#[tokio::test(flavor = "multi_thread")]
async fn own_data_dir_churn_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let home = root.join("home");
    write(&root.join("project/file"), 10);
    let expander = Expander::new(Some(home.clone()), BTreeMap::new());
    let engine = Engine::open(EngineOptions {
        load: LoadOptions::new(root.clone(), expander),
        data_dir: Some(root.join("data")),
        one_filesystem: true,
    })
    .await
    .unwrap();
    engine.scan(&root).await.unwrap();
    let generation = engine.snapshot_ref(&root).unwrap().generation;
    write(&root.join("data/extra.bin"), 1_000);
    engine
        .apply_dirty(vec![root.join("data"), root.join("data/snapshots")])
        .await
        .unwrap();
    assert_eq!(engine.snapshot_ref(&root).unwrap().generation, generation);
}

/// Changes inside a summarized directory (a build writing to `target/`) re-measure it at most
/// once per `monitor.summary_refresh_interval`.
#[tokio::test(flavor = "multi_thread")]
async fn summarized_directories_are_remeasured_at_most_once_per_interval() {
    let (_dir, root, engine) = setup("[monitor]\nsummary_refresh_interval = \"1h\"\n").await;
    engine.scan(&root).await.unwrap();
    let nm_size = |engine: &Engine| {
        let tree = engine.tree(&root).unwrap();
        let nm = tree.find(&root.join("web/node_modules")).unwrap();
        tree.info(nm).total_allocated
    };
    let initial = nm_size(&engine);

    write(&root.join("web/node_modules/b/one.js"), 100_000);
    engine
        .apply_dirty(vec![root.join("web/node_modules/b")])
        .await
        .unwrap();
    let first = nm_size(&engine);
    assert!(first > initial);

    // Within the interval: postponed, even through the maintenance hook.
    write(&root.join("web/node_modules/b/two.js"), 100_000);
    engine
        .apply_dirty(vec![root.join("web/node_modules/b")])
        .await
        .unwrap();
    engine.refresh_deferred().await.unwrap();
    assert_eq!(nm_size(&engine), first);

    // An explicit rescan is never postponed.
    engine
        .rescan(&root.join("web/node_modules"), false)
        .await
        .unwrap();
    assert!(nm_size(&engine) > first);
}
