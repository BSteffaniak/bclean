//! Headless TUI run against the real local backend (engine in-process).

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use bclean_cli::tui_backend::LocalBackend;
use bclean_config::{Expander, LoadOptions};
use bclean_engine::{Engine, EngineOptions};
use bclean_tui::{App, BackendEvent, EventSink, TuiOptions, render_to_text};

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![1_u8; bytes]).unwrap();
}

#[test]
fn local_backend_renders_tree_and_classifications() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("root");
    let home = base.join("home");
    write(&root.join("app/Cargo.toml"), 10);
    write(&root.join("app/target/debug/bin"), 400_000);
    write(&root.join("notes.txt"), 100);
    std::fs::create_dir_all(&home).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let engine = rt
        .block_on(Engine::open(EngineOptions {
            load: LoadOptions::new(
                root.clone(),
                Expander::new(Some(home.clone()), BTreeMap::new()),
            ),
            data_dir: Some(home.join("data")),
            one_filesystem: true,
        }))
        .unwrap();
    let backend = LocalBackend::new(engine, rt.handle().clone(), root.clone());
    let (sink, mut rx) = EventSink::channel();
    let mut app = App::new(backend, TuiOptions::default(), sink);

    let deadline = Instant::now() + Duration::from_secs(20);
    let (mut tree, mut classes) = (false, false);
    while !(tree && classes) && Instant::now() < deadline {
        match rx.try_recv() {
            Some(event) => {
                tree |= matches!(event, BackendEvent::Tree(_));
                classes |= matches!(&event, BackendEvent::Classifications(c) if !c.is_empty());
                app.apply(event);
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    assert!(tree && classes, "tree={tree} classes={classes}");
    let text = render_to_text(&mut app, 120, 30).join("\n");
    println!("{text}");
    assert!(text.contains("app"), "{text}");
    assert!(text.contains("notes.txt"), "{text}");
}
