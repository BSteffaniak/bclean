//! Portable change source on top of `notify` (inotify on Linux, `ReadDirectoryChangesW` on
//! Windows). No replay: the cursor is always `None`.
//!
//! Each event marks the *parent* directory of the changed path dirty (the directory whose
//! listing or child sizes changed). Queue overflows (`IN_Q_OVERFLOW`, `ReadDirectoryChangesW`
//! buffer overruns) and backend errors are reported as a full rescan of every root.

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};

use super::{
    ChangeBatch, ChangeSource, Pipeline, RawEvent, Roots, WatchBackend, WatchConfig, WatchError,
    spawn_coalescer,
};

/// `notify`-backed [`ChangeSource`].
pub struct NotifySource {
    // Field order matters: the watcher (holding the raw sender) drops first, which lets the
    // coalescer flush and disconnect the batch channel.
    _watcher: notify::RecommendedWatcher,
    pipeline: Pipeline,
}

impl NotifySource {
    pub fn start(config: &WatchConfig) -> Result<Self, WatchError> {
        let roots = Arc::new(Roots::new(&config.roots));
        let (tx, pipeline) = spawn_coalescer(Arc::clone(&roots), config)?;
        let root_list = roots.originals();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            handle(&tx, &root_list, res);
        })
        .map_err(|e| WatchError::Backend(e.to_string()))?;
        for root in roots.originals() {
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .map_err(|e| {
                    WatchError::Backend(format!("cannot watch {}: {e}", root.display()))
                })?;
        }
        Ok(Self {
            _watcher: watcher,
            pipeline,
        })
    }
}

fn handle(tx: &Sender<RawEvent>, roots: &[std::path::PathBuf], res: notify::Result<Event>) {
    let event = match res {
        Ok(event) => event,
        Err(e) => {
            log::warn!("filesystem watch error: {e}");
            let _ = tx.send(RawEvent::RescanAll);
            return;
        }
    };
    if event.need_rescan() {
        let _ = tx.send(RawEvent::RescanAll);
        return;
    }
    match event.kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write))
        | EventKind::Any
        | EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Other => {}
        EventKind::Access(_) => return,
    }
    for path in event.paths {
        let dir = if roots.iter().any(|r| r == &path) {
            path
        } else {
            path.parent()
                .map_or_else(|| path.clone(), Path::to_path_buf)
        };
        let _ = tx.send(RawEvent::Dirty(dir));
    }
}

impl ChangeSource for NotifySource {
    fn backend(&self) -> WatchBackend {
        WatchBackend::Notify
    }

    fn batches(&self) -> &Receiver<ChangeBatch> {
        &self.pipeline.rx
    }
}
