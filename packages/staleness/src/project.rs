//! `PROJECT_ACTIVITY`: activity of the project that owns an item.
//!
//! The owning project root is the nearest ancestor (starting at the item itself) containing a
//! `.git` entry; if there is none, the nearest ancestor containing a known manifest
//! ([`DEFAULT_PROJECT_MARKERS`]). The home directory and filesystem root are never project
//! roots (a dotfiles repository at `~` would otherwise make everything look active).
//!
//! Activity is the newest of:
//!
//! * the `HEAD` commit's committer time (read with `gix`)
//! * the mtime of the `HEAD` reflog (`logs/HEAD`: commits, checkouts, rebases, resets)
//! * the newest source file mtime from a bounded walk that skips ephemeral directories
//!   ([`DEFAULT_SKIP_DIRS`], dot-directories, and caller-provided names)
//!
//! The git index is deliberately left out: every `git status` (a shell prompt, a git UI, an
//! agent) rewrites it, so it looks fresh while nobody works on the project. Staging edits
//! sources, which the walk sees anyway.
//!
//! Results are cached per project root.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bclean_model::{SignalKind, SignalReading};

use crate::walk::{WalkLimits, bounded_walk};
use crate::{SignalProvider, Target, git};

/// Manifest files that mark a project root.
pub const DEFAULT_PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "go.mod",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "pom.xml",
    "Package.swift",
    "mix.exs",
    "CMakeLists.txt",
    "Gemfile",
    "composer.json",
    "stack.yaml",
    "cabal.project",
    "build.zig",
    "pubspec.yaml",
    "flake.nix",
    "deno.json",
    "MODULE.bazel",
    "WORKSPACE",
    "Makefile",
];

/// Directory names never counted as source when looking for the newest source mtime.
pub const DEFAULT_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "build",
    "dist",
    "out",
    "DerivedData",
    "__pycache__",
    "venv",
    "env",
    "vendor",
    "Pods",
    "_build",
    "deps",
    "dist-newstyle",
    "zig-cache",
    "zig-out",
    "coverage",
    "tmp",
    "logs",
];

/// Activity summary for one project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    /// Project root directory.
    pub root: PathBuf,
    /// Whether the root is a git worktree.
    pub git: bool,
    /// `HEAD` commit time.
    pub last_commit: Option<i64>,
    /// mtime of the `HEAD` reflog.
    pub reflog_mtime: Option<i64>,
    /// Newest source mtime (bounded walk).
    pub newest_source: Option<i64>,
    /// Whether the source walk hit its limits.
    pub source_walk_truncated: bool,
}

impl ProjectInfo {
    /// Newest activity across all sources.
    #[must_use]
    pub fn last_activity(&self) -> Option<i64> {
        [self.last_commit, self.reflog_mtime, self.newest_source]
            .into_iter()
            .flatten()
            .max()
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(t) = self.last_commit {
            parts.push(format!("last commit {t}"));
        }
        if let Some(t) = self.reflog_mtime {
            parts.push(format!("HEAD reflog {t}"));
        }
        if let Some(t) = self.newest_source {
            let partial = if self.source_walk_truncated {
                " (partial)"
            } else {
                ""
            };
            parts.push(format!("newest source {t}{partial}"));
        }
        format!("project {}: {}", self.root.display(), parts.join(", "))
    }
}

/// Finds the project root owning `path` (see the module docs). `home` is excluded.
#[must_use]
pub fn find_project_root(path: &Path, home: Option<&Path>, markers: &[&str]) -> Option<PathBuf> {
    let mut nearest_manifest: Option<PathBuf> = None;
    for dir in path.ancestors().take(64) {
        if dir.parent().is_none() || home == Some(dir) {
            break;
        }
        if dir.join(".git").symlink_metadata().is_ok() {
            return Some(dir.to_path_buf());
        }
        if nearest_manifest.is_none() && markers.iter().any(|m| dir.join(m).is_file()) {
            nearest_manifest = Some(dir.to_path_buf());
        }
    }
    nearest_manifest
}

type Slot = Arc<OnceLock<ProjectInfo>>;

/// `PROJECT_ACTIVITY` provider.
#[derive(Debug)]
pub struct ProjectActivityProvider {
    home: Option<PathBuf>,
    markers: Vec<String>,
    skip: BTreeSet<String>,
    limits: WalkLimits,
    cache: Mutex<BTreeMap<PathBuf, Slot>>,
}

impl Default for ProjectActivityProvider {
    fn default() -> Self {
        Self::new(std::env::home_dir())
    }
}

impl ProjectActivityProvider {
    /// A provider with default markers and skip list; `home` is never treated as a project.
    #[must_use]
    pub fn new(home: Option<PathBuf>) -> Self {
        Self {
            home,
            markers: DEFAULT_PROJECT_MARKERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            skip: DEFAULT_SKIP_DIRS.iter().map(|s| (*s).to_owned()).collect(),
            limits: WalkLimits {
                max_entries: 50_000,
                max_depth: 12,
            },
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Adds directory names to skip in the source walk (for example literal names of
    /// ephemeral rules).
    #[must_use]
    pub fn with_skip_dirs(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.skip.extend(names);
        self
    }

    /// Overrides the source walk limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: WalkLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Finds the project owning `path`.
    #[must_use]
    pub fn project_root(&self, path: &Path) -> Option<PathBuf> {
        let markers: Vec<&str> = self.markers.iter().map(String::as_str).collect();
        find_project_root(path, self.home.as_deref(), &markers)
    }

    /// Activity of the project owning `path` (cached per root).
    #[must_use]
    pub fn project(&self, path: &Path) -> Option<ProjectInfo> {
        let root = self.project_root(path)?;
        let slot = {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(cache.entry(root.clone()).or_default())
        };
        Some(slot.get_or_init(|| self.compute(root)).clone())
    }

    fn compute(&self, root: PathBuf) -> ProjectInfo {
        let link = git::read_git_link(&root);
        let git_dir = link
            .as_ref()
            .and_then(|l| l.git_dir().map(Path::to_path_buf));
        let last_commit = git_dir
            .as_ref()
            .and_then(|_| git::open(&root))
            .and_then(|repo| git::head_commit_time(&repo));
        let reflog_mtime = git_dir
            .as_ref()
            .and_then(|d| git::mtime(&d.join("logs").join("HEAD")));
        let skip = |name: &OsStr| {
            name.to_str()
                .is_none_or(|n| n.starts_with('.') || self.skip.contains(n))
        };
        let walk = bounded_walk(&root, self.limits, false, &skip);
        ProjectInfo {
            git: link.is_some(),
            root,
            last_commit,
            reflog_mtime,
            newest_source: walk.max_mtime,
            source_walk_truncated: walk.truncated,
        }
    }
}

impl SignalProvider for ProjectActivityProvider {
    fn kind(&self) -> SignalKind {
        SignalKind::ProjectActivity
    }

    fn read(&self, target: &Target<'_>) -> Option<SignalReading> {
        let start = if target.is_dir {
            target.path
        } else {
            target.path.parent()?
        };
        let info = self.project(start)?;
        Some(SignalReading {
            kind: SignalKind::ProjectActivity,
            last_activity: info.last_activity(),
            forces_stale: false,
            detail: info.describe(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{commit_at, init};
    use filetime::{FileTime, set_file_mtime};
    use std::fs;

    fn set_tree_mtime(root: &Path, secs: i64) {
        for entry in fs::read_dir(root).unwrap().flatten() {
            let p = entry.path();
            if entry.file_type().unwrap().is_dir() {
                set_tree_mtime(&p, secs);
            }
            set_file_mtime(&p, FileTime::from_unix_time(secs, 0)).unwrap();
        }
        set_file_mtime(root, FileTime::from_unix_time(secs, 0)).unwrap();
    }

    #[test]
    fn manifest_project_uses_source_mtime_and_skips_ephemeral() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("proj");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("target/debug/bin"), "").unwrap();
        set_tree_mtime(&root, 1_000);
        set_file_mtime(root.join("src/main.rs"), FileTime::from_unix_time(5_000, 0)).unwrap();
        set_file_mtime(
            root.join("target/debug/bin"),
            FileTime::from_unix_time(9_000, 0),
        )
        .unwrap();

        let provider = ProjectActivityProvider::new(None);
        let target_dir = root.join("target");
        let reading = provider.read(&Target::path(&target_dir, true)).unwrap();
        assert_eq!(reading.last_activity, Some(5_000));
        assert_eq!(provider.project_root(&target_dir), Some(root));
    }

    #[test]
    fn git_project_uses_commit_time_and_excludes_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let repo = home.join("repo");
        init(&repo);
        commit_at(&repo, "a.txt", 1_600_000_000);
        // Nested manifest dir inside the repo: the git root wins.
        fs::create_dir_all(repo.join("crates/x")).unwrap();
        fs::write(repo.join("crates/x/Cargo.toml"), "").unwrap();

        let provider = ProjectActivityProvider::new(Some(home.clone()));
        let info = provider.project(&repo.join("crates/x")).unwrap();
        assert_eq!(info.root, repo);
        assert!(info.git);
        assert_eq!(info.last_commit, Some(1_600_000_000));
        assert!(info.reflog_mtime.is_some());

        // Home itself is never a project, even with a .git directory.
        fs::create_dir_all(home.join(".git")).unwrap();
        fs::create_dir_all(home.join("loose")).unwrap();
        assert_eq!(provider.project_root(&home.join("loose")), None);
    }
}
