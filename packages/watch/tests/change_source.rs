//! End-to-end change source tests against real temp directories.
//!
//! These depend on OS event delivery, so they poll with generous timeouts and re-touch files
//! periodically instead of relying on a single event.

#![cfg(any(target_os = "macos", target_os = "linux", windows))]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use bclean_watch::{ChangeBatch, ChangeSource, WatchConfig, open_change_source};

const TIMEOUT: Duration = Duration::from_secs(30);

fn config(root: &Path) -> WatchConfig {
    WatchConfig {
        debounce: Duration::from_millis(50),
        max_latency: Duration::from_millis(500),
        ..WatchConfig::new(vec![root.to_path_buf()])
    }
}

/// Waits for a batch satisfying `pred`, calling `poke` about once a second.
fn wait_for(
    source: &dyn ChangeSource,
    mut poke: impl FnMut(usize),
    pred: impl Fn(&ChangeBatch) -> bool,
) -> Option<ChangeBatch> {
    let deadline = Instant::now() + TIMEOUT;
    let mut round = 0;
    poke(round);
    while Instant::now() < deadline {
        match source.batches().recv_timeout(Duration::from_secs(1)) {
            Ok(batch) if pred(&batch) => return Some(batch),
            Ok(_) => {}
            Err(_) => {
                round += 1;
                poke(round);
            }
        }
    }
    None
}

fn covers(batch: &ChangeBatch, dir: &Path) -> bool {
    batch.dirty.iter().any(|d| dir.starts_with(d))
}

#[test]
fn reports_parent_of_created_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let source = open_change_source(&config(tmp.path())).unwrap();
    let batch = wait_for(
        source.as_ref(),
        |i| fs::write(sub.join(format!("f{i}.txt")), b"hello").unwrap(),
        |b| covers(b, &sub),
    );
    let batch =
        batch.unwrap_or_else(|| panic!("no batch covering {} within {TIMEOUT:?}", sub.display()));
    // Paths come back in the caller's spelling and never above the root.
    assert!(
        batch.dirty.iter().all(|d| d.starts_with(tmp.path())),
        "{batch:?}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn fsevents_replays_changes_or_requests_a_safe_rescan() {
    use bclean_watch::WatchBackend;
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("replay");
    fs::create_dir(&sub).unwrap();
    let cursor = {
        let source = open_change_source(&config(tmp.path())).unwrap();
        assert_eq!(source.backend(), WatchBackend::FsEvents);
        assert!(source.supports_replay());
        source.current_cursor().unwrap()
    };
    // Changes made while no watcher is running.
    fs::write(sub.join("offline.txt"), b"x").unwrap();
    let source = open_change_source(&WatchConfig {
        since: Some(cursor),
        ..config(tmp.path())
    })
    .unwrap();
    let batch = wait_for(source.as_ref(), |_| {}, |b| covers(b, &sub));
    let batch = batch.expect("offline change was neither replayed nor covered by a rescan");
    // Replay is best-effort: an unavailable event-store UUID (for example on
    // a sandboxed /nix volume), expired history, or dropped events requires a
    // full rescan. That is the safe recovery path, not a replay failure.
    if batch.must_full_rescan {
        assert!(
            batch.dirty.iter().all(|path| path.starts_with(tmp.path())),
            "rescan escaped the watched root: {batch:?}"
        );
    } else {
        assert!(
            batch.cursor.is_some(),
            "replay must advance its cursor: {batch:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn fsevents_missing_volume_identity_forces_full_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let cursor = bclean_watch::WatchCursor::from_bytes(
        br#"{"backend":"FS_EVENTS","event_id":1,"volume_uuids":{}}"#,
    )
    .unwrap();
    let source = open_change_source(&WatchConfig {
        since: Some(cursor),
        ..config(tmp.path())
    })
    .unwrap();
    let batch = source
        .batches()
        .recv_timeout(TIMEOUT)
        .expect("no rescan batch");
    assert!(batch.must_full_rescan);
    assert_eq!(batch.dirty, vec![tmp.path().to_path_buf()]);
}

#[cfg(target_os = "macos")]
#[test]
fn fsevents_foreign_cursor_forces_full_rescan() {
    let tmp = tempfile::tempdir().unwrap();
    let bogus = bclean_watch::WatchCursor::from_bytes(
        br#"{"backend":"FS_EVENTS","event_id":1,"volume_uuids":{"/":"00000000-0000-0000-0000-000000000000"}}"#,
    )
    .unwrap();
    let source = open_change_source(&WatchConfig {
        since: Some(bogus),
        ..config(tmp.path())
    })
    .unwrap();
    let batch = source
        .batches()
        .recv_timeout(TIMEOUT)
        .expect("no rescan batch");
    assert!(batch.must_full_rescan);
    assert_eq!(batch.dirty, vec![tmp.path().to_path_buf()]);
}
