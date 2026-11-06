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
select = { ephemeral = true, auto_only = false, min_size = "1B" }
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
