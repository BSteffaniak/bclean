//! Cleanup plans: the serializable list of what an [`crate::Executor`] will do.

use std::path::{Path, PathBuf};

use bclean_model::{ActionSpec, Classification};
use serde::{Deserialize, Serialize};

/// The filesystem entry type observed when an item was planned.
///
/// The executor refuses to act if the entry's type changed since planning (for example a
/// directory that was replaced by a symlink).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EntryKind {
    /// Regular file.
    File,
    /// Directory.
    Dir,
    /// Symbolic link (or Windows junction / name-surrogate reparse point).
    Symlink,
    /// Anything else (device, socket, fifo).
    Other,
}

impl EntryKind {
    /// Reads the entry type of `path` without following symlinks.
    ///
    /// # Errors
    ///
    /// * The path does not exist or its metadata cannot be read.
    pub fn of(path: &Path) -> std::io::Result<Self> {
        let file_type = std::fs::symlink_metadata(path)?.file_type();
        Ok(if file_type.is_symlink() {
            Self::Symlink
        } else if file_type.is_dir() {
            Self::Dir
        } else if file_type.is_file() {
            Self::File
        } else {
            Self::Other
        })
    }
}

/// A classified filesystem entry offered for cleaning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Absolute path of the entry.
    pub path: PathBuf,
    /// Entry type at classification time, if known.
    pub kind: Option<EntryKind>,
    /// Classification produced by the rules engine.
    pub classification: Classification,
}

impl Candidate {
    /// Creates a candidate.
    #[must_use]
    pub fn new(
        path: impl Into<PathBuf>,
        kind: Option<EntryKind>,
        classification: Classification,
    ) -> Self {
        Self {
            path: path.into(),
            kind,
            classification,
        }
    }
}

/// One planned action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    /// Index of the item within its plan.
    pub id: u64,
    /// Target path.
    pub path: PathBuf,
    /// Expected entry type (checked again before acting).
    pub kind: Option<EntryKind>,
    /// Estimated reclaimable bytes.
    pub size: u64,
    /// What to do.
    pub action: ActionSpec,
    /// Rule that matched, if any.
    pub rule_id: Option<String>,
    /// Rule category, if any.
    pub category: Option<String>,
    /// Human-readable reason this item was selected.
    pub reason: String,
}

impl PlanItem {
    /// Builds a plan item from a candidate.
    #[must_use]
    pub fn from_candidate(id: u64, candidate: &Candidate) -> Self {
        let class = &candidate.classification;
        let reason = if class.reasons.is_empty() {
            class.rule_id.as_ref().map_or_else(
                || "selected".to_owned(),
                |rule| format!("matched rule {rule}"),
            )
        } else {
            class
                .reasons
                .iter()
                .map(|r| r.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        };
        Self {
            id,
            path: candidate.path.clone(),
            kind: candidate.kind,
            size: class.size,
            action: class.action.clone(),
            rule_id: class.rule_id.clone(),
            category: class.category.clone(),
            reason,
        }
    }
}

/// An ordered list of actions, serializable for IPC and dry-run output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Plan {
    /// Items in execution order.
    pub items: Vec<PlanItem>,
    /// Shadow plans are always executed as dry runs.
    #[serde(default)]
    pub shadow: bool,
    /// Policy that produced the plan, if any.
    #[serde(default)]
    pub policy_id: Option<String>,
    /// Creation time (Unix seconds).
    #[serde(default)]
    pub created_at: i64,
}

impl Plan {
    /// Builds a plan from candidates, preserving their order. Candidates whose action is
    /// `REPORT` are kept (the executor reports them without acting).
    #[must_use]
    pub fn from_candidates<'a>(candidates: impl IntoIterator<Item = &'a Candidate>) -> Self {
        let mut plan = Self {
            created_at: crate::now_unix(),
            ..Self::default()
        };
        for candidate in candidates {
            plan.push(candidate);
        }
        plan
    }

    /// Appends a candidate as the next item.
    pub fn push(&mut self, candidate: &Candidate) {
        let id = self.items.len() as u64;
        self.items.push(PlanItem::from_candidate(id, candidate));
    }

    /// Sum of estimated sizes of all items that will actually act (excludes `REPORT`).
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|item| !matches!(item.action, ActionSpec::Report))
            .map(|item| item.size)
            .fold(0, u64::saturating_add)
    }

    /// Number of items.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the plan has no items.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
