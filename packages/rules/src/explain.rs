//! `bclean explain <path>`: classify one arbitrary path, without a scanned tree, and explain
//! every decision (including why no rule matched).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bclean_model::{ActionSpec, Classification, Confidence, Reason};
use bclean_staleness::Target;
use bclean_tree::EntryKind;

use crate::classify::{
    Classifier, assemble, list_names, match_reasons, outcome_reasons, policy_for,
};
use crate::matcher::{ChildEntry, Contents, Probe};
use crate::score::score;

/// Maximum entries visited when measuring a path's size.
const SIZE_WALK_LIMIT: usize = 2_000_000;

/// An enclosing item that already matched a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enclosing {
    /// Path of the matched ancestor.
    pub path: PathBuf,
    /// Rule that matched it.
    pub rule_id: String,
}

/// A rule that was considered but did not match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearMiss {
    /// Rule id.
    pub rule_id: String,
    /// Why it did not match.
    pub reason: String,
}

/// Full explanation of one path.
#[derive(Debug, Clone, PartialEq)]
pub struct Explanation {
    /// The explained path (absolute).
    pub path: PathBuf,
    /// Entry kind.
    pub kind: EntryKind,
    /// Classification (with `rule_id = None` and `NO_MATCH` reasons when nothing matched).
    pub classification: Classification,
    /// Nearest ancestor matched by a rule, if any (the path is part of that item).
    pub enclosing: Option<Enclosing>,
    /// Rules considered (by name or path prefilter) that did not match.
    pub near_misses: Vec<NearMiss>,
    /// Whether the size measurement hit its limit.
    pub size_truncated: bool,
}

fn entry_kind(meta: &std::fs::Metadata) -> EntryKind {
    let t = meta.file_type();
    if t.is_symlink() {
        EntryKind::Symlink
    } else if t.is_dir() {
        EntryKind::Dir
    } else if t.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

#[cfg(unix)]
fn on_disk(meta: &std::fs::Metadata, allocated: bool) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if allocated {
        meta.blocks().saturating_mul(512)
    } else {
        meta.len()
    }
}

#[cfg(not(unix))]
fn on_disk(meta: &std::fs::Metadata, _allocated: bool) -> u64 {
    meta.len()
}

/// Total size of `path` (not following symlinks), bounded.
pub fn measure(path: &Path, allocated: bool) -> (u64, bool) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return (0, false);
    };
    let mut total = on_disk(&meta, allocated);
    if !meta.is_dir() {
        return (total, false);
    }
    let mut stack = vec![path.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > SIZE_WALK_LIMIT {
                return (total, true);
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            total = total.saturating_add(on_disk(&meta, allocated));
            if meta.is_dir() && !meta.file_type().is_symlink() {
                stack.push(entry.path());
            }
        }
    }
    (total, false)
}

impl Classifier<'_> {
    /// Classifies and explains one arbitrary path, reading only the filesystem (no tree).
    ///
    /// Relative paths are resolved against the working directory; symlinks are not resolved.
    ///
    /// # Errors
    ///
    /// * The path does not exist or its metadata cannot be read
    /// * The working directory cannot be determined (for relative paths)
    pub fn explain(&self, path: &Path) -> std::io::Result<Explanation> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let meta = std::fs::symlink_metadata(&path)?;
        let kind = entry_kind(&meta);
        let rules = self.rules;

        let (dir, name) = match (path.parent(), path.file_name()) {
            (Some(d), Some(n)) => (d.to_path_buf(), n.to_os_string()),
            _ => (path.clone(), OsString::new()),
        };
        let listing = list_names(&dir);
        let entries: Vec<ChildEntry<'_>> = listing
            .iter()
            .map(|(n, k)| ChildEntry { name: n, kind: *k })
            .collect();
        let probe = Probe {
            dir: &dir,
            name: &name,
            kind,
            siblings: &entries,
            contents: Contents::Fs,
        };
        let heuristics = rules.classify.gitignored_heuristic;
        let result = if name.is_empty() {
            None
        } else {
            rules.match_entry(&probe, heuristics)
        };
        let near_misses: Vec<NearMiss> = if name.is_empty() {
            Vec::new()
        } else {
            rules
                .diagnose(&probe)
                .into_iter()
                .filter_map(|(i, outcome)| {
                    outcome.err().map(|miss| NearMiss {
                        rule_id: rules.rule(i).id().to_owned(),
                        reason: miss.to_string(),
                    })
                })
                .collect()
        };
        let enclosing = self.enclosing(&path);
        let allocated = matches!(rules.size_mode, bclean_model::SizeModeSpec::Allocated);
        let (size, size_truncated) = measure(&path, allocated);
        let target = Target::path(&path, kind == EntryKind::Dir);

        let mut classification = result.as_ref().map_or_else(
            || self.unmatched(&path, &target, size, &near_misses, enclosing.as_ref()),
            |result| {
                let rule = rules.rule(result.winner);
                let reasons = match_reasons(rules, result, &probe);
                let policy = policy_for(rules, Some(rule.spec()));
                let staleness = self.engine.evaluate(&target, &policy);
                let mut class = assemble(Some((rule.id(), rule.spec())), staleness, size, reasons);
                crate::classify::trim(rules, &path, rule.spec(), &mut class);
                crate::classify::keep_foreign(rules, &path, rule.spec(), &mut class);
                class
            },
        );
        if size_truncated {
            classification.reasons.push(Reason::new(
                "SIZE_PARTIAL",
                format!("size measured from the first {SIZE_WALK_LIMIT} entries only"),
            ));
        }
        Ok(Explanation {
            path,
            kind,
            classification,
            enclosing,
            near_misses,
            size_truncated,
        })
    }

    /// Classification of a path no rule matched.
    fn unmatched(
        &self,
        path: &Path,
        target: &Target<'_>,
        size: u64,
        near_misses: &[NearMiss],
        enclosing: Option<&Enclosing>,
    ) -> Classification {
        let rules = self.rules;
        let mut reasons = vec![Reason::new("NO_MATCH", "no rule matched this path")];
        for miss in near_misses {
            reasons.push(Reason::new(
                "NEAR_MISS",
                format!("rule `{}` considered: {}", miss.rule_id, miss.reason),
            ));
        }
        if let Some(enc) = enclosing {
            reasons.push(Reason::new(
                "INSIDE_MATCH",
                format!(
                    "inside {} which matched rule `{}`",
                    enc.path.display(),
                    enc.rule_id
                ),
            ));
        }
        let policy = policy_for(rules, None);
        let staleness = self.engine.evaluate(target, &policy);
        let (action, confidence) = rules
            .durable_roots
            .iter()
            .find(|r| path.parent() == Some(r.as_path()))
            .map_or((ActionSpec::Report, Confidence::Low), |root| {
                reasons.push(Reason::new(
                    "DURABLE_ROOT",
                    format!("item directly under durable root {}", root.display()),
                ));
                (ActionSpec::Trash, Confidence::Medium)
            });
        outcome_reasons(
            &mut reasons,
            &staleness,
            size,
            confidence,
            None,
            &action,
            false,
        );
        Classification {
            rule_id: None,
            category: None,
            ephemeral: None,
            confidence,
            score: score(size, &staleness, confidence, None),
            staleness,
            size,
            action,
            auto: false,
            reasons,
        }
    }

    /// Finds the nearest ancestor of `path` matched by a rule (checking at most 64 levels).
    fn enclosing(&self, path: &Path) -> Option<Enclosing> {
        let mut current = path.parent()?;
        for _ in 0..64 {
            let (Some(dir), Some(name)) = (current.parent(), current.file_name()) else {
                return None;
            };
            let listing = list_names(dir);
            let entries: Vec<ChildEntry<'_>> = listing
                .iter()
                .map(|(n, k)| ChildEntry { name: n, kind: *k })
                .collect();
            let probe = Probe {
                dir,
                name,
                kind: EntryKind::Dir,
                siblings: &entries,
                contents: Contents::Fs,
            };
            if let Some(result) = self.rules.match_entry(&probe, false) {
                return Some(Enclosing {
                    path: current.to_path_buf(),
                    rule_id: self.rules.rule(result.winner).id().to_owned(),
                });
            }
            current = dir;
        }
        None
    }
}
