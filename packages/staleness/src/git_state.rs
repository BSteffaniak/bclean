//! `GIT_STATE`: worktree state that makes an item stale regardless of timestamps.
//!
//! For an item, the nearest ancestor (including the item) with a `.git` entry is its worktree.
//! The provider forces staleness when that worktree is a *linked* worktree and:
//!
//! * its `.git` file points to a git directory that no longer exists (orphaned worktree), or
//! * its branch was deleted, or
//! * its branch is fully merged into the default branch (`origin/HEAD`, else `main`, `master`,
//!   `trunk`, `develop`; local or `origin/` remote-tracking refs), or
//! * its branch tracked an upstream branch that is gone (`git branch -vv` shows `[gone]`;
//!   typical after a squash-merged pull request whose remote branch was deleted).
//!
//! Outside any repository, a directory named like a worktree-setup sibling (`wt-*`, `*-wt-*`,
//! `*.wt-*`) without a `.git` entry, next to a sibling that *is* a repository, is reported as a
//! leftover worktree directory and also forces staleness.
//!
//! Main worktrees never force staleness. Results are cached per worktree root.
//!
//! Only build output inside such a worktree (under a [`DEFAULT_SKIP_DIRS`] directory such as
//! `target/`) is forced stale on that basis alone: deleting it loses nothing. The worktree
//! itself and its sources are forced stale only when the worktree is fully redundant: its
//! working tree is clean (no modified, staged, or untracked files), and its commits survive
//! it (merged into the default branch, or still on the branch ref while only the upstream is
//! gone). A deleted branch, an orphaned worktree, or a leftover directory never qualifies.
//!
//! [`DEFAULT_SKIP_DIRS`]: crate::DEFAULT_SKIP_DIRS

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bclean_model::{SignalKind, SignalReading};

use crate::git::{self, GitDirLink};
use crate::{SignalProvider, Target};

/// Candidate default branch names, in preference order.
const DEFAULT_BRANCHES: &[&str] = &["main", "master", "trunk", "develop"];

/// State of a git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeState {
    /// The main worktree of a repository.
    Main {
        /// Checked-out branch, if any.
        branch: Option<String>,
    },
    /// A linked worktree with a detached `HEAD`.
    Detached,
    /// A linked worktree whose branch is still active.
    Active {
        /// Branch name.
        branch: String,
    },
    /// A linked worktree whose branch is merged into the default branch.
    Merged {
        /// Branch name.
        branch: String,
        /// Default branch it is merged into.
        into: String,
    },
    /// A linked worktree whose branch no longer exists.
    BranchDeleted {
        /// Branch name.
        branch: String,
    },
    /// A linked worktree whose upstream branch was deleted.
    UpstreamGone {
        /// Branch name.
        branch: String,
        /// Missing remote-tracking ref.
        upstream: String,
    },
    /// The `.git` file points to a missing git directory.
    Orphaned {
        /// The missing git directory.
        gitdir: PathBuf,
    },
    /// A worktree-like sibling directory with no `.git` at all.
    Leftover,
    /// The repository could not be opened.
    Unknown,
}

impl WorktreeState {
    /// Whether this state makes items inside the worktree stale.
    #[must_use]
    pub const fn forces_stale(&self) -> bool {
        matches!(
            self,
            Self::Merged { .. }
                | Self::BranchDeleted { .. }
                | Self::UpstreamGone { .. }
                | Self::Orphaned { .. }
                | Self::Leftover
        )
    }

    fn describe(&self, root: &Path) -> String {
        let root = root.display();
        match self {
            Self::Main { branch } => format!(
                "main worktree {root} on {}",
                branch.as_deref().unwrap_or("detached HEAD")
            ),
            Self::Detached => format!("linked worktree {root} with detached HEAD"),
            Self::Active { branch } => format!("linked worktree {root} on active branch {branch}"),
            Self::Merged { branch, into } => {
                format!("linked worktree {root}: branch {branch} is merged into {into}")
            }
            Self::BranchDeleted { branch } => {
                format!("linked worktree {root}: branch {branch} was deleted")
            }
            Self::UpstreamGone { branch, upstream } => {
                format!("linked worktree {root}: upstream {upstream} of {branch} is gone")
            }
            Self::Orphaned { gitdir } => format!(
                "orphaned worktree {root}: git directory {} is missing",
                gitdir.display()
            ),
            Self::Leftover => {
                format!("leftover worktree directory {root} (no .git, next to a repository)")
            }
            Self::Unknown => format!("worktree {root}: repository could not be read"),
        }
    }
}

/// One worktree of a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Checkout directory.
    pub path: PathBuf,
    /// Whether the checkout directory exists.
    pub exists: bool,
    /// Whether the worktree is locked (`git worktree lock`).
    pub locked: bool,
    /// State (only computed when the checkout exists).
    pub state: WorktreeState,
}

/// Analyzes the worktree rooted at `root` (a directory containing `.git`).
#[must_use]
pub fn analyze_worktree(root: &Path) -> WorktreeState {
    match git::read_git_link(root) {
        None => WorktreeState::Unknown,
        Some(GitDirLink::File {
            gitdir,
            exists: false,
        }) => WorktreeState::Orphaned { gitdir },
        Some(link) => {
            let Some(repo) = git::open(root) else {
                return WorktreeState::Unknown;
            };
            let branch = repo
                .head_name()
                .ok()
                .flatten()
                .map(|name| name.as_bstr().to_string());
            let linked = matches!(link, GitDirLink::File { .. })
                && repo.kind() == gix::repository::Kind::LinkedWorkTree;
            if !linked {
                return WorktreeState::Main {
                    branch: branch.map(|b| short(&b).to_owned()),
                };
            }
            let Some(full) = branch else {
                return WorktreeState::Detached;
            };
            let name = short(&full).to_owned();
            let Some(tip) = git::ref_id(&repo, &full) else {
                return WorktreeState::BranchDeleted { branch: name };
            };
            if let Some(into) = merged_into(&repo, &name, tip) {
                return WorktreeState::Merged { branch: name, into };
            }
            if let Some(upstream) = gone_upstream(&repo, &name) {
                return WorktreeState::UpstreamGone {
                    branch: name,
                    upstream,
                };
            }
            WorktreeState::Active { branch: name }
        }
    }
}

fn short(full: &str) -> &str {
    full.strip_prefix("refs/heads/").unwrap_or(full)
}

/// Default-branch candidates as (display name, full ref) pairs, best first.
fn default_branch_refs(repo: &gix::Repository) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(Some(head)) = repo.try_find_reference("refs/remotes/origin/HEAD")
        && let Some(target) = head.target().try_name()
    {
        let full = target.as_bstr().to_string();
        let name = full
            .strip_prefix("refs/remotes/origin/")
            .unwrap_or(&full)
            .to_owned();
        out.push((name.clone(), format!("refs/heads/{name}")));
        out.push((format!("origin/{name}"), full));
    }
    for name in DEFAULT_BRANCHES {
        out.push(((*name).to_owned(), format!("refs/heads/{name}")));
        out.push((
            format!("origin/{name}"),
            format!("refs/remotes/origin/{name}"),
        ));
    }
    out
}

/// Returns the default branch `tip` is merged into, if any (and the branch is not itself the
/// default branch).
fn merged_into(repo: &gix::Repository, branch: &str, tip: gix::ObjectId) -> Option<String> {
    let candidates = default_branch_refs(repo);
    let is_default = |name: &str| name.strip_prefix("origin/").unwrap_or(name) == branch;
    if DEFAULT_BRANCHES.contains(&branch) || candidates.first().is_some_and(|(n, _)| is_default(n))
    {
        return None;
    }
    for (name, full) in candidates {
        if is_default(&name) {
            continue;
        }
        let Some(base_tip) = git::ref_id(repo, &full) else {
            continue;
        };
        if base_tip == tip {
            // A branch pointing exactly at the default branch tip has no unique work, but is
            // usually a freshly created branch: do not treat it as merged.
            continue;
        }
        if let Ok(Some(base)) = repo.merge_base(tip, base_tip)
            && base.detach() == tip
        {
            return Some(name);
        }
    }
    None
}

/// The configured upstream remote-tracking ref of `branch`, if configured but missing.
fn gone_upstream(repo: &gix::Repository, branch: &str) -> Option<String> {
    let config = repo.config_snapshot();
    let remote = config.string(format!("branch.{branch}.remote").as_str())?;
    let merge = config.string(format!("branch.{branch}.merge").as_str())?;
    let remote = remote.to_string();
    if remote == "." {
        return None;
    }
    let merge = merge.to_string();
    let short_merge = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    let tracking = format!("refs/remotes/{remote}/{short_merge}");
    git::ref_id(repo, &tracking)
        .is_none()
        .then(|| format!("{remote}/{short_merge}"))
}

/// Whether `name` looks like a worktree-setup sibling directory.
fn worktree_like_name(name: &str) -> bool {
    name.starts_with("wt-")
        || name.contains("-wt-")
        || name.contains(".wt-")
        || name.ends_with("-wt")
}

/// Detects a leftover worktree directory: `dir` has a worktree-like name, no `.git`, and a
/// sibling that is a repository.
fn is_leftover(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !worktree_like_name(name) || dir.join(".git").symlink_metadata().is_ok() {
        return false;
    }
    let Some(parent) = dir.parent() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return false;
    };
    entries
        .flatten()
        .take(4_096)
        .any(|e| e.path() != dir && e.path().join(".git").symlink_metadata().is_ok())
}

/// Lists every worktree of the repository containing `repo_dir`, including the main one.
#[must_use]
pub fn list_worktrees(repo_dir: &Path) -> Vec<WorktreeInfo> {
    let Some(repo) = git::open(repo_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let main_root = repo
        .common_dir()
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| p.join(".git").is_dir());
    if let Some(main) = main_root {
        out.push(WorktreeInfo {
            state: analyze_worktree(&main),
            exists: true,
            locked: false,
            path: main,
        });
    }
    let Ok(proxies) = repo.worktrees() else {
        return out;
    };
    for proxy in proxies {
        let Ok(path) = proxy.base() else {
            continue;
        };
        let exists = path.is_dir();
        out.push(WorktreeInfo {
            state: if exists {
                analyze_worktree(&path)
            } else {
                WorktreeState::Unknown
            },
            exists,
            locked: proxy.is_locked(),
            path,
        });
    }
    out
}

type Slot = Arc<OnceLock<(PathBuf, WorktreeState)>>;

/// `GIT_STATE` provider.
#[derive(Debug, Default)]
pub struct GitStateProvider {
    cache: Mutex<BTreeMap<PathBuf, Slot>>,
    /// Whether each worktree's working tree is clean (see [`is_clean`]).
    clean: Mutex<BTreeMap<PathBuf, Arc<OnceLock<bool>>>>,
}

impl GitStateProvider {
    /// State of the worktree owning `path` (cached per worktree root), plus that root.
    #[must_use]
    pub fn worktree_state(&self, path: &Path) -> Option<(PathBuf, WorktreeState)> {
        let mut leftover = None;
        let mut root = None;
        for dir in path.ancestors().take(64) {
            if dir.join(".git").symlink_metadata().is_ok() {
                root = Some(dir.to_path_buf());
                break;
            }
            if leftover.is_none() && dir.parent().is_some() && is_leftover(dir) {
                leftover = Some(dir.to_path_buf());
            }
        }
        let key = match (root, leftover) {
            (Some(root), _) => root,
            (None, Some(dir)) => return Some((dir, WorktreeState::Leftover)),
            (None, None) => return None,
        };
        let slot = {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(cache.entry(key.clone()).or_default())
        };
        Some(
            slot.get_or_init(|| (key.clone(), analyze_worktree(&key)))
                .clone(),
        )
    }
}

impl SignalProvider for GitStateProvider {
    fn kind(&self) -> SignalKind {
        SignalKind::GitState
    }

    fn read(&self, target: &Target<'_>) -> Option<SignalReading> {
        let (root, state) = self.worktree_state(target.path)?;
        let mut detail = state.describe(&root);
        let forces_stale = state.forces_stale()
            && (is_build_output(&root, target.path) || {
                let redundant = self.is_redundant(&root, &state);
                if !redundant {
                    detail.push_str(
                        "; the worktree itself is kept: it has uncommitted work or commits \
                         nothing else holds",
                    );
                }
                redundant
            });
        Some(SignalReading {
            kind: SignalKind::GitState,
            last_activity: None,
            forces_stale,
            detail,
        })
    }
}

impl GitStateProvider {
    /// Whether removing the worktree at `root` would lose nothing: its commits survive it and
    /// its working tree is clean.
    fn is_redundant(&self, root: &Path, state: &WorktreeState) -> bool {
        if !matches!(
            state,
            WorktreeState::Merged { .. } | WorktreeState::UpstreamGone { .. }
        ) {
            return false;
        }
        let slot = {
            let mut cache = self.clean.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(cache.entry(root.to_path_buf()).or_default())
        };
        *slot.get_or_init(|| is_clean(root))
    }
}

/// Whether `path` is build output inside the worktree at `root`: at or below a directory such
/// as `target/` or `node_modules/`.
fn is_build_output(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|name| crate::DEFAULT_SKIP_DIRS.contains(&name))
        })
    })
}

/// Whether the working tree at `root` has no modified, staged, or untracked (non-ignored)
/// files, per `git status`. Unknown (git missing or failing) counts as not clean.
fn is_clean(root: &Path) -> bool {
    std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .current_dir(root)
        .output()
        .is_ok_and(|out| out.status.success() && out.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{commit_at, git, init};
    use std::fs;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let repo = base.join("repo");
        init(&repo);
        commit_at(&repo, "a.txt", 1_600_000_000);
        (dir, base)
    }

    #[test]
    fn main_worktree_is_not_forced() {
        let (_d, base) = setup();
        let state = analyze_worktree(&base.join("repo"));
        assert_eq!(
            state,
            WorktreeState::Main {
                branch: Some("main".to_owned())
            }
        );
        assert!(!state.forces_stale());
    }

    #[test]
    fn merged_active_deleted_and_orphaned_worktrees() {
        let (_d, base) = setup();
        let repo = base.join("repo");

        // Active: has a commit not on main.
        let active = base.join("repo-wt-active");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "active",
                active.to_str().unwrap(),
            ],
        );
        commit_at(&active, "b.txt", 1_600_000_100);
        assert_eq!(
            analyze_worktree(&active),
            WorktreeState::Active {
                branch: "active".to_owned()
            }
        );

        // Merged: a feature commit that main then fast-forwards past.
        let merged = base.join("repo-wt-merged");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                merged.to_str().unwrap(),
            ],
        );
        commit_at(&merged, "c.txt", 1_600_000_200);
        git(&repo, &["merge", "-q", "--ff-only", "feature"]);
        commit_at(&repo, "d.txt", 1_600_000_300);
        let state = analyze_worktree(&merged);
        assert_eq!(
            state,
            WorktreeState::Merged {
                branch: "feature".to_owned(),
                into: "main".to_owned()
            }
        );
        let provider = GitStateProvider::default();
        let target = merged.join("target");
        fs::create_dir_all(&target).unwrap();
        let reading = provider.read(&Target::path(&target, true)).unwrap();
        assert!(reading.forces_stale, "{}", reading.detail);
        // The worktree itself only once nothing in it would be lost.
        fs::write(merged.join("notes.txt"), b"wip").unwrap();
        let reading = GitStateProvider::default()
            .read(&Target::path(&merged, true))
            .unwrap();
        assert!(!reading.forces_stale, "untracked work: {}", reading.detail);
        fs::remove_file(merged.join("notes.txt")).unwrap();
        let reading = GitStateProvider::default()
            .read(&Target::path(&merged, true))
            .unwrap();
        assert!(reading.forces_stale, "clean and merged: {}", reading.detail);

        // Branch deleted underneath the worktree (git refuses `branch -D` for checked-out
        // branches, so delete the ref directly).
        let gone = base.join("repo-wt-gone");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "gone",
                gone.to_str().unwrap(),
            ],
        );
        commit_at(&gone, "e.txt", 1_600_000_400);
        git(&repo, &["update-ref", "-d", "refs/heads/gone"]);
        assert_eq!(
            analyze_worktree(&gone),
            WorktreeState::BranchDeleted {
                branch: "gone".to_owned()
            }
        );
        // Its commits live only in the worktree: never redundant, even when clean.
        let reading = provider.read(&Target::path(&gone, true)).unwrap();
        assert!(!reading.forces_stale, "{}", reading.detail);
        let reading = provider
            .read(&Target::path(&gone.join("target"), true))
            .unwrap();
        assert!(reading.forces_stale, "{}", reading.detail);

        // Orphaned: the git directory was removed.
        let orphan = base.join("repo-wt-orphan");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "orphan",
                orphan.to_str().unwrap(),
            ],
        );
        fs::remove_dir_all(repo.join(".git/worktrees/repo-wt-orphan")).unwrap();
        assert!(matches!(
            analyze_worktree(&orphan),
            WorktreeState::Orphaned { .. }
        ));

        let listed = list_worktrees(&repo);
        assert!(listed.iter().any(|w| w.path == repo));
        assert!(
            listed
                .iter()
                .any(|w| w.path.ends_with("repo-wt-merged") && w.state.forces_stale())
        );
    }

    #[test]
    fn upstream_gone_is_detected() {
        let (_d, base) = setup();
        let repo = base.join("repo");
        let wt = base.join("repo-wt-up");
        git(
            &repo,
            &["worktree", "add", "-q", "-b", "up", wt.to_str().unwrap()],
        );
        commit_at(&wt, "u.txt", 1_600_000_500);
        git(&wt, &["config", "branch.up.remote", "origin"]);
        git(&wt, &["config", "branch.up.merge", "refs/heads/up"]);
        assert_eq!(
            analyze_worktree(&wt),
            WorktreeState::UpstreamGone {
                branch: "up".to_owned(),
                upstream: "origin/up".to_owned()
            }
        );
    }

    #[test]
    fn leftover_sibling_directory() {
        let (_d, base) = setup();
        let leftover = base.join("repo-wt-old");
        fs::create_dir_all(leftover.join("target")).unwrap();
        let provider = GitStateProvider::default();
        let (_, state) = provider.worktree_state(&leftover.join("target")).unwrap();
        assert_eq!(state, WorktreeState::Leftover);
        assert!(provider.worktree_state(&base.join("plain")).is_none());
    }
}
