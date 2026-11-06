//! Facade opening snapshots and history under one data directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bclean_tree::{Tree, TreeParts};
use xxhash_rust::xxh3::xxh3_64;

use crate::history::{History, HistoryError};
use crate::pathbytes::os_to_bytes;
use crate::snapshot::{self, Snapshot, SnapshotError, SnapshotHeader, SnapshotMeta};

/// Sub-directory holding snapshot files.
const SNAPSHOT_DIR: &str = "snapshots";
/// Snapshot file extension.
const SNAPSHOT_EXT: &str = "snap";
/// History database file name.
const HISTORY_FILE: &str = "history.sqlite3";

/// Errors from [`Store`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Creating or listing the data directory failed.
    #[error("data directory error: {0}")]
    Io(#[from] io::Error),
    /// Snapshot failure.
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
    /// History failure.
    #[error(transparent)]
    History(#[from] HistoryError),
}

/// All persistent index state under one data directory:
///
/// ```text
/// <data_dir>/history.sqlite3          volume samples + directory rollups
/// <data_dir>/snapshots/<hash>.snap    one tree snapshot per scanned root
/// ```
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    history: History,
}

impl Store {
    /// Opens (creating if needed) the store under `data_dir`.
    ///
    /// # Errors
    ///
    /// * [`StoreError::Io`] if the directories cannot be created
    /// * [`StoreError::History`] if the history database cannot be opened or migrated
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let dir = data_dir.as_ref().to_path_buf();
        fs::create_dir_all(dir.join(SNAPSHOT_DIR))?;
        let history = History::open(&dir.join(HISTORY_FILE)).await?;
        Ok(Self { dir, history })
    }

    /// The data directory.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.dir
    }

    /// The history database (all of its methods take `&self`).
    #[must_use]
    pub const fn history(&self) -> &History {
        &self.history
    }

    /// Path of the snapshot file for `root` (named by a hash of the root path).
    #[must_use]
    pub fn snapshot_path(&self, root: &Path) -> PathBuf {
        let hash = xxh3_64(&os_to_bytes(root.as_os_str()));
        self.dir
            .join(SNAPSHOT_DIR)
            .join(format!("{hash:016x}.{SNAPSHOT_EXT}"))
    }

    /// Atomically saves a snapshot of `tree` (cloned into parts). Returns bytes written.
    ///
    /// # Errors
    ///
    /// * [`StoreError::Snapshot`] if writing fails
    pub fn save_snapshot(&self, tree: &Tree, meta: &SnapshotMeta) -> Result<u64, StoreError> {
        Ok(snapshot::write_tree(
            &self.snapshot_path(tree.root_path()),
            tree,
            meta,
        )?)
    }

    /// Atomically saves a snapshot from raw parts. Returns bytes written.
    ///
    /// # Errors
    ///
    /// * [`StoreError::Snapshot`] if writing fails
    pub fn save_snapshot_parts(
        &self,
        parts: &TreeParts,
        meta: &SnapshotMeta,
    ) -> Result<u64, StoreError> {
        Ok(snapshot::write_parts(
            &self.snapshot_path(&parts.root_path),
            parts,
            meta,
        )?)
    }

    /// Loads the snapshot for `root`, or `None` if there is none.
    ///
    /// Callers should treat an error with [`SnapshotError::is_invalid`] as "no usable snapshot",
    /// rescan, and overwrite it.
    ///
    /// # Errors
    ///
    /// * [`StoreError::Snapshot`] if the file is unreadable, corrupt, from another format
    ///   version, or recorded for a different root (hash collision)
    pub fn load_snapshot(&self, root: &Path) -> Result<Option<Snapshot>, StoreError> {
        match snapshot::read(&self.snapshot_path(root), Some(root)) {
            Ok(snap) => Ok(Some(snap)),
            Err(SnapshotError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Deletes the snapshot for `root`. Returns whether one existed.
    ///
    /// # Errors
    ///
    /// * [`StoreError::Io`] if the file exists but cannot be removed
    pub fn remove_snapshot(&self, root: &Path) -> Result<bool, StoreError> {
        match fs::remove_file(self.snapshot_path(root)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Headers of all readable snapshots (unreadable files are skipped and logged).
    ///
    /// # Errors
    ///
    /// * [`StoreError::Io`] if the snapshot directory cannot be listed
    pub fn snapshots(&self) -> Result<Vec<SnapshotHeader>, StoreError> {
        let mut out = Vec::new();
        for entry in fs::read_dir(self.dir.join(SNAPSHOT_DIR))? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != SNAPSHOT_EXT) {
                continue;
            }
            match snapshot::read_header(&path) {
                Ok(h) => out.push(h),
                Err(e) => log::warn!("skipping unreadable snapshot {}: {e}", path.display()),
            }
        }
        out.sort_by(|a, b| a.root_path.cmp(&b.root_path));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_tree::{EntryKind, EntryMeta, NodeId};
    use std::ffi::OsStr;

    #[tokio::test]
    async fn store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("data")).await.unwrap();
        let root = dir.path().join("scanned");
        assert!(store.load_snapshot(&root).unwrap().is_none());

        let mut tree = Tree::new(&root, EntryMeta::dir());
        tree.add_child(
            NodeId::ROOT,
            OsStr::new("f"),
            EntryMeta::new(EntryKind::File, 3, 4096, 1),
        );
        let meta = SnapshotMeta {
            scanned_at: 42,
            backend: "PORTABLE".into(),
            cursor: None,
        };
        store.save_snapshot(&tree, &meta).unwrap();
        let snap = store.load_snapshot(&root).unwrap().unwrap();
        assert_eq!(snap.meta, meta);
        assert_eq!(snap.tree.len(), 2);
        assert_eq!(store.snapshots().unwrap().len(), 1);

        store.history().record_rollup(&tree, 42, 3).await.unwrap();
        assert_eq!(
            store.history().size_series(&root, 0).await.unwrap()[0].allocated,
            4096
        );

        fs::write(store.snapshot_path(&root), b"garbage").unwrap();
        let err = store.load_snapshot(&root).unwrap_err();
        assert!(matches!(err, StoreError::Snapshot(ref e) if e.is_invalid()));
        assert_eq!(store.snapshots().unwrap().len(), 0);
        assert!(store.remove_snapshot(&root).unwrap());
        assert!(!store.remove_snapshot(&root).unwrap());
        drop(store);
        Store::open(dir.path().join("data")).await.unwrap();
    }
}
