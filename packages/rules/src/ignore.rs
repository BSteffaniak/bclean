//! Gitignore evaluation for `match.git_ignored` and the gitignored heuristic.
//!
//! This is a faithful-but-lightweight approximation of git's exclude logic built on
//! `gix::glob` patterns (the same pattern engine gix uses):
//!
//! * `.gitignore` files from the repository root down to the entry's parent directory, the
//!   deepest file deciding first and the last matching pattern within a file winning
//! * `$GIT_DIR/info/exclude`, then the global excludes file (`$XDG_CONFIG_HOME/git/ignore` or
//!   `~/.config/git/ignore`)
//! * an excluded parent directory excludes everything below it
//!
//! Differences from git: `core.excludesFile` is not read (only the default global location),
//! and the index is not consulted (tracked files that match an ignore pattern count as ignored).
//! Parsed files and repository roots are cached; the cache is `Sync`.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bclean_config::Expander;
use gix::bstr::BStr;
use gix::glob::Pattern;
use gix::glob::pattern::Case;
use gix::glob::wildmatch::Mode;

type Patterns = Arc<[Pattern]>;

/// Cached gitignore oracle.
#[derive(Debug)]
pub struct GitIgnoreCache {
    case: Case,
    global_path: Option<PathBuf>,
    global: OnceLock<Patterns>,
    roots: Mutex<BTreeMap<PathBuf, Option<PathBuf>>>,
    files: Mutex<BTreeMap<PathBuf, Patterns>>,
}

fn parse_patterns(text: &str) -> Patterns {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches(['\r', ' ', '\t']);
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line
                .strip_prefix('\\')
                .filter(|l| l.starts_with('#'))
                .unwrap_or(line);
            gix::glob::parse(line)
        })
        .collect()
}

fn read_patterns(path: &Path) -> Patterns {
    std::fs::read_to_string(path).map_or_else(|_| Arc::from(Vec::new()), |t| parse_patterns(&t))
}

/// Outcome of checking one path against one pattern list.
fn decide(patterns: &[Pattern], rel: &str, is_dir: bool, case: Case) -> Option<bool> {
    let basename = rel.rfind('/').map(|p| p + 1);
    patterns.iter().rev().find_map(|p| {
        p.matches_repo_relative_path(
            BStr::new(rel.as_bytes()),
            basename,
            Some(is_dir),
            case,
            Mode::NO_MATCH_SLASH_LITERAL,
        )
        .then(|| !p.is_negative())
    })
}

impl GitIgnoreCache {
    /// Creates an empty cache; the expander locates the global excludes file.
    #[must_use]
    pub fn new(expander: &Expander) -> Self {
        let global_path = expander
            .var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| expander.home().map(|h| h.join(".config")))
            .map(|c| c.join("git").join("ignore"));
        Self {
            case: if cfg!(any(target_os = "macos", target_os = "windows")) {
                Case::Fold
            } else {
                Case::Sensitive
            },
            global_path,
            global: OnceLock::new(),
            roots: Mutex::new(BTreeMap::new()),
            files: Mutex::new(BTreeMap::new()),
        }
    }

    /// The repository root (worktree directory containing `.git`) enclosing `dir`, if any.
    #[must_use]
    pub fn repo_root(&self, dir: &Path) -> Option<PathBuf> {
        if let Some(hit) = self
            .roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(dir)
        {
            return hit.clone();
        }
        let found = dir
            .ancestors()
            .take(128)
            .find(|d| d.join(".git").symlink_metadata().is_ok())
            .map(Path::to_path_buf);
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(dir.to_path_buf(), found.clone());
        found
    }

    fn file(&self, path: PathBuf) -> Patterns {
        if let Some(hit) = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&path)
        {
            return Arc::clone(hit);
        }
        let parsed = read_patterns(&path);
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, Arc::clone(&parsed));
        parsed
    }

    fn global(&self) -> &[Pattern] {
        self.global.get_or_init(|| {
            self.global_path
                .as_deref()
                .map_or_else(|| Arc::from(Vec::new()), read_patterns)
        })
    }

    /// Whether `path` is ignored by git. Returns `None` when `path` is not inside a
    /// repository (or is the repository root). Entries inside `.git` are never ignored.
    ///
    /// When `check_ancestors` is `false` the caller guarantees that no ancestor directory
    /// between the repository root and `path` is ignored (true during a top-down walk that
    /// stops at ignored directories), which avoids re-evaluating them.
    #[must_use]
    pub fn is_ignored(&self, path: &Path, is_dir: bool, check_ancestors: bool) -> Option<bool> {
        let root = self.repo_root(path.parent()?)?;
        let rel = path.strip_prefix(&root).ok()?;
        let names: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        if names.is_empty() {
            return None;
        }
        if names.iter().any(|n| n == ".git") {
            return Some(false);
        }
        // Pattern lists per directory level: level j = root/names[..j]/.gitignore.
        let mut levels: Vec<Patterns> = Vec::with_capacity(names.len());
        let mut dir = root.clone();
        levels.push(self.file(dir.join(".gitignore")));
        let info = self.file(root.join(".git").join("info").join("exclude"));
        let start = if check_ancestors { 1 } else { names.len() };
        for i in 1..=names.len() {
            if i > 1 {
                dir.push(&names[i - 2]);
                levels.push(self.file(dir.join(".gitignore")));
            }
            if i < start {
                continue;
            }
            let entry_is_dir = i < names.len() || is_dir;
            if self.decide_at(&names[..i], &levels, &info, entry_is_dir) {
                return Some(true);
            }
        }
        Some(false)
    }

    fn decide_at(
        &self,
        names: &[String],
        levels: &[Patterns],
        info: &[Pattern],
        is_dir: bool,
    ) -> bool {
        // Deepest .gitignore first: level j applies to names[j..].
        for j in (0..names.len()).rev() {
            let Some(patterns) = levels.get(j) else {
                continue;
            };
            if patterns.is_empty() {
                continue;
            }
            let rel = names[j..].join("/");
            if let Some(ignored) = decide(patterns, &rel, is_dir, self.case) {
                return ignored;
            }
        }
        let rel = names.join("/");
        decide(info, &rel, is_dir, self.case)
            .or_else(|| decide(self.global(), &rel, is_dir, self.case))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn evaluates_nested_gitignores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::create_dir_all(root.join("a/out")).unwrap();
        fs::create_dir_all(root.join("a/keep")).unwrap();
        fs::create_dir_all(root.join("gen/deep")).unwrap();
        fs::write(root.join(".gitignore"), "# comment\n/gen/\n*.log\nout/\n").unwrap();
        fs::write(root.join("a/.gitignore"), "!out/\nkeep\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "secret\n").unwrap();

        let cache = GitIgnoreCache::new(&Expander::new(None, BTreeMap::new()));
        assert_eq!(cache.is_ignored(&root.join("gen"), true, true), Some(true));
        assert_eq!(
            cache.is_ignored(&root.join("gen/deep"), true, true),
            Some(true)
        );
        assert_eq!(
            cache.is_ignored(&root.join("a/out"), true, true),
            Some(false)
        );
        assert_eq!(
            cache.is_ignored(&root.join("a/keep"), true, true),
            Some(true)
        );
        assert_eq!(
            cache.is_ignored(&root.join("a/x.log"), false, true),
            Some(true)
        );
        assert_eq!(
            cache.is_ignored(&root.join("secret"), false, true),
            Some(true)
        );
        assert_eq!(cache.is_ignored(&root.join("a"), true, true), Some(false));
        assert_eq!(
            cache.is_ignored(&root.join(".git/info"), true, true),
            Some(false)
        );
        assert_eq!(cache.is_ignored(&root, true, true), None);
        // out/ (dir-only) does not match a file named out.
        assert_eq!(
            cache.is_ignored(&root.join("b/out"), false, true),
            Some(false)
        );
    }
}
