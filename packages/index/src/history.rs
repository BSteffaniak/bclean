//! SQLite-backed usage history (via `switchy_database`): volume samples and directory size
//! rollups.
//!
//! # Schema (v1)
//!
//! * `history_schema(contract_id = 1, schema_version)`: the schema contract row.
//! * `volumes(id, mount_point UNIQUE, fs_type)` and
//!   `volume_samples(volume_id, at, total, free, available, purgeable)`, keyed by
//!   `(volume_id, at)`.
//! * `paths(id, parent_id, name)`: a path *tree* so each directory stores only its own name
//!   (`parent_id = 0` rows hold a full root path). Names are stored as TEXT: valid UTF-8 is kept
//!   verbatim, `\` is escaped as `\\`, and bytes that are not UTF-8 become `\xHH` (lossless on
//!   Unix). TEXT is used because typed `switchy_database` queries do not return BLOB columns.
//! * `rollups(id, root_id, at, depth)` and
//!   `rollup_sizes(rollup_id, path_id, allocated, apparent, items)`, keyed by
//!   `(rollup_id, path_id)`.
//!
//! [`History::open`] applies pending migrations in order inside a transaction and refuses
//! databases written by a newer version. Referential integrity (sizes of a rollup, orphaned
//! paths) is maintained by this module rather than by foreign keys.
//!
//! Sizes are stored as SQLite `INTEGER` (i64) and saturate at `i64::MAX`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use bclean_model::VolumeStats;
use bclean_tree::{EntryKind, NodeId, Tree};
use switchy_database::query::{FilterableQuery as _, SortDirection, identifier, where_in};
use switchy_database::schema::{Column, DataType, create_index, create_table};
use switchy_database::{Database, DatabaseValue, Row, SqliteJournalMode};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

use crate::pathbytes::{bytes_to_os, os_to_bytes};

/// Latest schema version understood by this build.
pub const SCHEMA_VERSION: i64 = 1;

/// Maximum number of values bound in one `IN (...)` list.
const IN_CHUNK: usize = 500;
/// Size rows buffered before they are written.
const SIZE_BATCH: usize = 2_000;

/// Errors from the history database.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// Opening or configuring the SQLite connection failed.
    #[error("cannot open history database: {0}")]
    Open(#[from] switchy_database_connection::InitSqliteRusqliteError),
    /// A database operation failed.
    #[error("history database error: {0}")]
    Database(#[from] switchy_database::DatabaseError),
    /// The database was written by a newer bclean.
    #[error("history schema version {found} is newer than supported version {supported}")]
    FutureSchema {
        /// Version found in the file.
        found: i64,
        /// Newest version this build supports.
        supported: i64,
    },
    /// A row did not have the expected shape.
    #[error("invalid history data: {0}")]
    InvalidData(String),
}

/// One directory size observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizePoint {
    /// Rollup time (Unix seconds).
    pub at: i64,
    /// Allocated bytes of the subtree.
    pub allocated: u64,
    /// Apparent bytes of the subtree.
    pub apparent: u64,
    /// Items in the subtree.
    pub items: u64,
}

/// Growth of one directory between two rollups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirGrowth {
    /// Directory path.
    pub path: PathBuf,
    /// Baseline rollup time.
    pub from_at: i64,
    /// Latest rollup time.
    pub to_at: i64,
    /// Allocated bytes at the baseline (0 if the directory is new).
    pub before_allocated: u64,
    /// Allocated bytes now.
    pub after_allocated: u64,
    /// Apparent bytes at the baseline (0 if the directory is new).
    pub before_apparent: u64,
    /// Apparent bytes now.
    pub after_apparent: u64,
}

impl DirGrowth {
    /// Change in allocated bytes.
    #[must_use]
    pub fn delta_allocated(&self) -> i128 {
        i128::from(self.after_allocated) - i128::from(self.before_allocated)
    }

    /// Change in apparent bytes.
    #[must_use]
    pub fn delta_apparent(&self) -> i128 {
        i128::from(self.after_apparent) - i128::from(self.before_apparent)
    }
}

/// Linear fill-rate estimate for a volume.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeToFull {
    /// Bytes consumed per second (positive while filling, negative while freeing).
    pub fill_rate: f64,
    /// Seconds from the last sample until available space reaches zero, if filling.
    pub seconds_to_full: Option<f64>,
    /// Available bytes at the last sample.
    pub available: u64,
    /// Time of the last sample (Unix seconds).
    pub at: i64,
    /// Number of samples used.
    pub samples: usize,
}

impl TimeToFull {
    /// Estimated Unix time at which the volume fills, if filling.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)] // far-future estimates saturate
    pub fn full_at(&self) -> Option<i64> {
        self.seconds_to_full
            .map(|s| self.at.saturating_add(s as i64))
    }
}

/// Rows removed by [`History::prune`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Volume samples removed.
    pub volume_samples: usize,
    /// Rollups removed (with all their sizes).
    pub rollups: usize,
    /// Orphaned path rows removed.
    pub paths: usize,
}

/// Estimates the fill rate and time to full by least-squares regression of available bytes over
/// time.
///
/// Returns `None` with fewer than two samples or when all samples share one timestamp.
#[must_use]
#[allow(clippy::cast_precision_loss)] // byte counts and timestamps are fine as f64 for a trend
pub fn estimate_time_to_full(samples: &[VolumeStats]) -> Option<TimeToFull> {
    let last = samples.iter().max_by_key(|s| s.at)?;
    if samples.len() < 2 {
        return None;
    }
    let t0 = last.at;
    let n = samples.len() as f64;
    let (mut sx, mut sy) = (0.0, 0.0);
    for s in samples {
        sx += (s.at - t0) as f64;
        sy += s.available as f64;
    }
    let (mx, my) = (sx / n, sy / n);
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for s in samples {
        let dx = (s.at - t0) as f64 - mx;
        sxy = dx.mul_add(s.available as f64 - my, sxy);
        sxx = dx.mul_add(dx, sxx);
    }
    if sxx <= 0.0 {
        return None;
    }
    let fill_rate = -(sxy / sxx);
    let seconds_to_full = (fill_rate > 0.0).then(|| last.available as f64 / fill_rate);
    Some(TimeToFull {
        fill_rate,
        seconds_to_full,
        available: last.available,
        at: last.at,
        samples: samples.len(),
    })
}

/// Handle to the history database.
#[derive(Debug)]
pub struct History {
    db: Box<dyn Database>,
}

impl History {
    /// Opens (creating if needed) the database at `path`, enables WAL, and applies migrations.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Open`] if the file cannot be opened or configured
    /// * [`HistoryError::Database`] if a migration fails
    /// * [`HistoryError::FutureSchema`] if the database was written by a newer version
    pub async fn open(path: &Path) -> Result<Self, HistoryError> {
        let db = open_connection(path)?;
        db.sqlite_journal_mode(SqliteJournalMode::Wal).await?;
        Self::from_database(Box::new(db)).await
    }

    /// Opens a private in-memory database (for tests and dry runs).
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Open`] if SQLite fails to initialize
    /// * [`HistoryError::Database`] if creating the schema fails
    pub async fn open_in_memory() -> Result<Self, HistoryError> {
        Self::from_database(Box::new(open_connection(Path::new(":memory:"))?)).await
    }

    /// Wraps an existing connection and applies migrations.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a migration fails
    /// * [`HistoryError::FutureSchema`] if the database was written by a newer version
    pub async fn from_database(db: Box<dyn Database>) -> Result<Self, HistoryError> {
        migrate(&*db).await?;
        Ok(Self { db })
    }

    /// The underlying connection.
    #[must_use]
    pub fn database(&self) -> &dyn Database {
        &*self.db
    }

    /// Current schema version of the open database.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if the contract row cannot be read
    pub async fn schema_version(&self) -> Result<i64, HistoryError> {
        schema_version(&*self.db).await
    }

    /// Records volume samples in one transaction (a repeated `(mount_point, at)` overwrites).
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a write fails
    pub async fn record_volumes(&self, samples: &[VolumeStats]) -> Result<(), HistoryError> {
        let tx = self.db.begin_transaction().await?;
        for s in samples {
            let row = tx
                .upsert("volumes")
                .where_eq("mount_point", s.mount_point.as_str())
                .value("mount_point", s.mount_point.as_str())
                .value("fs_type", s.fs_type.as_str())
                .execute_first(&*tx)
                .await?;
            let volume_id = int(&row, "id")?;
            tx.upsert("volume_samples")
                .where_eq("volume_id", volume_id)
                .where_eq("at", s.at)
                .value("volume_id", volume_id)
                .value("at", s.at)
                .value("total", to_sql(s.total))
                .value("free", to_sql(s.free))
                .value("available", to_sql(s.available))
                .value("purgeable", s.purgeable.map(to_sql))
                .execute(&*tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records one volume sample.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a write fails
    pub async fn record_volume(&self, sample: &VolumeStats) -> Result<(), HistoryError> {
        self.record_volumes(std::slice::from_ref(sample)).await
    }

    /// Mount points with recorded samples.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if the query fails
    pub async fn volumes(&self) -> Result<Vec<String>, HistoryError> {
        let rows = self
            .db
            .select("volumes")
            .columns(&["mount_point"])
            .sort("mount_point", SortDirection::Asc)
            .execute(&*self.db)
            .await?;
        rows.iter().map(|r| text(r, "mount_point")).collect()
    }

    /// Samples for `mount_point` with `at >= since`, oldest first.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if the query fails
    pub async fn volume_series(
        &self,
        mount_point: &str,
        since: i64,
    ) -> Result<Vec<VolumeStats>, HistoryError> {
        let Some(volume) = self
            .db
            .select("volumes")
            .where_eq("mount_point", mount_point)
            .execute_first(&*self.db)
            .await?
        else {
            return Ok(Vec::new());
        };
        let rows = self
            .db
            .select("volume_samples")
            .where_eq("volume_id", int(&volume, "id")?)
            .where_gte("at", since)
            .sort("at", SortDirection::Asc)
            .execute(&*self.db)
            .await?;
        let fs_type = text(&volume, "fs_type")?;
        rows.iter()
            .map(|r| volume_stats(r, mount_point, &fs_type))
            .collect()
    }

    /// The most recent sample of every volume.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a query fails
    pub async fn latest_volumes(&self) -> Result<Vec<VolumeStats>, HistoryError> {
        let volumes = self
            .db
            .select("volumes")
            .sort("mount_point", SortDirection::Asc)
            .execute(&*self.db)
            .await?;
        let mut out = Vec::with_capacity(volumes.len());
        for volume in &volumes {
            let latest = self
                .db
                .select("volume_samples")
                .where_eq("volume_id", int(volume, "id")?)
                .sort("at", SortDirection::Desc)
                .limit(1)
                .execute_first(&*self.db)
                .await?;
            if let Some(row) = latest {
                out.push(volume_stats(
                    &row,
                    &text(volume, "mount_point")?,
                    &text(volume, "fs_type")?,
                )?);
            }
        }
        Ok(out)
    }

    /// Fill-rate estimate for `mount_point` over samples with `at >= since`.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if the query fails
    pub async fn time_to_full(
        &self,
        mount_point: &str,
        since: i64,
    ) -> Result<Option<TimeToFull>, HistoryError> {
        Ok(estimate_time_to_full(
            &self.volume_series(mount_point, since).await?,
        ))
    }

    /// Records the sizes of every directory of `tree` at depth `<= max_depth` (the root is depth
    /// 0) as one rollup at time `at`, in one transaction. Returns the rollup id.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a write fails
    pub async fn record_rollup(
        &self,
        tree: &Tree,
        at: i64,
        max_depth: usize,
    ) -> Result<i64, HistoryError> {
        let tx = self.db.begin_transaction().await?;
        let db: &dyn Database = &*tx;
        let root_id = path_get_or_insert(
            db,
            0,
            &encode_name(&os_to_bytes(tree.root_path().as_os_str())),
        )
        .await?;
        let rollup = db
            .insert("rollups")
            .value("root_id", root_id)
            .value("at", at)
            .value("depth", i64::try_from(max_depth).unwrap_or(i64::MAX))
            .execute(db)
            .await?;
        let rollup_id = int(&rollup, "id")?;

        let mut batch: Vec<[i64; 4]> = Vec::with_capacity(SIZE_BATCH);
        let mut stack = vec![(NodeId::ROOT, root_id, 0_usize)];
        while let Some((node, path_id, depth)) = stack.pop() {
            let info = tree.info(node);
            batch.push([
                path_id,
                to_sql(info.total_allocated),
                to_sql(info.total_apparent),
                to_sql(info.items),
            ]);
            if batch.len() >= SIZE_BATCH {
                insert_sizes(db, rollup_id, &std::mem::take(&mut batch)).await?;
            }
            if depth < max_depth {
                for child in tree.children(node) {
                    if tree.kind(child) == EntryKind::Dir {
                        let name = encode_name(&os_to_bytes(tree.name(child)));
                        let id = path_get_or_insert(db, path_id, &name).await?;
                        stack.push((child, id, depth + 1));
                    }
                }
            }
        }
        insert_sizes(db, rollup_id, &batch).await?;
        tx.commit().await?;
        Ok(rollup_id)
    }

    /// Size history of the directory at `path` across rollups with `at >= since`, oldest first.
    /// Returns an empty series for paths that were never recorded.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a query fails
    pub async fn size_series(
        &self,
        path: &Path,
        since: i64,
    ) -> Result<Vec<SizePoint>, HistoryError> {
        let Some(id) = self.path_id(path).await? else {
            return Ok(Vec::new());
        };
        let sizes = self
            .db
            .select("rollup_sizes")
            .where_eq("path_id", id)
            .execute(&*self.db)
            .await?;
        let rollup_ids: Vec<i64> = sizes
            .iter()
            .map(|r| int(r, "rollup_id"))
            .collect::<Result<_, _>>()?;
        let times = self.rollup_times(&rollup_ids).await?;
        let mut out = Vec::with_capacity(sizes.len());
        for row in &sizes {
            let Some(&at) = times.get(&int(row, "rollup_id")?) else {
                continue;
            };
            if at >= since {
                out.push(SizePoint {
                    at,
                    allocated: from_sql(int(row, "allocated")?),
                    apparent: from_sql(int(row, "apparent")?),
                    items: from_sql(int(row, "items")?),
                });
            }
        }
        out.sort_by_key(|p| p.at);
        Ok(out)
    }

    /// Directories under `root` (excluding `root` itself) that grew the most in allocated bytes
    /// between the rollup nearest to `since` and the latest rollup containing `root`.
    ///
    /// Directories that are new since the baseline count from zero. Only positive growth is
    /// returned, largest first, at most `limit` entries. Returns an empty list when fewer than two
    /// rollups contain `root`.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a query fails
    pub async fn top_growth(
        &self,
        root: &Path,
        since: i64,
        limit: usize,
    ) -> Result<Vec<DirGrowth>, HistoryError> {
        let Some(root_id) = self.path_id(root).await? else {
            return Ok(Vec::new());
        };
        let containing = self
            .db
            .select("rollup_sizes")
            .columns(&["rollup_id"])
            .where_eq("path_id", root_id)
            .execute(&*self.db)
            .await?;
        let ids: Vec<i64> = containing
            .iter()
            .map(|r| int(r, "rollup_id"))
            .collect::<Result<_, _>>()?;
        let times = self.rollup_times(&ids).await?;
        let Some((&latest_id, &latest_at)) = times.iter().max_by_key(|(id, at)| (**at, **id))
        else {
            return Ok(Vec::new());
        };
        let Some((&base_id, &base_at)) = times
            .iter()
            .filter(|(id, at)| **id != latest_id && **at <= latest_at)
            .min_by_key(|(_, at)| ((i128::from(**at) - i128::from(since)).abs(), **at))
        else {
            return Ok(Vec::new());
        };

        let subtree = self.descendants(root_id).await?;
        let after = self.rollup_sizes(latest_id).await?;
        let before = self.rollup_sizes(base_id).await?;
        let mut grown: Vec<(i64, [u64; 4])> = after
            .iter()
            .filter(|(id, _)| subtree.contains(id))
            .filter_map(|(id, (alloc, app))| {
                let (b_alloc, b_app) = before.get(id).copied().unwrap_or((0, 0));
                (*alloc > b_alloc).then_some((*id, [b_alloc, *alloc, b_app, *app]))
            })
            .collect();
        grown.sort_by(|a, b| {
            (b.1[1] - b.1[0])
                .cmp(&(a.1[1] - a.1[0]))
                .then(a.0.cmp(&b.0))
        });
        grown.truncate(limit);

        let mut cache = BTreeMap::new();
        let mut out = Vec::with_capacity(grown.len());
        for (
            id,
            [
                before_allocated,
                after_allocated,
                before_apparent,
                after_apparent,
            ],
        ) in grown
        {
            out.push(DirGrowth {
                path: self.path_of(id, &mut cache).await?,
                from_at: base_at,
                to_at: latest_at,
                before_allocated,
                after_allocated,
                before_apparent,
                after_apparent,
            });
        }
        Ok(out)
    }

    /// Deletes samples and rollups older than `before` (Unix seconds) and garbage-collects
    /// path rows no longer referenced, in one transaction.
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a query or delete fails
    pub async fn prune(&self, before: i64) -> Result<PruneStats, HistoryError> {
        let tx = self.db.begin_transaction().await?;
        let db: &dyn Database = &*tx;
        let volume_samples = count(
            db.exec_delete_count(&db.delete("volume_samples").where_lt("at", before))
                .await?,
        );

        let old = db
            .select("rollups")
            .columns(&["id"])
            .where_lt("at", before)
            .execute(db)
            .await?;
        let old: Vec<i64> = old.iter().map(|r| int(r, "id")).collect::<Result<_, _>>()?;
        for chunk in old.chunks(IN_CHUNK) {
            db.exec_delete_count(
                &db.delete("rollup_sizes")
                    .filter(Box::new(where_in("rollup_id", chunk.to_vec()))),
            )
            .await?;
        }
        let rollups = count(
            db.exec_delete_count(&db.delete("rollups").where_lt("at", before))
                .await?,
        );

        // Keep every referenced path and its ancestors; delete the rest.
        let mut referenced: BTreeSet<i64> = BTreeSet::new();
        for row in db
            .select("rollup_sizes")
            .columns(&["path_id"])
            .distinct()
            .execute(db)
            .await?
        {
            referenced.insert(int(&row, "path_id")?);
        }
        for row in db
            .select("rollups")
            .columns(&["root_id"])
            .distinct()
            .execute(db)
            .await?
        {
            referenced.insert(int(&row, "root_id")?);
        }
        let mut parents = BTreeMap::new();
        for row in db
            .select("paths")
            .columns(&["id", "parent_id"])
            .execute(db)
            .await?
        {
            parents.insert(int(&row, "id")?, int(&row, "parent_id")?);
        }
        let mut keep = BTreeSet::new();
        for id in referenced {
            let mut cur = id;
            while cur != 0 && keep.insert(cur) {
                cur = parents.get(&cur).copied().unwrap_or(0);
            }
        }
        let orphans: Vec<i64> = parents
            .keys()
            .copied()
            .filter(|id| !keep.contains(id))
            .collect();
        for chunk in orphans.chunks(IN_CHUNK) {
            db.exec_delete_count(
                &db.delete("paths")
                    .filter(Box::new(where_in("id", chunk.to_vec()))),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(PruneStats {
            volume_samples,
            rollups,
            paths: orphans.len(),
        })
    }

    /// Prunes everything older than `retention` before `now` (Unix seconds).
    ///
    /// # Errors
    ///
    /// * [`HistoryError::Database`] if a query or delete fails
    pub async fn prune_retention(
        &self,
        retention: Duration,
        now: i64,
    ) -> Result<PruneStats, HistoryError> {
        let secs = i64::try_from(retention.as_secs()).unwrap_or(i64::MAX);
        self.prune(now.saturating_sub(secs)).await
    }

    /// Resolves the path-table id of `path`, preferring the longest recorded root that
    /// contains it.
    async fn path_id(&self, path: &Path) -> Result<Option<i64>, HistoryError> {
        let rows = self
            .db
            .select("paths")
            .where_eq("parent_id", 0_i64)
            .execute(&*self.db)
            .await?;
        let mut roots: Vec<(i64, PathBuf)> = rows
            .iter()
            .map(|r| {
                Ok((
                    int(r, "id")?,
                    PathBuf::from(bytes_to_os(&decode_name(&text(r, "name")?))),
                ))
            })
            .collect::<Result<_, HistoryError>>()?;
        roots.sort_by_key(|(_, p)| std::cmp::Reverse(p.components().count()));
        'roots: for (root_id, root) in roots {
            let Ok(rel) = path.strip_prefix(&root) else {
                continue;
            };
            let mut id = root_id;
            for comp in rel.components() {
                let Component::Normal(name) = comp else {
                    continue 'roots;
                };
                match path_lookup(&*self.db, id, &encode_name(&os_to_bytes(name))).await? {
                    Some(next) => id = next,
                    None => continue 'roots,
                }
            }
            return Ok(Some(id));
        }
        Ok(None)
    }

    /// Rebuilds the full path for a path-table id.
    async fn path_of(
        &self,
        id: i64,
        cache: &mut BTreeMap<i64, PathBuf>,
    ) -> Result<PathBuf, HistoryError> {
        // Walk up to the root, then build downwards.
        let mut chain = Vec::new();
        let mut cur = id;
        let mut base = PathBuf::new();
        while cur != 0 {
            if let Some(p) = cache.get(&cur) {
                base.clone_from(p);
                break;
            }
            let row = self
                .db
                .select("paths")
                .where_eq("id", cur)
                .execute_first(&*self.db)
                .await?
                .ok_or_else(|| HistoryError::InvalidData(format!("dangling path id {cur}")))?;
            chain.push((cur, bytes_to_os(&decode_name(&text(&row, "name")?))));
            cur = int(&row, "parent_id")?;
        }
        for (pid, name) in chain.into_iter().rev() {
            base = if base.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                base.join(name)
            };
            cache.insert(pid, base.clone());
        }
        Ok(base)
    }

    /// `rollup id -> at` for the given ids.
    async fn rollup_times(&self, ids: &[i64]) -> Result<BTreeMap<i64, i64>, HistoryError> {
        let mut out = BTreeMap::new();
        for chunk in ids.chunks(IN_CHUNK) {
            let rows = self
                .db
                .select("rollups")
                .columns(&["id", "at"])
                .filter(Box::new(where_in("id", chunk.to_vec())))
                .execute(&*self.db)
                .await?;
            for row in &rows {
                out.insert(int(row, "id")?, int(row, "at")?);
            }
        }
        Ok(out)
    }

    /// `path id -> (allocated, apparent)` for one rollup.
    async fn rollup_sizes(
        &self,
        rollup_id: i64,
    ) -> Result<BTreeMap<i64, (u64, u64)>, HistoryError> {
        let rows = self
            .db
            .select("rollup_sizes")
            .where_eq("rollup_id", rollup_id)
            .execute(&*self.db)
            .await?;
        rows.iter()
            .map(|r| {
                Ok((
                    int(r, "path_id")?,
                    (
                        from_sql(int(r, "allocated")?),
                        from_sql(int(r, "apparent")?),
                    ),
                ))
            })
            .collect()
    }

    /// Ids of all strict descendants of a path-table id.
    async fn descendants(&self, id: i64) -> Result<BTreeSet<i64>, HistoryError> {
        let mut out = BTreeSet::new();
        let mut frontier = vec![id];
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for chunk in frontier.chunks(IN_CHUNK) {
                let rows = self
                    .db
                    .select("paths")
                    .columns(&["id"])
                    .filter(Box::new(where_in("parent_id", chunk.to_vec())))
                    .execute(&*self.db)
                    .await?;
                for row in &rows {
                    let child = int(row, "id")?;
                    if out.insert(child) {
                        next.push(child);
                    }
                }
            }
            frontier = next;
        }
        Ok(out)
    }
}

fn open_connection(
    path: &Path,
) -> Result<switchy_database::rusqlite::RusqliteDatabase, HistoryError> {
    let options = SqliteOpenOptions {
        access: SqliteAccess::ReadWriteCreate,
        allow_uri: false,
        busy_timeout: Duration::from_secs(5),
        foreign_keys: true,
        connections: std::num::NonZeroUsize::MIN,
    };
    Ok(open_sqlite_rusqlite(path, &options)?)
}

async fn schema_version(db: &dyn Database) -> Result<i64, HistoryError> {
    if !db.table_exists("history_schema").await? {
        return Ok(0);
    }
    let row = db
        .select("history_schema")
        .where_eq("contract_id", 1_i64)
        .execute_first(db)
        .await?;
    row.map_or(Ok(0), |r| int(&r, "schema_version"))
}

/// Applies pending migrations, each in its own transaction.
async fn migrate(db: &dyn Database) -> Result<(), HistoryError> {
    let version = schema_version(db).await?;
    if version > SCHEMA_VERSION {
        return Err(HistoryError::FutureSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version < 1 {
        let tx = db.begin_transaction().await?;
        install_v1(&*tx).await?;
        tx.insert("history_schema")
            .value("contract_id", 1_i64)
            .value("schema_version", 1_i64)
            .execute(&*tx)
            .await?;
        tx.commit().await?;
        log::debug!("history schema created at v1");
    }
    // Future migrations: `if version < 2 { ...; update history_schema.schema_version = 2 }`.
    Ok(())
}

fn column(
    name: &str,
    data_type: DataType,
    nullable: bool,
    default: Option<DatabaseValue>,
) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default,
    }
}

fn id_column() -> Column {
    Column {
        name: "id".into(),
        data_type: DataType::BigInt,
        nullable: false,
        auto_increment: true,
        default: None,
    }
}

async fn install_v1(db: &dyn Database) -> Result<(), HistoryError> {
    create_table("history_schema")
        .if_not_exists(true)
        .column(column("contract_id", DataType::Int, false, None))
        .primary_key("contract_id")
        .check_integer_equals("contract_id", 1)
        .column(column("schema_version", DataType::Int, false, None))
        .execute(db)
        .await?;
    create_table("volumes")
        .if_not_exists(true)
        .column(id_column())
        .primary_key("id")
        .column(column("mount_point", DataType::Text, false, None))
        .column(column(
            "fs_type",
            DataType::Text,
            false,
            Some(DatabaseValue::String(String::new())),
        ))
        .unique_columns(["mount_point"])
        .execute(db)
        .await?;
    create_table("volume_samples")
        .if_not_exists(true)
        .column(column("volume_id", DataType::BigInt, false, None))
        .column(column("at", DataType::BigInt, false, None))
        .column(column("total", DataType::BigInt, false, None))
        .column(column("free", DataType::BigInt, false, None))
        .column(column("available", DataType::BigInt, false, None))
        .column(column("purgeable", DataType::BigInt, true, None))
        .primary_key_columns(["volume_id", "at"])
        .execute(db)
        .await?;
    create_index("volume_samples_at")
        .if_not_exists(true)
        .table("volume_samples")
        .column("at")
        .execute(db)
        .await?;
    create_table("paths")
        .if_not_exists(true)
        .column(id_column())
        .primary_key("id")
        .column(column("parent_id", DataType::BigInt, false, None))
        .column(column("name", DataType::Text, false, None))
        .unique_columns(["parent_id", "name"])
        .execute(db)
        .await?;
    create_table("rollups")
        .if_not_exists(true)
        .column(id_column())
        .primary_key("id")
        .column(column("root_id", DataType::BigInt, false, None))
        .column(column("at", DataType::BigInt, false, None))
        .column(column("depth", DataType::BigInt, false, None))
        .execute(db)
        .await?;
    create_index("rollups_root_at")
        .if_not_exists(true)
        .table("rollups")
        .columns(vec!["root_id", "at"])
        .execute(db)
        .await?;
    create_index("rollups_at")
        .if_not_exists(true)
        .table("rollups")
        .column("at")
        .execute(db)
        .await?;
    create_table("rollup_sizes")
        .if_not_exists(true)
        .column(column("rollup_id", DataType::BigInt, false, None))
        .column(column("path_id", DataType::BigInt, false, None))
        .column(column("allocated", DataType::BigInt, false, None))
        .column(column("apparent", DataType::BigInt, false, None))
        .column(column("items", DataType::BigInt, false, None))
        .primary_key_columns(["rollup_id", "path_id"])
        .execute(db)
        .await?;
    create_index("rollup_sizes_path")
        .if_not_exists(true)
        .table("rollup_sizes")
        .column("path_id")
        .execute(db)
        .await?;
    Ok(())
}

async fn path_lookup(
    db: &dyn Database,
    parent: i64,
    name: &str,
) -> Result<Option<i64>, HistoryError> {
    let row = db
        .select("paths")
        .columns(&["id"])
        .where_eq("parent_id", parent)
        .where_eq("name", name)
        .execute_first(db)
        .await?;
    row.map(|r| int(&r, "id")).transpose()
}

async fn path_get_or_insert(
    db: &dyn Database,
    parent: i64,
    name: &str,
) -> Result<i64, HistoryError> {
    if let Some(id) = path_lookup(db, parent, name).await? {
        return Ok(id);
    }
    let row = db
        .insert("paths")
        .value("parent_id", parent)
        .value("name", name)
        .execute(db)
        .await?;
    int(&row, "id")
}

/// Inserts one rollup's size rows as a single multi-row upsert keyed by `(rollup_id, path_id)`.
async fn insert_sizes(
    db: &dyn Database,
    rollup_id: i64,
    rows: &[[i64; 4]],
) -> Result<(), HistoryError> {
    if rows.is_empty() {
        return Ok(());
    }
    let values = rows
        .iter()
        .map(|[path_id, allocated, apparent, items]| {
            vec![
                ("rollup_id", DatabaseValue::Int64(rollup_id)),
                ("path_id", DatabaseValue::Int64(*path_id)),
                ("allocated", DatabaseValue::Int64(*allocated)),
                ("apparent", DatabaseValue::Int64(*apparent)),
                ("items", DatabaseValue::Int64(*items)),
            ]
        })
        .collect();
    db.upsert_multi("rollup_sizes")
        .unique(vec![
            Box::new(identifier("rollup_id")),
            Box::new(identifier("path_id")),
        ])
        .values(values)
        .execute(db)
        .await?;
    Ok(())
}

fn volume_stats(row: &Row, mount_point: &str, fs_type: &str) -> Result<VolumeStats, HistoryError> {
    Ok(VolumeStats {
        mount_point: mount_point.to_string(),
        fs_type: fs_type.to_string(),
        at: int(row, "at")?,
        total: from_sql(int(row, "total")?),
        free: from_sql(int(row, "free")?),
        available: from_sql(int(row, "available")?),
        purgeable: opt_int(row, "purgeable")?.map(from_sql),
    })
}

fn opt_int(row: &Row, column: &str) -> Result<Option<i64>, HistoryError> {
    match row.get(column) {
        Some(DatabaseValue::Null | DatabaseValue::Int64Opt(None)) => Ok(None),
        Some(DatabaseValue::Int64(v) | DatabaseValue::Int64Opt(Some(v))) => Ok(Some(v)),
        Some(DatabaseValue::Int32(v)) => Ok(Some(i64::from(v))),
        Some(DatabaseValue::UInt64(v)) => Ok(Some(to_sql(v))),
        _ => Err(HistoryError::InvalidData(format!(
            "column {column} is not an integer"
        ))),
    }
}

fn int(row: &Row, column: &str) -> Result<i64, HistoryError> {
    opt_int(row, column)?
        .ok_or_else(|| HistoryError::InvalidData(format!("column {column} is null")))
}

fn text(row: &Row, column: &str) -> Result<String, HistoryError> {
    match row.get(column) {
        Some(DatabaseValue::String(v) | DatabaseValue::StringOpt(Some(v))) => Ok(v),
        _ => Err(HistoryError::InvalidData(format!(
            "column {column} is not text"
        ))),
    }
}

fn count(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn to_sql(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn from_sql(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Encodes name bytes as TEXT: UTF-8 verbatim, `\` as `\\`, other bytes as `\xHH`.
fn encode_name(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '\\' {
                out.push_str("\\\\");
            } else {
                out.push(c);
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02X}");
        }
    }
    out
}

/// Inverse of [`encode_name`].
fn decode_name(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if bytes.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'x')
                && let Some(v) = s
                    .get(i + 2..i + 4)
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_tree::{EntryMeta, SizeMode};
    use std::ffi::OsStr;

    fn vol(at: i64, available: u64) -> VolumeStats {
        VolumeStats {
            mount_point: "/".into(),
            fs_type: "apfs".into(),
            total: 1000,
            free: available + 10,
            available,
            purgeable: Some(5),
            at,
        }
    }

    fn file(size: u64) -> EntryMeta {
        EntryMeta::new(EntryKind::File, size, size, 0)
    }

    /// `/r/{a/{deep/{deeper}}, b}` with configurable sizes.
    fn tree(a: u64, b: u64, deeper: u64) -> Tree {
        let mut t = Tree::new("/r", EntryMeta::dir());
        let da = t.add_child_raw(NodeId::ROOT, OsStr::new("a"), EntryMeta::dir());
        t.add_child_raw(da, OsStr::new("fa"), file(a));
        let deep = t.add_child_raw(da, OsStr::new("deep"), EntryMeta::dir());
        let deeper_dir = t.add_child_raw(deep, OsStr::new("deeper"), EntryMeta::dir());
        t.add_child_raw(deeper_dir, OsStr::new("f"), file(deeper));
        let db = t.add_child_raw(NodeId::ROOT, OsStr::new("b"), EntryMeta::dir());
        t.add_child_raw(db, OsStr::new("fb"), file(b));
        t.recompute_aggregates();
        t
    }

    #[test]
    fn name_encoding_is_lossless() {
        for case in [
            &b"plain"[..],
            b"back\\slash",
            b"\\x41",
            b"bad\xff\xfeutf8",
            "ü".as_bytes(),
            b"",
        ] {
            assert_eq!(decode_name(&encode_name(case)), case, "{case:?}");
        }
        assert_eq!(encode_name(b"plain"), "plain");
    }

    #[tokio::test]
    async fn migrations_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.sqlite3");
        {
            let h = History::open(&path).await.unwrap();
            assert_eq!(h.schema_version().await.unwrap(), SCHEMA_VERSION);
            h.record_volume(&vol(1, 100)).await.unwrap();
        }
        let h = History::open(&path).await.unwrap();
        assert_eq!(h.volume_series("/", 0).await.unwrap().len(), 1);
        h.database()
            .update("history_schema")
            .value("schema_version", SCHEMA_VERSION + 1)
            .where_eq("contract_id", 1_i64)
            .execute(h.database())
            .await
            .unwrap();
        drop(h);
        assert!(matches!(
            History::open(&path).await,
            Err(HistoryError::FutureSchema { .. })
        ));
    }

    #[tokio::test]
    async fn volume_series_and_time_to_full() {
        let h = History::open_in_memory().await.unwrap();
        h.record_volumes(&[vol(0, 1000), vol(100, 900), vol(200, 800)])
            .await
            .unwrap();
        h.record_volume(&VolumeStats {
            mount_point: "/other".into(),
            ..vol(50, 1)
        })
        .await
        .unwrap();
        // Overwrite of an existing (mount_point, at) sample.
        h.record_volume(&vol(200, 800)).await.unwrap();
        let series = h.volume_series("/", 100).await.unwrap();
        assert_eq!(
            series.iter().map(|s| s.at).collect::<Vec<_>>(),
            vec![100, 200]
        );
        assert_eq!(series[0], vol(100, 900));
        assert_eq!(
            h.volumes().await.unwrap(),
            vec!["/".to_string(), "/other".to_string()]
        );
        let latest = h.latest_volumes().await.unwrap();
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].at, 200);
        assert_eq!(h.volume_series("/missing", 0).await.unwrap().len(), 0);

        let ttf = h.time_to_full("/", 0).await.unwrap().unwrap();
        assert!((ttf.fill_rate - 1.0).abs() < 1e-9);
        assert!((ttf.seconds_to_full.unwrap() - 800.0).abs() < 1e-6);
        assert_eq!(ttf.full_at(), Some(1000));
        assert!(
            estimate_time_to_full(&[vol(0, 1), vol(10, 5)])
                .unwrap()
                .seconds_to_full
                .is_none()
        );
        assert!(estimate_time_to_full(&[vol(0, 1)]).is_none());
        assert!(estimate_time_to_full(&[vol(5, 1), vol(5, 2)]).is_none());
    }

    #[tokio::test]
    async fn rollups_series_growth_and_prune() {
        let h = History::open_in_memory().await.unwrap();
        let t1 = tree(10, 20, 5);
        h.record_rollup(&t1, 100, 2).await.unwrap();
        h.record_rollup(&tree(10, 25, 5), 200, 2).await.unwrap();
        h.record_rollup(&tree(110, 25, 1005), 300, 2).await.unwrap();

        let series = h.size_series(Path::new("/r/a"), 0).await.unwrap();
        assert_eq!(
            series.iter().map(|p| p.allocated).collect::<Vec<_>>(),
            vec![15, 15, 1115]
        );
        assert_eq!(
            series[0].items,
            t1.info(t1.find(Path::new("/r/a")).unwrap()).items
        );
        let root = h.size_series(Path::new("/r"), 150).await.unwrap();
        assert_eq!(root.len(), 2);
        assert_eq!(
            root[1].allocated,
            tree(110, 25, 1005).size(NodeId::ROOT, SizeMode::Allocated)
        );
        // depth 3 is beyond the recorded depth of 2
        assert_eq!(
            h.size_series(Path::new("/r/a/deep/deeper"), 0)
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            h.size_series(Path::new("/elsewhere"), 0)
                .await
                .unwrap()
                .len(),
            0
        );

        // Baseline nearest to 190 is the t=200 rollup.
        let growth = h.top_growth(Path::new("/r"), 190, 10).await.unwrap();
        let summary: Vec<_> = growth
            .iter()
            .map(|g| (g.path.clone(), g.delta_allocated()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (PathBuf::from("/r/a"), 1100),
                (PathBuf::from("/r/a/deep"), 1000)
            ]
        );
        assert_eq!((growth[0].from_at, growth[0].to_at), (200, 300));
        let growth = h.top_growth(Path::new("/r"), 0, 1).await.unwrap();
        assert_eq!(growth.len(), 1);
        assert_eq!(growth[0].from_at, 100);
        let sub = h.top_growth(Path::new("/r/a"), 0, 10).await.unwrap();
        assert_eq!(
            sub.iter().map(|g| g.path.clone()).collect::<Vec<_>>(),
            vec![PathBuf::from("/r/a/deep")]
        );

        h.record_volume(&vol(50, 1)).await.unwrap();
        let stats = h.prune(250).await.unwrap();
        assert_eq!(stats.rollups, 2);
        assert_eq!(stats.volume_samples, 1);
        assert_eq!(stats.paths, 0);
        assert_eq!(h.top_growth(Path::new("/r"), 0, 10).await.unwrap().len(), 0);
        let stats = h.prune_retention(Duration::ZERO, i64::MAX).await.unwrap();
        assert_eq!(stats.rollups, 1);
        assert_eq!(stats.paths, 4); // /r, /r/a, /r/a/deep, /r/b
        assert_eq!(h.size_series(Path::new("/r"), 0).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn large_rollup_batches_and_odd_names() {
        let h = History::open_in_memory().await.unwrap();
        let mut t = Tree::new("/big", EntryMeta::dir());
        for i in 0..(SIZE_BATCH + 10) {
            let d = t.add_child_raw(NodeId::ROOT, OsStr::new(&format!("d{i}")), EntryMeta::dir());
            t.add_child_raw(d, OsStr::new("f"), file(1));
        }
        t.add_child_raw(NodeId::ROOT, OsStr::new("we\\ird"), EntryMeta::dir());
        t.recompute_aggregates();
        h.record_rollup(&t, 1, 1).await.unwrap();
        assert_eq!(
            h.size_series(Path::new("/big/d7"), 0).await.unwrap()[0].allocated,
            1
        );
        #[cfg(unix)] // `\` is a path separator on Windows
        assert_eq!(
            h.size_series(Path::new("/big/we\\ird"), 0)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            h.size_series(Path::new("/big"), 0).await.unwrap()[0].allocated,
            u64::try_from(SIZE_BATCH + 10).unwrap()
        );
    }
}

#[cfg(test)]
mod send_tests {
    use super::*;

    fn assert_send<T: Send>(_: &T) {}

    /// History futures must be `Send` so the daemon can drive them on a multi-threaded runtime.
    #[tokio::test]
    async fn futures_are_send() {
        let h = History::open_in_memory().await.unwrap();
        let tree = Tree::new("/s", bclean_tree::EntryMeta::dir());
        let fut = h.record_rollup(&tree, 1, 1);
        assert_send(&fut);
        fut.await.unwrap();
        let fut = h.top_growth(Path::new("/s"), 0, 1);
        assert_send(&fut);
        fut.await.unwrap();
        let fut = h.prune(0);
        assert_send(&fut);
        fut.await.unwrap();
        assert_send(&History::open_in_memory());
    }
}
