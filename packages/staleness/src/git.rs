//! Shared git helpers built on `gix` (opened in isolated mode: only the repository's own
//! configuration is read, never the user's global configuration or environment).

use std::fs;
use std::path::{Path, PathBuf};

/// What a `.git` entry in a directory points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitDirLink {
    /// `.git` is a directory (a main worktree or a non-bare repository).
    Dir(PathBuf),
    /// `.git` is a file with `gitdir: <path>` (a linked worktree or submodule).
    File {
        /// Resolved git directory.
        gitdir: PathBuf,
        /// Whether that git directory exists.
        exists: bool,
    },
}

impl GitDirLink {
    /// The git directory, if it exists.
    #[must_use]
    pub fn git_dir(&self) -> Option<&Path> {
        match self {
            Self::Dir(dir) => Some(dir),
            Self::File {
                gitdir,
                exists: true,
            } => Some(gitdir),
            Self::File { .. } => None,
        }
    }
}

/// Inspects `<dir>/.git`, returning `None` if it does not exist or is unreadable.
#[must_use]
pub fn read_git_link(dir: &Path) -> Option<GitDirLink> {
    let dot_git = dir.join(".git");
    let meta = fs::symlink_metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(GitDirLink::Dir(dot_git));
    }
    if meta.file_type().is_symlink() {
        return fs::metadata(&dot_git)
            .ok()
            .filter(fs::Metadata::is_dir)
            .map(|_| GitDirLink::Dir(dot_git));
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let raw = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?
        .trim();
    let gitdir = {
        let p = PathBuf::from(raw);
        if p.is_absolute() { p } else { dir.join(p) }
    };
    let exists = gitdir.is_dir();
    Some(GitDirLink::File { gitdir, exists })
}

/// Opens the repository whose worktree (or git dir) is `dir`, in isolated mode.
pub fn open(dir: &Path) -> Option<gix::Repository> {
    gix::open_opts(dir, gix::open::Options::isolated()).ok()
}

/// Commit time (Unix seconds) of `HEAD`.
pub fn head_commit_time(repo: &gix::Repository) -> Option<i64> {
    repo.head_commit().ok()?.time().ok().map(|t| t.seconds)
}

/// Peeled object id of a fully-qualified reference, if it exists.
pub fn ref_id(repo: &gix::Repository, name: &str) -> Option<gix::ObjectId> {
    let mut reference = repo.try_find_reference(name).ok()??;
    reference.peel_to_id().ok().map(gix::Id::detach)
}

/// mtime (Unix seconds) of a file, if it exists.
pub fn mtime(path: &Path) -> Option<i64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(crate::system_time_secs)
}

#[cfg(test)]
pub mod test_support {
    //! Helpers for building real git repositories in tests (requires the `git` CLI).

    use std::path::Path;
    use std::process::Command;

    /// Runs git with a deterministic identity, panicking on failure.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git is installed");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Commits a file with a fixed timestamp.
    pub fn commit_at(dir: &Path, file: &str, epoch: i64) {
        std::fs::write(dir.join(file), file).unwrap();
        git(dir, &["add", file]);
        let date = format!("{epoch} +0000");
        let out = Command::new("git")
            .args(["commit", "-q", "-m", file])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Initializes a repository on branch `main`.
    pub fn init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
    }
}
