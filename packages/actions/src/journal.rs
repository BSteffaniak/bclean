//! SQLite action journal (via `switchy_database`).
//!
//! Every non-dry-run item the executor attempts is recorded as one row. Undo operations are
//! appended as new rows referencing the entry they reverse (`undoes`), so rows are never
//! modified. Each append is its own transaction, so a crash can never leave a torn entry.
//!
//! # Schema (v1)
//!
//! * `journal_schema(contract_id = 1, schema_version)`: the schema contract row.
//! * `journal_entries(seq, id UNIQUE, at, plan_item_id, policy_id, path, action, performed,
//!   fallback_used, size, measured_freed, outcome, detail, undo, undoes)`, ordered by the
//!   auto-increment `seq`.
//!
//! Structured fields (`action`, `undo`) are JSON-encoded TEXT, because typed `switchy_database`
//! queries do not return BLOB columns. Paths (including those inside `undo`) are stored as TEXT
//! with a lossless escape: valid UTF-8 verbatim, `\` as `\\`, other bytes as `\xHH` (lossless on
//! Unix; other platforms store UTF-8). Sizes saturate at `i64::MAX`.
//!
//! [`Journal::open`] applies pending migrations inside a transaction and refuses databases
//! written by a newer version ([`JournalError::FutureSchema`]).

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bclean_model::ActionSpec;
use serde::{Deserialize, Serialize};
use switchy_database::query::{FilterableQuery as _, SortDirection};
use switchy_database::schema::{Column, DataType, create_index, create_table};
use switchy_database::{Database, DatabaseValue, Row, SqliteJournalMode};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};
/// What was actually done to an item (may differ from the planned action when a fallback ran).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PerformedAction {
    /// Permanently deleted.
    Delete,
    /// Moved to the OS trash.
    Trash,
    /// Ran the tool command.
    Command,
    /// Archived, then deleted.
    Archive,
    /// Moved elsewhere.
    Move,
    /// Reversed an earlier entry.
    Undo,
}

/// Final state of a journaled item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JournalOutcome {
    /// Completed.
    Done,
    /// Refused before acting (protected, busy, missing, changed type).
    Skipped,
    /// Attempted and failed.
    Failed,
    /// Stopped by cancellation.
    Cancelled,
}

/// Information needed to reverse an action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UndoInfo {
    /// Item was moved to the OS trash.
    Trash {
        /// Original location.
        original: PathBuf,
        /// Time it was trashed (Unix seconds).
        trashed_at: i64,
    },
    /// Item was moved.
    Move {
        /// Original location.
        from: PathBuf,
        /// New location.
        to: PathBuf,
    },
    /// Item was archived and then deleted.
    Archive {
        /// Original location.
        original: PathBuf,
        /// Archive file.
        archive: PathBuf,
    },
}

/// One journal line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Unique entry id.
    pub id: String,
    /// Time recorded (Unix seconds).
    pub at: i64,
    /// Plan item id, if the entry came from a plan.
    #[serde(default)]
    pub plan_item_id: Option<u64>,
    /// Policy that produced the plan, if any.
    #[serde(default)]
    pub policy_id: Option<String>,
    /// Target path.
    pub path: PathBuf,
    /// Planned action.
    pub action: ActionSpec,
    /// What was actually performed.
    #[serde(default)]
    pub performed: Option<PerformedAction>,
    /// A command's fallback action was used.
    #[serde(default)]
    pub fallback_used: bool,
    /// Estimated size in bytes.
    pub size: u64,
    /// Measured change in volume free space (bytes; may be negative or noisy).
    #[serde(default)]
    pub measured_freed: Option<i64>,
    /// Outcome.
    pub outcome: JournalOutcome,
    /// Human-readable detail (error, skip reason).
    #[serde(default)]
    pub detail: Option<String>,
    /// How to reverse this entry, if reversible.
    #[serde(default)]
    pub undo: Option<UndoInfo>,
    /// For undo entries: the id of the entry that was reversed.
    #[serde(default)]
    pub undoes: Option<String>,
}

/// Generates a unique, roughly time-ordered entry id.
#[must_use]
pub fn new_entry_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{:x}-{seq:x}", std::process::id())
}

/// Latest journal schema version understood by this build.
pub const SCHEMA_VERSION: i64 = 1;

const ENTRIES: &str = "journal_entries";
const CONTRACT: &str = "journal_schema";

/// Error reading or writing the journal.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Opening or configuring the SQLite connection failed.
    #[error("cannot open journal database: {0}")]
    Open(#[from] switchy_database_connection::InitSqliteRusqliteError),
    /// Creating the journal's directory failed.
    #[error("cannot create journal directory {}: {source}", path.display())]
    CreateDir {
        /// Directory path.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A database operation failed.
    #[error("journal database error: {0}")]
    Database(#[from] switchy_database::DatabaseError),
    /// JSON encoding of a structured field failed.
    #[error("journal entry serialization: {0}")]
    Serde(#[from] serde_json::Error),
    /// The database was written by a newer bclean.
    #[error("journal schema version {found} is newer than supported version {supported}")]
    FutureSchema {
        /// Version found in the file.
        found: i64,
        /// Newest version this build supports.
        supported: i64,
    },
    /// A row did not have the expected shape.
    #[error("invalid journal data: {0}")]
    InvalidData(String),
}

/// Handle to the SQLite action journal.
#[derive(Debug)]
pub struct Journal {
    db: Box<dyn Database>,
}

impl Journal {
    /// Opens (creating the file and its parent directory if needed) the journal database at
    /// `path` (conventionally `journal.sqlite3`), enables WAL, and applies migrations.
    ///
    /// # Errors
    ///
    /// * [`JournalError::CreateDir`] if the parent directory cannot be created.
    /// * [`JournalError::Open`] if the file cannot be opened or configured.
    /// * [`JournalError::Database`] if enabling WAL or a migration fails.
    /// * [`JournalError::FutureSchema`] if the database was written by a newer version.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|source| JournalError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let db = open_connection(path)?;
        db.sqlite_journal_mode(SqliteJournalMode::Wal).await?;
        Self::from_database(Box::new(db)).await
    }

    /// Opens a private in-memory journal (tests, dry runs).
    ///
    /// # Errors
    ///
    /// * [`JournalError::Open`] if SQLite fails to initialize.
    /// * [`JournalError::Database`] if creating the schema fails.
    pub async fn open_in_memory() -> Result<Self, JournalError> {
        Self::from_database(Box::new(open_connection(Path::new(":memory:"))?)).await
    }

    /// Wraps an existing connection (for example one shared by the daemon) and applies
    /// migrations. The journal tables can live alongside other tables in the same database.
    ///
    /// # Errors
    ///
    /// * [`JournalError::Database`] if a migration fails.
    /// * [`JournalError::FutureSchema`] if the database was written by a newer version.
    pub async fn from_database(db: Box<dyn Database>) -> Result<Self, JournalError> {
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
    /// * [`JournalError::Database`] if the contract row cannot be read.
    pub async fn schema_version(&self) -> Result<i64, JournalError> {
        schema_version(&*self.db).await
    }

    /// Appends one entry in its own transaction.
    ///
    /// # Errors
    ///
    /// * [`JournalError::Serde`] if a structured field cannot be encoded.
    /// * [`JournalError::Database`] if the write fails (including a duplicate id).
    pub async fn append(&self, entry: &JournalEntry) -> Result<(), JournalError> {
        self.append_all(std::slice::from_ref(entry)).await
    }

    /// Appends entries in order, all in one transaction (all or nothing). Rows are inserted
    /// one by one.
    ///
    /// # Errors
    ///
    /// * [`JournalError::Serde`] if a structured field cannot be encoded.
    /// * [`JournalError::Database`] if a write fails; nothing is written then.
    pub async fn append_all(&self, entries: &[JournalEntry]) -> Result<(), JournalError> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self.db.begin_transaction().await?;
        for entry in entries {
            insert_entry(&*tx, entry).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Persists every journal record produced by an execution (see
    /// [`crate::ExecutionReport::journal_entries`]) in one transaction.
    ///
    /// # Errors
    ///
    /// * As for [`Journal::append_all`].
    pub async fn record(&self, report: &crate::ExecutionReport) -> Result<(), JournalError> {
        let entries: Vec<JournalEntry> = report.journal_entries().cloned().collect();
        self.append_all(&entries).await
    }

    /// All entries in append order.
    ///
    /// # Errors
    ///
    /// * [`JournalError::Database`] if the query fails.
    /// * [`JournalError::InvalidData`] / [`JournalError::Serde`] if a row is malformed.
    pub async fn entries(&self) -> Result<Vec<JournalEntry>, JournalError> {
        let rows = self
            .db
            .select(ENTRIES)
            .sort("seq", SortDirection::Asc)
            .execute(&*self.db)
            .await?;
        rows.iter().map(entry_from_row).collect()
    }

    /// Finds an entry by id.
    ///
    /// # Errors
    ///
    /// * [`JournalError::Database`] if the query fails.
    /// * [`JournalError::InvalidData`] / [`JournalError::Serde`] if the row is malformed.
    pub async fn find(&self, id: &str) -> Result<Option<JournalEntry>, JournalError> {
        let row = self
            .db
            .select(ENTRIES)
            .where_eq("id", id)
            .execute_first(&*self.db)
            .await?;
        row.as_ref().map(entry_from_row).transpose()
    }

    /// Whether a completed undo entry exists for `id`.
    ///
    /// # Errors
    ///
    /// * [`JournalError::Database`] if the query fails.
    pub async fn is_undone(&self, id: &str) -> Result<bool, JournalError> {
        let row = self
            .db
            .select(ENTRIES)
            .columns(&["seq"])
            .where_eq("undoes", id)
            .where_eq("outcome", outcome_str(JournalOutcome::Done))
            .limit(1)
            .execute_first(&*self.db)
            .await?;
        Ok(row.is_some())
    }
}

fn open_connection(
    path: &Path,
) -> Result<switchy_database::rusqlite::RusqliteDatabase, JournalError> {
    let options = SqliteOpenOptions {
        access: SqliteAccess::ReadWriteCreate,
        allow_uri: false,
        busy_timeout: Duration::from_secs(5),
        foreign_keys: true,
        connections: std::num::NonZeroUsize::MIN,
    };
    Ok(open_sqlite_rusqlite(path, &options)?)
}

async fn schema_version(db: &dyn Database) -> Result<i64, JournalError> {
    if !db.table_exists(CONTRACT).await? {
        return Ok(0);
    }
    let row = db
        .select(CONTRACT)
        .where_eq("contract_id", 1_i64)
        .execute_first(db)
        .await?;
    row.map_or(Ok(0), |r| int(&r, "schema_version"))
}

/// Applies pending migrations, each in its own transaction.
async fn migrate(db: &dyn Database) -> Result<(), JournalError> {
    let version = schema_version(db).await?;
    if version > SCHEMA_VERSION {
        return Err(JournalError::FutureSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version < 1 {
        let tx = db.begin_transaction().await?;
        install_v1(&*tx).await?;
        tx.insert(CONTRACT)
            .value("contract_id", 1_i64)
            .value("schema_version", 1_i64)
            .execute(&*tx)
            .await?;
        tx.commit().await?;
        log::debug!("journal schema created at v1");
    }
    // Future migrations: `if version < 2 { ...; update journal_schema.schema_version = 2 }`.
    Ok(())
}

fn column(name: &str, data_type: DataType, nullable: bool) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default: None,
    }
}

async fn install_v1(db: &dyn Database) -> Result<(), JournalError> {
    create_table(CONTRACT)
        .if_not_exists(true)
        .column(column("contract_id", DataType::Int, false))
        .primary_key("contract_id")
        .check_integer_equals("contract_id", 1)
        .column(column("schema_version", DataType::Int, false))
        .execute(db)
        .await?;
    create_table(ENTRIES)
        .if_not_exists(true)
        .column(Column {
            name: "seq".into(),
            data_type: DataType::BigInt,
            nullable: false,
            auto_increment: true,
            default: None,
        })
        .primary_key("seq")
        .column(column("id", DataType::Text, false))
        .column(column("at", DataType::BigInt, false))
        .column(column("plan_item_id", DataType::BigInt, true))
        .column(column("policy_id", DataType::Text, true))
        .column(column("path", DataType::Text, false))
        .column(column("action", DataType::Text, false))
        .column(column("performed", DataType::Text, true))
        .column(column("fallback_used", DataType::BigInt, false))
        .column(column("size", DataType::BigInt, false))
        .column(column("measured_freed", DataType::BigInt, true))
        .column(column("outcome", DataType::Text, false))
        .column(column("detail", DataType::Text, true))
        .column(column("undo", DataType::Text, true))
        .column(column("undoes", DataType::Text, true))
        .unique_columns(["id"])
        .execute(db)
        .await?;
    create_index("journal_entries_undoes")
        .if_not_exists(true)
        .table(ENTRIES)
        .column("undoes")
        .execute(db)
        .await?;
    Ok(())
}

/// Storage form of [`UndoInfo`] with paths escaped as TEXT.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
enum StoredUndo {
    Trash { original: String, trashed_at: i64 },
    Move { from: String, to: String },
    Archive { original: String, archive: String },
}

impl From<&UndoInfo> for StoredUndo {
    fn from(info: &UndoInfo) -> Self {
        match info {
            UndoInfo::Trash {
                original,
                trashed_at,
            } => Self::Trash {
                original: encode_path(original),
                trashed_at: *trashed_at,
            },
            UndoInfo::Move { from, to } => Self::Move {
                from: encode_path(from),
                to: encode_path(to),
            },
            UndoInfo::Archive { original, archive } => Self::Archive {
                original: encode_path(original),
                archive: encode_path(archive),
            },
        }
    }
}

impl From<StoredUndo> for UndoInfo {
    fn from(stored: StoredUndo) -> Self {
        match stored {
            StoredUndo::Trash {
                original,
                trashed_at,
            } => Self::Trash {
                original: decode_path(&original),
                trashed_at,
            },
            StoredUndo::Move { from, to } => Self::Move {
                from: decode_path(&from),
                to: decode_path(&to),
            },
            StoredUndo::Archive { original, archive } => Self::Archive {
                original: decode_path(&original),
                archive: decode_path(&archive),
            },
        }
    }
}

/// Serializes a unit-variant enum to its bare serde name (`"DONE"`).
fn enum_str<T: Serialize>(value: &T) -> Result<String, JournalError> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => Err(JournalError::InvalidData(format!(
            "expected a string enum, got {other}"
        ))),
    }
}

fn enum_from<T: for<'de> Deserialize<'de>>(s: &str) -> Result<T, JournalError> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_owned(),
    ))?)
}

const fn outcome_str(outcome: JournalOutcome) -> &'static str {
    match outcome {
        JournalOutcome::Done => "DONE",
        JournalOutcome::Skipped => "SKIPPED",
        JournalOutcome::Failed => "FAILED",
        JournalOutcome::Cancelled => "CANCELLED",
    }
}

async fn insert_entry(db: &dyn Database, entry: &JournalEntry) -> Result<(), JournalError> {
    let undo = entry
        .undo
        .as_ref()
        .map(|u| serde_json::to_string(&StoredUndo::from(u)))
        .transpose()?;
    let performed = entry.performed.as_ref().map(enum_str).transpose()?;
    db.insert(ENTRIES)
        .value("id", entry.id.as_str())
        .value("at", entry.at)
        .value("plan_item_id", entry.plan_item_id.map(to_sql))
        .value("policy_id", entry.policy_id.clone())
        .value("path", encode_path(&entry.path))
        .value("action", serde_json::to_string(&entry.action)?)
        .value("performed", performed)
        .value("fallback_used", i64::from(entry.fallback_used))
        .value("size", to_sql(entry.size))
        .value("measured_freed", entry.measured_freed)
        .value("outcome", outcome_str(entry.outcome))
        .value("detail", entry.detail.clone())
        .value("undo", undo)
        .value("undoes", entry.undoes.clone())
        .execute(db)
        .await?;
    Ok(())
}

fn entry_from_row(row: &Row) -> Result<JournalEntry, JournalError> {
    let undo = opt_text(row, "undo")?
        .map(|s| serde_json::from_str::<StoredUndo>(&s).map(UndoInfo::from))
        .transpose()?;
    Ok(JournalEntry {
        id: text(row, "id")?,
        at: int(row, "at")?,
        plan_item_id: opt_int(row, "plan_item_id")?.map(from_sql),
        policy_id: opt_text(row, "policy_id")?,
        path: decode_path(&text(row, "path")?),
        action: serde_json::from_str(&text(row, "action")?)?,
        performed: opt_text(row, "performed")?
            .map(|s| enum_from(&s))
            .transpose()?,
        fallback_used: int(row, "fallback_used")? != 0,
        size: from_sql(int(row, "size")?),
        measured_freed: opt_int(row, "measured_freed")?,
        outcome: enum_from(&text(row, "outcome")?)?,
        detail: opt_text(row, "detail")?,
        undo,
        undoes: opt_text(row, "undoes")?,
    })
}

fn opt_int(row: &Row, column: &str) -> Result<Option<i64>, JournalError> {
    match row.get(column) {
        Some(DatabaseValue::Null | DatabaseValue::Int64Opt(None)) | None => Ok(None),
        Some(DatabaseValue::Int64(v) | DatabaseValue::Int64Opt(Some(v))) => Ok(Some(v)),
        Some(DatabaseValue::Int32(v)) => Ok(Some(i64::from(v))),
        Some(DatabaseValue::UInt64(v)) => Ok(Some(i64::try_from(v).unwrap_or(i64::MAX))),
        _ => Err(JournalError::InvalidData(format!(
            "column {column} is not an integer"
        ))),
    }
}

fn int(row: &Row, column: &str) -> Result<i64, JournalError> {
    opt_int(row, column)?
        .ok_or_else(|| JournalError::InvalidData(format!("column {column} is null")))
}

fn opt_text(row: &Row, column: &str) -> Result<Option<String>, JournalError> {
    match row.get(column) {
        Some(DatabaseValue::Null | DatabaseValue::StringOpt(None)) | None => Ok(None),
        Some(DatabaseValue::String(v) | DatabaseValue::StringOpt(Some(v))) => Ok(Some(v)),
        _ => Err(JournalError::InvalidData(format!(
            "column {column} is not text"
        ))),
    }
}

fn text(row: &Row, column: &str) -> Result<String, JournalError> {
    opt_text(row, column)?
        .ok_or_else(|| JournalError::InvalidData(format!("column {column} is null")))
}

fn to_sql(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn from_sql(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

#[cfg(unix)]
fn os_to_bytes(s: &OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt as _;
    Cow::Borrowed(s.as_bytes())
}

#[cfg(unix)]
fn bytes_to_os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt as _;
    OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os_to_bytes(s: &OsStr) -> Cow<'_, [u8]> {
    match s.to_string_lossy() {
        Cow::Borrowed(s) => Cow::Borrowed(s.as_bytes()),
        Cow::Owned(s) => Cow::Owned(s.into_bytes()),
    }
}

#[cfg(not(unix))]
fn bytes_to_os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Encodes a path as TEXT: UTF-8 verbatim, `\` as `\\`, other bytes as `\xHH`.
fn encode_path(path: &Path) -> String {
    let bytes = os_to_bytes(path.as_os_str());
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

/// Inverse of [`encode_path`].
fn decode_path(s: &str) -> PathBuf {
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
    PathBuf::from(bytes_to_os(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: PathBuf) -> JournalEntry {
        JournalEntry {
            id: new_entry_id(),
            at: 1,
            plan_item_id: Some(0),
            policy_id: Some("low".to_owned()),
            path: path.clone(),
            action: ActionSpec::Move {
                dest: "/cold".to_owned(),
            },
            performed: Some(PerformedAction::Move),
            fallback_used: true,
            size: 10,
            measured_freed: Some(-8),
            outcome: JournalOutcome::Done,
            detail: Some("moved".to_owned()),
            undo: Some(UndoInfo::Move {
                from: path,
                to: PathBuf::from("/cold/x"),
            }),
            undoes: None,
        }
    }

    #[tokio::test]
    async fn append_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("sub/journal.sqlite3"))
            .await
            .unwrap();
        let first = entry(PathBuf::from("/a\\b/ü"));
        let mut second = entry(PathBuf::from("/c"));
        second.undo = None;
        second.performed = None;
        second.plan_item_id = None;
        second.measured_freed = None;
        second.outcome = JournalOutcome::Skipped;
        journal.append(&first).await.unwrap();
        journal.append(&second).await.unwrap();
        assert_eq!(
            journal.entries().await.unwrap(),
            vec![first.clone(), second]
        );
        assert_eq!(journal.find(&first.id).await.unwrap(), Some(first.clone()));
        assert_eq!(journal.find("missing").await.unwrap(), None);
        // Duplicate ids are rejected and the whole batch rolls back.
        let third = entry(PathBuf::from("/d"));
        assert!(
            journal
                .append_all(&[third.clone(), first.clone()])
                .await
                .is_err()
        );
        assert_eq!(journal.find(&third.id).await.unwrap(), None);
        assert!(!journal.is_undone(&first.id).await.unwrap());
        let mut undo = entry(PathBuf::from("/a"));
        undo.undoes = Some(first.id.clone());
        journal.append(&undo).await.unwrap();
        assert!(journal.is_undone(&first.id).await.unwrap());
    }

    #[tokio::test]
    async fn reopen_and_future_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.sqlite3");
        let e = entry(PathBuf::from("/x"));
        {
            let journal = Journal::open(&path).await.unwrap();
            assert_eq!(journal.schema_version().await.unwrap(), SCHEMA_VERSION);
            journal.append(&e).await.unwrap();
        }
        let journal = Journal::open(&path).await.unwrap();
        assert_eq!(journal.entries().await.unwrap(), vec![e]);
        journal
            .database()
            .update(CONTRACT)
            .value("schema_version", SCHEMA_VERSION + 1)
            .where_eq("contract_id", 1_i64)
            .execute(journal.database())
            .await
            .unwrap();
        drop(journal);
        assert!(matches!(
            Journal::open(&path).await,
            Err(JournalError::FutureSchema { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn path_encoding_is_lossless() {
        use std::os::unix::ffi::OsStrExt as _;
        for case in [&b"/plain"[..], b"/back\\slash", b"/\\x41", b"/bad\xff\xfe"] {
            let path = PathBuf::from(OsStr::from_bytes(case));
            assert_eq!(decode_path(&encode_path(&path)), path, "{case:?}");
        }
    }

    fn assert_send<T: Send>(_: &T) {}

    /// Journal futures must be `Send` so the daemon can drive them on a multi-threaded runtime.
    #[tokio::test]
    async fn futures_are_send() {
        let journal = Journal::open_in_memory().await.unwrap();
        let e = entry(PathBuf::from("/x"));
        let fut = journal.append(&e);
        assert_send(&fut);
        fut.await.unwrap();
        let fut = journal.entries();
        assert_send(&fut);
        fut.await.unwrap();
        assert_send(&Journal::open_in_memory());
    }
}
