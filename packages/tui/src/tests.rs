//! Headless tests of the app state machine and rendering.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bclean_model::{ActionSpec, AlertEvent, Severity, VolumeStats};
use bclean_tree::{EntryKind, EntryMeta, NodeFlags, NodeId, Tree};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};

use crate::listing::tests::{class, file, sample_tree};
use crate::{
    ActionOverride, App, BackendEvent, EventReceiver, EventSink, ExecutionEvent, Handled,
    InMemoryBackend, Lens, Modal, ScanProgress, TuiOptions, View, render_to_text,
};

type TestApp = App<InMemoryBackend>;

fn setup(backend: InMemoryBackend) -> (TestApp, EventReceiver) {
    let (sink, rx) = EventSink::channel();
    let mut app = App::new(backend, TuiOptions::default(), sink);
    app.viewport_rows = 10;
    let mut rx = rx;
    pump(&mut app, &mut rx);
    (app, rx)
}

fn pump(app: &mut TestApp, rx: &mut EventReceiver) {
    while let Some(event) = rx.try_recv() {
        app.apply(event);
    }
}

fn key(code: KeyCode) -> KeyStroke {
    KeyStroke::simple(code)
}

fn ch(c: char) -> KeyStroke {
    if c.is_ascii_uppercase() {
        KeyStroke {
            key: KeyCode::Char(c.to_ascii_lowercase()),
            modifiers: Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        }
    } else {
        key(KeyCode::Char(c))
    }
}

fn press(app: &mut TestApp, rx: &mut EventReceiver, strokes: &[KeyStroke]) {
    for stroke in strokes {
        app.handle_key(*stroke);
        pump(app, rx);
    }
}

fn selected_name(app: &mut TestApp) -> String {
    let id = app.selected().expect("selection");
    app.tree().unwrap().name(id).to_string_lossy().into_owned()
}

fn backend() -> InMemoryBackend {
    InMemoryBackend::new(sample_tree()).with_classifications([
        (PathBuf::from("/r/c/target"), class(true, true, 9.0, 500)),
        (PathBuf::from("/r/b"), class(false, true, 3.0, 300)),
    ])
}

#[test]
fn navigates_in_and_out() {
    let (mut app, mut rx) = setup(backend());
    assert_eq!(selected_name(&mut app), "c");
    press(&mut app, &mut rx, &[key(KeyCode::Down), key(KeyCode::Down)]);
    assert_eq!(selected_name(&mut app), "a");
    press(&mut app, &mut rx, &[key(KeyCode::Enter)]);
    assert_eq!(app.tree().unwrap().path(app.cwd()), Path::new("/r/a"));
    assert_eq!(selected_name(&mut app), "x");
    press(&mut app, &mut rx, &[ch('G')]);
    assert_eq!(selected_name(&mut app), "y");
    press(&mut app, &mut rx, &[key(KeyCode::Left)]);
    assert_eq!(app.cwd(), NodeId::ROOT);
    assert_eq!(
        selected_name(&mut app),
        "a",
        "returns to the directory we left"
    );
    press(&mut app, &mut rx, &[ch('g')]);
    assert_eq!(selected_name(&mut app), "c");
    // Can't go above the root; files don't open.
    press(
        &mut app,
        &mut rx,
        &[key(KeyCode::Backspace), ch('j'), key(KeyCode::Enter)],
    );
    assert_eq!(app.cwd(), NodeId::ROOT);
}

#[test]
fn sort_keys_keep_selection() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[key(KeyCode::Down)]); // b
    press(&mut app, &mut rx, &[ch('n')]);
    assert_eq!(selected_name(&mut app), "b");
    assert_eq!(app.rows.selected, 2, ".hidden, a, b");
    press(&mut app, &mut rx, &[ch('n')]);
    assert!(app.sort.reverse);
    press(&mut app, &mut rx, &[ch('.')]);
    assert!(!app.show_hidden);
}

#[test]
fn filter_typing() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[ch('/'), ch('a')]);
    assert!(app.filter_editing);
    assert_eq!(app.listing().len(), 1);
    press(&mut app, &mut rx, &[key(KeyCode::Enter)]);
    assert!(!app.filter_editing);
    assert_eq!(app.filter, "a");
    press(&mut app, &mut rx, &[key(KeyCode::Escape)]);
    assert_eq!(app.filter, "");
    assert_eq!(app.listing().len(), 4);
}

#[test]
fn lenses_flatten_and_jump() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[ch('4')]);
    assert_eq!(app.lens(), Lens::Reclaimable);
    assert_eq!(selected_name(&mut app), "target", "sorted by score");
    press(&mut app, &mut rx, &[key(KeyCode::Tab)]);
    assert_eq!(app.lens(), Lens::All, "tab skips the unavailable lens");
    press(&mut app, &mut rx, &[ch('2')]);
    assert_eq!(app.listing().len(), 1);
    press(&mut app, &mut rx, &[key(KeyCode::Enter)]);
    assert_eq!(app.lens(), Lens::All);
    assert_eq!(app.tree().unwrap().path(app.cwd()), Path::new("/r/c"));
    assert_eq!(selected_name(&mut app), "target");
    press(&mut app, &mut rx, &[ch('5')]);
    assert_eq!(app.lens(), Lens::All, "grew-since is a hook only");
    assert!(app.message.is_some());
}

#[test]
fn mark_plan_execute_report() {
    let (mut app, mut rx) = setup(backend());
    // Mark c and b (space moves down).
    press(
        &mut app,
        &mut rx,
        &[key(KeyCode::Space), key(KeyCode::Space)],
    );
    assert_eq!(app.marked().len(), 2);
    assert_eq!(app.marked_bytes(), 800);
    press(&mut app, &mut rx, &[ch('p')]);
    let Some(Modal::PlanPreview { plan, action, .. }) = app.modal().cloned() else {
        panic!("expected plan preview, got {:?}", app.modal());
    };
    assert_eq!(action, ActionOverride::RuleDefault);
    assert_eq!(plan.items.len(), 2);
    assert_eq!(plan.estimated_bytes(), 800);

    // Cycle the action override: rebuilds the plan.
    press(&mut app, &mut rx, &[ch('a')]);
    let Some(Modal::PlanPreview { action, .. }) = app.modal().cloned() else {
        panic!("expected plan preview");
    };
    assert_eq!(action, ActionOverride::Force(ActionSpec::Trash));

    press(&mut app, &mut rx, &[ch('y')]);
    let Some(Modal::Execution(exec)) = app.modal().cloned() else {
        panic!("expected execution");
    };
    let report = exec.report.expect("report");
    assert_eq!(report.done, 2);
    assert_eq!(report.estimated_bytes, 800);
    assert_eq!(report.measured_freed_bytes, Some(800));
    // Items were removed locally and unmarked.
    let tree = app.tree().unwrap();
    assert!(tree.find(Path::new("/r/b")).is_none());
    assert!(tree.find(Path::new("/r/c")).is_none());
    assert!(app.marked().is_empty());
    press(&mut app, &mut rx, &[key(KeyCode::Enter)]);
    assert!(app.modal().is_none());
    assert_eq!(app.listing().len(), 2);
}

#[test]
fn execution_shows_progress_inside_a_large_item() {
    let backend = backend().with_item_delay(std::time::Duration::from_secs(30));
    let (mut app, mut rx) = setup(backend);
    // Mark c (500 B) and clean it.
    press(&mut app, &mut rx, &[key(KeyCode::Space), ch('p'), ch('y')]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !matches!(app.modal(), Some(Modal::Execution(exec)) if exec.current == Some(0)) {
        assert!(std::time::Instant::now() < deadline, "item never started");
        std::thread::sleep(std::time::Duration::from_millis(5));
        pump(&mut app, &mut rx);
    }
    app.apply(BackendEvent::Execution(ExecutionEvent::ItemProgress {
        index: 0,
        removed: 2,
        entries: Some(4),
    }));
    let Some(Modal::Execution(exec)) = app.modal().cloned() else {
        panic!("expected execution");
    };
    assert_eq!(exec.done_estimated_bytes(), 250);
    let text = render_to_text(&mut app, 120, 30).join("\n");
    assert!(text.contains("250 B / 500 B · 0/1 items"), "{text}");
    assert!(text.contains("2 of 4 entries"), "{text}");
}

#[test]
fn dry_run_changes_nothing() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[ch('p'), ch('t')]);
    let Some(Modal::Execution(exec)) = app.modal().cloned() else {
        panic!("expected execution");
    };
    assert!(exec.dry_run);
    assert_eq!(exec.report.unwrap().measured_freed_bytes, None);
    assert!(app.tree().unwrap().find(Path::new("/r/c")).is_some());
}

#[test]
fn delete_goes_through_plan_and_cancel_works() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[ch('d')]);
    let Some(Modal::PlanPreview { plan, action, .. }) = app.modal().cloned() else {
        panic!("expected plan preview");
    };
    assert_eq!(action, ActionOverride::Force(ActionSpec::Delete));
    assert_eq!(plan.items[0].path, PathBuf::from("/r/c"));
    assert_eq!(plan.items[0].action, ActionSpec::Delete);
    press(&mut app, &mut rx, &[key(KeyCode::Escape)]);
    assert!(app.modal().is_none());
    assert!(app.tree().unwrap().find(Path::new("/r/c")).is_some());
}

#[test]
fn stale_plan_answers_are_ignored() {
    let (sink, mut rx) = EventSink::channel();
    // A backend that never answers by itself: drop its events.
    let mut app = App::new(
        InMemoryBackend::new(sample_tree()),
        TuiOptions::default(),
        sink,
    );
    pump(&mut app, &mut rx);
    app.handle_key(ch('p'));
    let mut events = Vec::new();
    while let Some(e) = rx.try_recv() {
        events.push(e);
    }
    let Some(BackendEvent::Plan(mut plan)) = events.pop() else {
        panic!("expected a plan event");
    };
    plan.request_id += 100;
    app.apply(BackendEvent::Plan(plan));
    assert!(matches!(app.modal(), Some(Modal::PlanPending { .. })));
    assert_eq!(app.handle_key(key(KeyCode::Escape)), Handled::Redraw);
    assert!(app.modal().is_none());
}

#[test]
fn expands_summarized_directories() {
    let mut tree = sample_tree();
    let mut meta =
        EntryMeta::new(EntryKind::Dir, 1_000, 1_000, 0).with_flags(NodeFlags::SUMMARIZED);
    meta.items = 3;
    tree.add_child(NodeId::ROOT, OsStr::new("node_modules"), meta);
    let mut sub = Tree::new("/r/node_modules", EntryMeta::dir());
    sub.add_child_raw(NodeId::ROOT, OsStr::new("left-pad"), file(600, 1));
    sub.add_child_raw(NodeId::ROOT, OsStr::new("lodash"), file(400, 1));
    sub.recompute_aggregates();
    let backend = InMemoryBackend::new(tree).with_subtree(PathBuf::from("/r/node_modules"), sub);
    let (mut app, mut rx) = setup(backend);
    assert_eq!(selected_name(&mut app), "node_modules");
    press(&mut app, &mut rx, &[key(KeyCode::Enter)]);
    assert!(
        app.backend()
            .log
            .iter()
            .any(|l| l == "expand /r/node_modules")
    );
    let tree = app.tree().unwrap();
    assert_eq!(tree.path(app.cwd()), Path::new("/r/node_modules"));
    assert_eq!(app.listing().len(), 2);
    assert_eq!(selected_name(&mut app), "left-pad");
    assert_eq!(
        app.tree()
            .unwrap()
            .size(NodeId::ROOT, bclean_tree::SizeMode::Allocated),
        1_960
    );
}

/// The daemon backend reports a finished expansion only by publishing the updated tree.
#[test]
fn expansion_completes_from_a_tree_update() {
    let summarized = || {
        let mut meta =
            EntryMeta::new(EntryKind::Dir, 1_000, 1_000, 0).with_flags(NodeFlags::SUMMARIZED);
        meta.items = 3;
        meta
    };
    let mut tree = sample_tree();
    tree.add_child(NodeId::ROOT, OsStr::new("node_modules"), summarized());
    let (mut app, mut rx) = setup(InMemoryBackend::new(tree));
    assert_eq!(selected_name(&mut app), "node_modules");
    app.handle_key(key(KeyCode::Enter));
    // Drop the in-memory backend's reply (an error: it has no subtree for this path).
    while rx.try_recv().is_some() {}
    let text = render_to_text(&mut app, 120, 14).join("\n");
    assert!(text.contains("expanding"), "{text}");

    // An unrelated update leaves the expansion pending.
    let mut unrelated = sample_tree();
    unrelated.add_child(NodeId::ROOT, OsStr::new("node_modules"), summarized());
    app.apply(BackendEvent::Tree(Arc::new(unrelated)));
    assert_eq!(app.tree().unwrap().path(app.cwd()), Path::new("/r"));

    // The daemon publishes the root's tree with node_modules materialized.
    let mut expanded = sample_tree();
    let nm = expanded.add_child_raw(NodeId::ROOT, OsStr::new("node_modules"), EntryMeta::dir());
    expanded.add_child_raw(nm, OsStr::new("left-pad"), file(600, 1));
    expanded.recompute_aggregates();
    app.apply(BackendEvent::Tree(Arc::new(expanded)));
    assert_eq!(
        app.tree().unwrap().path(app.cwd()),
        Path::new("/r/node_modules")
    );
    assert_eq!(selected_name(&mut app), "left-pad");
    let text = render_to_text(&mut app, 120, 14).join("\n");
    assert!(!text.contains("expanding"), "{text}");
}

#[test]
fn new_trees_keep_position_and_defer_during_modals() {
    let (mut app, mut rx) = setup(backend());
    press(
        &mut app,
        &mut rx,
        &[ch('j'), ch('j'), key(KeyCode::Enter), ch('j')],
    );
    assert_eq!(selected_name(&mut app), "y");
    let mut fresh = sample_tree();
    let a = fresh.find(Path::new("/r/a")).unwrap();
    fresh.add_child(a, OsStr::new("z"), file(1, 1));
    app.apply(BackendEvent::Tree(Arc::new(fresh.clone())));
    assert_eq!(app.tree().unwrap().path(app.cwd()), Path::new("/r/a"));
    assert_eq!(selected_name(&mut app), "y");
    assert_eq!(app.listing().len(), 3);
    // Classifications survive the swap (re-resolved by path).
    let target = app.tree().unwrap().find(Path::new("/r/c/target")).unwrap();
    assert!(app.classification(target).is_some());

    press(&mut app, &mut rx, &[ch('?')]);
    assert!(matches!(app.modal(), Some(Modal::Help { .. })));
    press(&mut app, &mut rx, &[key(KeyCode::Escape)]);
    press(&mut app, &mut rx, &[ch('p')]);
    assert!(matches!(app.modal(), Some(Modal::PlanPreview { .. })));
    app.apply(BackendEvent::Tree(Arc::new(sample_tree())));
    assert_eq!(app.listing().len(), 3, "deferred while the plan is open");
    press(&mut app, &mut rx, &[key(KeyCode::Escape)]);
    assert_eq!(app.listing().len(), 2, "applied when it closes");
}

#[test]
fn dashboard_toggle_and_quit() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[ch('D')]);
    assert_eq!(app.view(), View::Dashboard);
    press(&mut app, &mut rx, &[key(KeyCode::Escape)]);
    assert_eq!(app.view(), View::Browser);
    assert_eq!(app.handle_key(ch('q')), Handled::Quit);
}

fn dashboard_backend() -> InMemoryBackend {
    let volume = VolumeStats {
        mount_point: "/".into(),
        fs_type: "apfs".into(),
        total: 1_000 << 30,
        free: 50 << 30,
        available: 50 << 30,
        purgeable: Some(5 << 30),
        at: 1_700_000_000,
    };
    backend()
        .with_volumes(vec![volume.clone()])
        .with_alerts(vec![AlertEvent {
            key: "k".into(),
            policy_id: None,
            severity: Severity::Critical,
            title: "Disk almost full".into(),
            body: "5% free".into(),
            volume: Some(volume),
            at: 1_700_000_000,
        }])
}

#[test]
fn render_browser_snapshot() {
    let (mut app, _rx) = setup(backend());
    let screen = render_to_text(&mut app, 120, 14);
    let text = screen.join("\n");
    assert!(screen[0].starts_with(" bclean "), "{text}");
    assert!(screen[0].contains("/r"), "{text}");
    assert!(
        screen[1].contains("1 All") && screen[1].contains("4 Reclaimable"),
        "{text}"
    );
    assert!(
        screen[2].contains("↓Size") && screen[2].contains("Name"),
        "{text}"
    );
    // Rows: c (500 B, contains target), b (300 B, stale), a, .hidden.
    assert!(
        screen[3].contains("500 B") && screen[3].contains("c/"),
        "{text}"
    );
    assert!(screen[3].contains("inside"), "{text}");
    assert!(
        screen[4].contains("300 B") && screen[4].contains("stale 40d"),
        "{text}"
    );
    assert!(screen[4].contains("3.00"), "score column: {text}");
    assert!(screen[5].contains("a/"), "{text}");
    assert!(
        screen[13].contains("move") && screen[13].contains("mark"),
        "{text}"
    );
    assert!(screen[12].contains("960 B"), "status shows totals: {text}");
}

#[test]
fn size_bars_are_scaled_to_the_whole_listing() {
    let mut tree = Tree::new("/r", EntryMeta::dir());
    tree.add_child_raw(NodeId::ROOT, OsStr::new("big"), file(1_000, 1));
    for i in 0..20 {
        tree.add_child_raw(
            NodeId::ROOT,
            OsStr::new(&format!("small{i:02}")),
            file(10, 1),
        );
    }
    tree.recompute_aggregates();
    let (mut app, mut rx) = setup(InMemoryBackend::new(tree));
    let screen = render_to_text(&mut app, 120, 12);
    let text = screen.join("\n");
    assert!(
        screen[3].contains("big") && screen[3].contains("██████████"),
        "{text}"
    );

    // Scrolled past `big`: 10 B rows are 1% of the largest entry, not full bars.
    press(&mut app, &mut rx, &[key(KeyCode::End)]);
    let screen = render_to_text(&mut app, 120, 12);
    let text = screen.join("\n");
    assert!(!text.contains("big"), "{text}");
    let small: Vec<_> = screen
        .iter()
        .filter(|line| line.contains("small"))
        .collect();
    assert!(!small.is_empty(), "{text}");
    // The bar column (not the scrollbar thumb at the right edge) is at most a sliver.
    assert!(
        small
            .iter()
            .all(|line| line.chars().take(40).all(|c| c != '█')),
        "{text}"
    );
}

#[test]
fn render_modals_and_dashboard() {
    let (mut app, mut rx) = setup(dashboard_backend());
    press(&mut app, &mut rx, &[key(KeyCode::Space), ch('p')]);
    let text = render_to_text(&mut app, 100, 20).join("\n");
    assert!(text.contains("Clean plan"), "{text}");
    assert!(text.contains("/r/c"), "{text}");
    press(&mut app, &mut rx, &[ch('y')]);
    let text = render_to_text(&mut app, 100, 20).join("\n");
    assert!(text.contains("Clean complete"), "{text}");
    assert!(text.contains("measured freed"), "{text}");
    press(&mut app, &mut rx, &[key(KeyCode::Enter), ch('?')]);
    let text = render_to_text(&mut app, 100, 30).join("\n");
    assert!(text.contains("Navigate"), "{text}");
    press(&mut app, &mut rx, &[ch('j'); 20]);
    let text = render_to_text(&mut app, 100, 30).join("\n");
    assert!(
        text.contains("dry run") && !text.contains("Navigate"),
        "{text}"
    );
    press(&mut app, &mut rx, &[key(KeyCode::Escape), ch('D')]);
    let text = render_to_text(&mut app, 100, 20).join("\n");
    assert!(text.contains("Volumes") && text.contains("apfs"), "{text}");
    assert!(text.contains("95%"), "{text}");
    assert!(
        text.contains("CRIT") && text.contains("Disk almost full"),
        "{text}"
    );
}

#[test]
fn render_explain_and_progress() {
    let (mut app, mut rx) = setup(backend());
    press(&mut app, &mut rx, &[ch('j'), ch('i')]);
    // Docked on wide terminals …
    let wide = render_to_text(&mut app, 140, 24).join("\n");
    assert!(
        wide.contains("Explain") && wide.contains("Quadrant"),
        "{wide}"
    );
    assert!(wide.contains("durable + stale"), "{wide}");
    // … modal on narrow ones.
    let narrow = render_to_text(&mut app, 80, 24).join("\n");
    assert!(
        narrow.contains("Explain") && narrow.contains("RULE") || narrow.contains("Rule"),
        "{narrow}"
    );

    let (sink, _rx) = EventSink::channel();
    let mut empty = App::new(InMemoryBackend::default(), TuiOptions::default(), sink);
    empty.apply(BackendEvent::Progress(ScanProgress {
        root: PathBuf::from("/home"),
        running: true,
        files: 12_345,
        dirs: 678,
        allocated_bytes: 3 << 30,
        ..ScanProgress::default()
    }));
    let text = render_to_text(&mut empty, 100, 24).join("\n");
    assert!(
        text.contains("Scanning") && text.contains("/home"),
        "{text}"
    );
    assert!(
        text.contains("12,345") && text.contains("3.0 GiB"),
        "{text}"
    );
}

#[test]
fn huge_directory_renders_only_visible_rows() {
    let mut tree = Tree::new("/big", EntryMeta::dir());
    for i in 0..200_000_u64 {
        tree.add_child_raw(NodeId::ROOT, OsStr::new(&format!("f{i}")), file(i, 0));
    }
    tree.recompute_aggregates();
    let (mut app, mut rx) = setup(InMemoryBackend::new(tree));
    let start = std::time::Instant::now();
    for _ in 0..50 {
        press(&mut app, &mut rx, &[key(KeyCode::PageDown)]);
        let _ = render_to_text(&mut app, 120, 40);
    }
    assert_eq!(app.cache.computed, 1, "listing sorted once, then cached");
    assert!(start.elapsed().as_secs() < 20);
}
