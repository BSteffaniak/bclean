//! Interactive demo of the bclean TUI against an in-memory backend.
//!
//! ```sh
//! cargo run -p bclean_tui --example demo -- ~/some/project
//! ```
//!
//! Walks the given directory with `std::fs` (default: the current directory), summarizes
//! `node_modules`/`.git` directories (press enter on them to expand on demand), and attaches
//! synthetic classifications based on well-known names and modification times.
//!
//! Pass `--snapshot` to print one rendered frame instead of starting the interactive UI.
//!
//! Cleaning is SIMULATED: the in-memory backend never touches the filesystem.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bclean_model::{
    ActionSpec, AlertEvent, Classification, Confidence, EphemeralKind, EphemeralSpec, Reason,
    RegenCost, Severity, SignalKind, SignalReading, Staleness, VolumeStats,
};
use bclean_tree::{EntryKind, EntryMeta, NodeFlags, NodeId, SizeMode, Tree};
use bclean_tui::{App, EventSink, InMemoryBackend, TuiOptions, render_to_text, run};

const MAX_ENTRIES: usize = 500_000;
const STALE_AFTER: u64 = 30 * 86_400;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn meta_of(md: &fs::Metadata) -> EntryMeta {
    let kind = if md.is_dir() {
        EntryKind::Dir
    } else if md.file_type().is_symlink() {
        EntryKind::Symlink
    } else if md.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let apparent = if kind == EntryKind::Dir { 0 } else { md.len() };
    #[cfg(unix)]
    let allocated = {
        use std::os::unix::fs::MetadataExt;
        md.blocks() * 512
    };
    #[cfg(not(unix))]
    let allocated = apparent;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    EntryMeta::new(kind, apparent, allocated, mtime)
}

fn is_summarized(name: &OsString) -> bool {
    name == "node_modules" || name == ".git"
}

/// Walks `root` into a tree. Summarized dirs get their total size but no children.
fn walk(root: &Path, summarize: bool, budget: &mut usize) -> Tree {
    let root_meta = fs::symlink_metadata(root).map_or_else(|_| EntryMeta::dir(), |m| meta_of(&m));
    let mut tree = Tree::new(root, root_meta);
    let mut stack = vec![(root.to_path_buf(), NodeId::ROOT)];
    while let Some((dir, id)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            tree.insert_flags(id, NodeFlags::ERROR);
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return finish(tree);
            }
            *budget -= 1;
            let Ok(md) = entry.metadata() else { continue };
            let name = entry.file_name();
            let mut meta = meta_of(&md);
            if meta.kind == EntryKind::Dir && summarize && is_summarized(&name) {
                let sub = walk(&entry.path(), false, budget);
                let info = sub.info(NodeId::ROOT);
                meta.apparent = info.total_apparent;
                meta.allocated = info.total_allocated;
                meta.items = info.items;
                meta.mtime = info.max_mtime;
                meta.flags = NodeFlags::SUMMARIZED;
                tree.add_child_raw(id, &name, meta);
                continue;
            }
            let child = tree.add_child_raw(id, &name, meta);
            if meta.kind == EntryKind::Dir {
                stack.push((entry.path(), child));
            }
        }
    }
    finish(tree)
}

fn finish(mut tree: Tree) -> Tree {
    tree.recompute_aggregates();
    tree
}

fn ephemeral_kind(name: &str) -> Option<(EphemeralKind, RegenCost)> {
    Some(match name {
        "target" | "dist" | "build" | "out" | ".next" | "DerivedData" => {
            (EphemeralKind::BuildArtifact, RegenCost::Cheap)
        }
        "node_modules" | ".venv" | "venv" | "vendor" => {
            (EphemeralKind::Dependencies, RegenCost::Network)
        }
        ".cache" | ".pytest_cache" | "__pycache__" | ".mypy_cache" | ".gradle" => {
            (EphemeralKind::ToolCache, RegenCost::Cheap)
        }
        "tmp" | "temp" => (EphemeralKind::Temp, RegenCost::Free),
        "logs" => (EphemeralKind::Log, RegenCost::Free),
        _ if name.ends_with(".log") => (EphemeralKind::Log, RegenCost::Free),
        _ => return None,
    })
}

#[allow(clippy::cast_precision_loss)]
fn classify(tree: &Tree) -> Vec<(PathBuf, Classification)> {
    let now = now();
    let mut out = Vec::new();
    for id in tree.descendants(NodeId::ROOT) {
        if id == NodeId::ROOT {
            continue;
        }
        let name = tree.name(id).to_string_lossy().into_owned();
        let info = tree.info(id);
        let idle = u64::try_from(now - info.max_mtime).unwrap_or(0);
        let stale = info.max_mtime > 0 && idle > STALE_AFTER;
        let eph = ephemeral_kind(&name);
        let big_old_file = eph.is_none()
            && stale
            && info.kind == EntryKind::File
            && info.total_allocated > 50 << 20;
        if eph.is_none() && !big_old_file {
            continue;
        }
        let size = info.total_allocated;
        let (confidence, regen) = eph.map_or((Confidence::Low, 4.0), |(_, cost)| {
            (Confidence::High, cost.weight())
        });
        let mib = (size as f64 / f64::from(1 << 20)).max(1.0);
        let score =
            mib.log2().max(0.1) * confidence.weight() * if stale { 2.0 } else { 1.0 } / regen;
        out.push((
            tree.path(id),
            Classification {
                rule_id: Some(
                    eph.map_or("durable.large-old-file", |_| "demo.well-known-name")
                        .to_owned(),
                ),
                category: Some(eph.map_or("downloads", |_| "dev").to_owned()),
                ephemeral: eph.map(|(kind, regen_cost)| EphemeralSpec { kind, regen_cost }),
                confidence,
                staleness: Staleness {
                    stale,
                    last_activity: Some(info.max_mtime),
                    idle_secs: Some(idle),
                    threshold_secs: STALE_AFTER,
                    signals: vec![SignalReading {
                        kind: SignalKind::FsTimes,
                        last_activity: Some(info.max_mtime),
                        forces_stale: false,
                        detail: "newest mtime in subtree".to_owned(),
                    }],
                },
                size,
                score,
                action: if eph.is_some() {
                    ActionSpec::Delete
                } else {
                    ActionSpec::Trash
                },
                auto: eph.is_some() && stale,
                reasons: vec![
                    Reason::new("RULE_MATCH", format!("name `{name}` matches demo rule")),
                    Reason::new("SIGNAL", format!("idle for {} days", idle / 86_400)),
                ],
            },
        ));
    }
    out
}

fn main() {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let snapshot = args
        .iter()
        .position(|a| a == "--snapshot")
        .map(|i| args.remove(i));
    let root = args.into_iter().next().map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let root = fs::canonicalize(&root).unwrap_or(root);
    eprintln!("walking {} …", root.display());
    let mut budget = MAX_ENTRIES;
    let tree = walk(&root, true, &mut budget);

    let mut backend =
        InMemoryBackend::new(tree.clone()).with_item_delay(Duration::from_millis(150));
    // Register expandable subtrees for every summarized directory.
    let summarized: Vec<NodeId> = tree
        .descendants(NodeId::ROOT)
        .filter(|id| tree.flags(*id).contains(NodeFlags::SUMMARIZED))
        .collect();
    let mut classifications = classify(&tree);
    for id in summarized {
        let path = tree.path(id);
        let mut budget = MAX_ENTRIES;
        let sub = walk(&path, false, &mut budget);
        classifications.extend(classify(&sub));
        backend = backend.with_subtree(path, sub);
    }

    let total = tree.size(NodeId::ROOT, SizeMode::Allocated);
    let volume = VolumeStats {
        mount_point: "/".to_owned(),
        fs_type: "demo".to_owned(),
        total: (total * 4).max(1 << 30),
        free: total,
        available: total,
        purgeable: Some(total / 10),
        at: now(),
    };
    let alerts = vec![AlertEvent {
        key: "demo.low-space".to_owned(),
        policy_id: Some("low-space".to_owned()),
        severity: Severity::Warning,
        title: "Disk usage above 75%".to_owned(),
        body: "synthetic alert from the demo backend".to_owned(),
        volume: Some(volume.clone()),
        at: now(),
    }];
    let backend = backend
        .with_classifications(classifications)
        .with_volumes(vec![
            volume,
            VolumeStats {
                mount_point: "/Volumes/Data".to_owned(),
                fs_type: "demo".to_owned(),
                total: 2 << 40,
                free: 1 << 37,
                available: 1 << 37,
                purgeable: None,
                at: now(),
            },
        ])
        .with_alerts(alerts);

    let options = TuiOptions {
        title: "bclean demo".to_owned(),
        ..TuiOptions::default()
    };
    if snapshot.is_some() {
        // Headless: render one frame to stdout (useful without an interactive terminal).
        let (sink, mut events) = EventSink::channel();
        let mut app = App::new(backend, options, sink);
        while let Some(event) = events.try_recv() {
            app.apply(event);
        }
        for line in render_to_text(&mut app, 120, 32) {
            println!("{line}");
        }
        return;
    }
    match run(backend, options) {
        Ok(exit) => {
            for path in exit.marked {
                println!("{}", path.display());
            }
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
