//! End-to-end: an in-process daemon (real engine) on a temp socket, driven through the client.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use bclean_actions::{ExecutionReport, ItemOutcome, JournalEntry, Plan};
use bclean_client::{Client, ClientOptions};
use bclean_config::{Expander, LoadOptions};
use bclean_engine::{Engine, EngineOptions};
use bclean_protocol::{Endpoint, Event, Request, Response, Topic};

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
    moved: PathBuf,
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![7_u8; bytes]).unwrap();
}

fn set_old(path: &Path) {
    let old =
        filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(400 * 86_400));
    for entry in walk(path) {
        filetime::set_symlink_file_times(&entry, old, old).unwrap();
    }
}

fn walk(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    if path.is_dir() {
        for e in std::fs::read_dir(path).unwrap() {
            out.extend(walk(&e.unwrap().path()));
        }
    }
    out
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let home = base.join("home");
    let root = base.join("projects");
    let moved = base.join("moved");
    write(&root.join("app/Cargo.toml"), 20);
    write(&root.join("app/src/main.rs"), 20);
    write(&root.join("app/target/debug/app"), 200_000);
    write(&root.join("web/package.json"), 2);
    write(&root.join("web/node_modules/left-pad/index.js"), 50_000);
    write(&root.join("old/Cargo.toml"), 20);
    write(&root.join("old/target/release/big"), 300_000);
    write(&root.join("tool/junkcache/blob"), 10_000);
    set_old(&root.join("old"));
    let config = format!(
        r#"
[monitor]
index_roots = []
volumes = ["{home}"]

[rules.junk_cache]
match = {{ name = ["junkcache"] }}
ephemeral = {{ kind = "TEMP", regen_cost = "FREE" }}
action = {{ kind = "MOVE", dest = "{moved}" }}
"#,
        home = home.display(),
        moved = moved.display()
    );
    write(&home.join(".config/bclean/config.toml"), 0);
    std::fs::write(home.join(".config/bclean/config.toml"), config).unwrap();
    Fixture {
        _dir: dir,
        home,
        root,
        moved,
    }
}

async fn engine(fx: &Fixture) -> Engine {
    let mut env = BTreeMap::new();
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        fx.home.join(".config").display().to_string(),
    );
    let expander = Expander::new(Some(fx.home.clone()), env);
    Engine::open(EngineOptions {
        load: LoadOptions::new(fx.root.clone(), expander),
        data_dir: Some(fx.home.join("data")),
        one_filesystem: true,
    })
    .await
    .unwrap()
}

fn endpoint(tag: &str) -> Endpoint {
    if cfg!(windows) {
        Endpoint::Namespaced(format!("bclean-e2e-{tag}-{}", std::process::id()))
    } else {
        Endpoint::Path(PathBuf::from(format!(
            "/tmp/bclean-e2e-{tag}-{}/d.sock",
            std::process::id()
        )))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_flows_through_the_client() {
    let fx = fixture();
    let engine = engine(&fx).await;
    let endpoint = endpoint("flows");
    let serving = tokio::spawn({
        let engine = engine.clone();
        let endpoint = endpoint.clone();
        async move { bclean_daemon::run_daemon(engine, endpoint, "test").await }
    });
    let mut options = ClientOptions::new(endpoint.clone());
    options.autostart = false;
    options.client_version = "test".to_owned();
    let mut client = loop {
        if let Ok(c) = Client::connect(&options).await {
            break c;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut events = Client::connect(&options).await.unwrap();
    events
        .request(Request::Subscribe {
            topics: vec![Topic::Scans, Topic::Actions],
        })
        .await
        .unwrap();

    // Status.
    let Response::Status(status) = client.request(Request::Status).await.unwrap() else {
        panic!("status");
    };
    assert_eq!(status.version, "test");

    // Scan (wait) -> snapshot on disk, loaded via bclean_index.
    let Response::Snapshot(snapshot) = client
        .request(Request::Scan {
            root: fx.root.clone(),
            force: true,
            wait: true,
        })
        .await
        .unwrap()
    else {
        panic!("snapshot");
    };
    assert_eq!(snapshot.root, fx.root);
    let tree = bclean_index::snapshot::read(&snapshot.path, Some(&fx.root))
        .unwrap()
        .tree;
    assert!(tree.find(&fx.root.join("app/target")).is_some());
    let updated = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::TreeUpdated(s)) = events.next_event().await.unwrap() {
                break s;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(updated.root, fx.root);

    // Classifications.
    let Response::Classifications { items } = client
        .request(Request::Classifications {
            root: fx.root.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("classifications");
    };
    let rule = |p: &str| {
        items
            .iter()
            .find(|i| i.path == fx.root.join(p))
            .and_then(|i| i.classification.rule_id.clone())
    };
    assert_eq!(rule("app/target").as_deref(), Some("cargo_target"));
    assert_eq!(rule("web/node_modules").as_deref(), Some("node_modules"));
    assert_eq!(rule("tool/junkcache").as_deref(), Some("junk_cache"));
    let old = items
        .iter()
        .find(|i| i.path == fx.root.join("old/target"))
        .unwrap();
    assert!(
        old.classification.staleness.stale,
        "old project target is stale"
    );

    // Explain.
    let Response::Explain(explained) = client
        .request(Request::Explain {
            path: fx.root.join("old/target"),
        })
        .await
        .unwrap()
    else {
        panic!("explain");
    };
    assert_eq!(
        explained.classification.rule_id.as_deref(),
        Some("cargo_target")
    );

    // Plan + dry run + execute a MOVE.
    let junk = fx.root.join("tool/junkcache");
    let Response::Plan { plan } = client
        .request(Request::Plan {
            paths: vec![junk.clone()],
        })
        .await
        .unwrap()
    else {
        panic!("plan");
    };
    let parsed: Plan = serde_json::from_value(plan.clone()).unwrap();
    assert_eq!(parsed.items.len(), 1);
    let Response::Actions { value } = client
        .request(Request::Execute {
            plan: plan.clone(),
            dry_run: true,
        })
        .await
        .unwrap()
    else {
        panic!("dry run");
    };
    let report: ExecutionReport = serde_json::from_value(value).unwrap();
    assert!(report.dry_run);
    assert!(junk.exists());
    let Response::Actions { value } = client
        .request(Request::Execute {
            plan,
            dry_run: false,
        })
        .await
        .unwrap()
    else {
        panic!("execute");
    };
    let report: ExecutionReport = serde_json::from_value(value).unwrap();
    assert_eq!(report.results[0].outcome, ItemOutcome::Done, "{report:?}");
    assert!(!junk.exists());
    assert!(fx.moved.join("junkcache").exists());
    // The tree was refreshed around the moved item.
    let Response::Classifications { items } = client
        .request(Request::Classifications {
            root: fx.root.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("classifications");
    };
    assert!(items.iter().all(|i| i.path != junk));

    // Journal + undo.
    let Response::Actions { value } = client
        .request(Request::Journal { limit: 10 })
        .await
        .unwrap()
    else {
        panic!("journal");
    };
    let entries: Vec<JournalEntry> = serde_json::from_value(value).unwrap();
    let entry = bclean_engine::latest_undoable(&entries).expect("undoable entry");
    assert_eq!(entry.path, junk);
    client
        .request(Request::Undo {
            id: entry.id.clone(),
        })
        .await
        .unwrap();
    assert!(junk.join("blob").exists());
    assert!(
        client
            .request(Request::Undo {
                id: entry.id.clone(),
            })
            .await
            .is_err(),
        "double undo is refused"
    );

    // Volumes, alerts, rescan with expansion.
    assert!(matches!(
        client.request(Request::Volumes).await.unwrap(),
        Response::Volumes { .. }
    ));
    assert!(matches!(
        client.request(Request::Alerts).await.unwrap(),
        Response::Alerts { .. }
    ));
    let Response::Expanded { subtree } = client
        .request(Request::Rescan {
            path: fx.root.join("web/node_modules"),
            expand: true,
        })
        .await
        .unwrap()
    else {
        panic!("rescan");
    };
    // The expanded directory comes alone in its own file.
    assert_eq!(subtree.root, fx.root.join("web/node_modules"));
    let alone = bclean_index::snapshot::read(&subtree.path, None)
        .unwrap()
        .tree;
    assert!(
        alone
            .find(&fx.root.join("web/node_modules/left-pad"))
            .is_some()
    );
    assert!(alone.len() < 10, "only the subtree: {} nodes", alone.len());
    // The root's snapshot has it too.
    let Response::Snapshot(after) = client
        .request(Request::Snapshot {
            root: fx.root.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("snapshot");
    };
    let tree = bclean_index::snapshot::read(&after.path, Some(&fx.root))
        .unwrap()
        .tree;
    assert!(
        tree.find(&fx.root.join("web/node_modules/left-pad"))
            .is_some(),
        "expanded"
    );
    assert!(after.generation > snapshot.generation);

    // Dashboard summary over the scanned root.
    let Response::Dashboard(summary) = client
        .request(Request::Dashboard {
            roots: vec![fx.root.clone()],
            one_filesystem: false,
            safe_after_secs: None,
        })
        .await
        .unwrap()
    else {
        panic!("dashboard");
    };
    assert!(
        summary.roots.iter().any(|r| r.path == fx.root),
        "{summary:?}"
    );
    assert!(summary.reclaimable.items >= 1, "{summary:?}");
    assert!(summary.reclaimable.total > 0);

    client.request(Request::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
