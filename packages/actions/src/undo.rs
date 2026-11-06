//! Reversing journaled actions.

use std::path::{Path, PathBuf};

use crate::archive::{extract, move_path};
use crate::journal::{
    Journal, JournalEntry, JournalError, JournalOutcome, PerformedAction, UndoInfo, new_entry_id,
};
use crate::now_unix;

/// Why an undo failed.
#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    /// No journal entry has that id.
    #[error("no journal entry with id {0}")]
    NotFound(String),
    /// The entry cannot be reversed.
    #[error("entry {id} is not reversible: {reason}")]
    NotReversible {
        /// Entry id.
        id: String,
        /// Explanation.
        reason: String,
    },
    /// The entry was already undone.
    #[error("entry {0} was already undone")]
    AlreadyUndone(String),
    /// Something already exists at the original location.
    #[error("cannot restore: {} already exists", .0.display())]
    DestinationExists(PathBuf),
    /// The saved copy (moved item, archive) is missing.
    #[error("cannot restore: {} no longer exists", .0.display())]
    SourceMissing(PathBuf),
    /// Restoring from the trash is not supported on this platform.
    #[error("restoring from the trash is not supported on this platform: {0}")]
    Unsupported(String),
    /// The item could not be found in the trash.
    #[error("{} was not found in the trash", .0.display())]
    NotInTrash(PathBuf),
    /// Filesystem or trash failure.
    #[error("undo failed: {0}")]
    Failed(String),
    /// The journal could not be read or written.
    #[error(transparent)]
    Journal(#[from] JournalError),
}

/// Reverses journal entry `entry_id` and appends an undo entry to the journal.
///
/// Equivalent to [`prepare_undo`], then [`perform_undo`], then [`Journal::append`] of the
/// returned record. The filesystem work runs inline on the current task; callers on an async
/// runtime that want it off the executor thread can call the three steps themselves and run
/// [`perform_undo`] on a blocking thread.
///
/// Supported: `MOVE` (moved back), `ARCHIVE` (extracted back, archive removed afterwards), and
/// `TRASH` (restored via the OS trash on Linux/freedesktop and Windows; macOS has no public
/// restore API, so it returns [`UndoError::Unsupported`]).
///
/// Returns the restored path.
///
/// # Errors
///
/// * Any error of [`prepare_undo`] or [`perform_undo`].
/// * [`UndoError::Journal`] if the undo record cannot be written (the item has then already
///   been restored).
pub async fn undo(journal: &Journal, entry_id: &str) -> Result<PathBuf, UndoError> {
    let entry = prepare_undo(journal, entry_id).await?;
    let record = perform_undo(&entry)?;
    journal.append(&record).await?;
    Ok(record.path)
}

/// Loads `entry_id` and checks that it can be reversed.
///
/// # Errors
///
/// * [`UndoError::NotFound`] if no entry has that id.
/// * [`UndoError::NotReversible`] if the entry did not complete or its action has no undo
///   (`DELETE`, `COMMAND`, `REPORT`, or an undo entry itself).
/// * [`UndoError::AlreadyUndone`] if a completed undo already references it.
/// * [`UndoError::Journal`] if the journal cannot be read.
pub async fn prepare_undo(journal: &Journal, entry_id: &str) -> Result<JournalEntry, UndoError> {
    let entry = journal
        .find(entry_id)
        .await?
        .ok_or_else(|| UndoError::NotFound(entry_id.to_owned()))?;
    let not_reversible = |reason: &str| UndoError::NotReversible {
        id: entry_id.to_owned(),
        reason: reason.to_owned(),
    };
    if entry.outcome != JournalOutcome::Done {
        return Err(not_reversible("the action did not complete"));
    }
    if journal.is_undone(entry_id).await? {
        return Err(UndoError::AlreadyUndone(entry_id.to_owned()));
    }
    if entry.undo.is_none() {
        let reason = match entry.performed {
            Some(PerformedAction::Delete) => "deleted items cannot be restored",
            Some(PerformedAction::Command) => "tool commands cannot be reversed",
            Some(PerformedAction::Undo) => "undo entries cannot be reversed",
            _ => "no undo information was recorded",
        };
        return Err(not_reversible(reason));
    }
    Ok(entry)
}

/// Performs the filesystem part of reversing `entry` (synchronous, blocking) and returns the
/// undo record to append to the journal.
///
/// # Errors
///
/// * [`UndoError::NotReversible`] if the entry has no undo information.
/// * [`UndoError::DestinationExists`] if the original location is occupied.
/// * [`UndoError::SourceMissing`] if the moved item or archive is gone.
/// * [`UndoError::Unsupported`] / [`UndoError::NotInTrash`] for trash restores that cannot be
///   performed.
/// * [`UndoError::Failed`] on filesystem errors.
pub fn perform_undo(entry: &JournalEntry) -> Result<JournalEntry, UndoError> {
    let Some(info) = &entry.undo else {
        return Err(UndoError::NotReversible {
            id: entry.id.clone(),
            reason: "no undo information was recorded".to_owned(),
        });
    };
    let restored = match info {
        UndoInfo::Move { from, to } => {
            ensure_free(from)?;
            ensure_exists(to)?;
            move_path(to, from).map_err(|e| UndoError::Failed(e.to_string()))?;
            from.clone()
        }
        UndoInfo::Archive { original, archive } => {
            ensure_free(original)?;
            ensure_exists(archive)?;
            let (parent, name) = split(original)?;
            extract(archive, parent, name).map_err(|e| UndoError::Failed(e.to_string()))?;
            if let Err(err) = std::fs::remove_file(archive) {
                log::warn!("restored but could not remove {}: {err}", archive.display());
            }
            original.clone()
        }
        UndoInfo::Trash {
            original,
            trashed_at,
        } => {
            ensure_free(original)?;
            restore_from_trash(original, *trashed_at)?;
            original.clone()
        }
    };
    Ok(JournalEntry {
        id: new_entry_id(),
        at: now_unix(),
        plan_item_id: None,
        policy_id: entry.policy_id.clone(),
        path: restored,
        action: entry.action.clone(),
        performed: Some(PerformedAction::Undo),
        fallback_used: false,
        size: entry.size,
        measured_freed: None,
        outcome: JournalOutcome::Done,
        detail: Some(format!("undid {}", entry.id)),
        undo: None,
        undoes: Some(entry.id.clone()),
    })
}

fn ensure_free(path: &Path) -> Result<(), UndoError> {
    if std::fs::symlink_metadata(path).is_ok() {
        Err(UndoError::DestinationExists(path.to_path_buf()))
    } else {
        Ok(())
    }
}

fn ensure_exists(path: &Path) -> Result<(), UndoError> {
    if std::fs::symlink_metadata(path).is_ok() {
        Ok(())
    } else {
        Err(UndoError::SourceMissing(path.to_path_buf()))
    }
}

fn split(path: &Path) -> Result<(&Path, &Path), UndoError> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => Ok((parent, Path::new(name))),
        _ => Err(UndoError::Failed(format!(
            "{} has no parent or name",
            path.display()
        ))),
    }
}

#[cfg(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
))]
fn restore_from_trash(original: &Path, trashed_at: i64) -> Result<(), UndoError> {
    let items = trash::os_limited::list().map_err(|e| UndoError::Failed(e.to_string()))?;
    // Prefer the item trashed closest to (and not long before) the recorded time.
    let best = items
        .into_iter()
        .filter(|item| item.original_path() == original)
        .min_by_key(|item| (item.time_deleted - trashed_at).abs())
        .ok_or_else(|| UndoError::NotInTrash(original.to_path_buf()))?;
    trash::os_limited::restore_all([best]).map_err(|e| UndoError::Failed(e.to_string()))
}

#[cfg(not(any(
    target_os = "windows",
    all(
        unix,
        not(target_os = "macos"),
        not(target_os = "ios"),
        not(target_os = "android")
    )
)))]
fn restore_from_trash(original: &Path, _trashed_at: i64) -> Result<(), UndoError> {
    Err(UndoError::Unsupported(format!(
        "use Finder's \"Put Back\" to restore {}",
        original.display()
    )))
}
