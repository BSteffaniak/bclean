//! Protected paths: locations no action may ever touch.
//!
//! A path is refused when any of the following holds:
//!
//! * It is a filesystem root (`/`, `C:\`, `\\server\share`).
//! * It is one of a set of hardcoded "never-touch exactly" locations (the home directory itself,
//!   `/Users`, `/home`, `~/Documents`, `C:\Users`, ...). Their *children* are allowed.
//! * It is inside a hardcoded protected subtree (`/System`, `/usr`, `/etc`, `C:\Windows`,
//!   `C:\Program Files`, `~/.ssh`, bclean's own config/data dirs, ...).
//! * It is an *ancestor* of a protected subtree or literal protected path (deleting `~/.config`
//!   would delete bclean's config, so it is refused).
//! * It, or one of its ancestors, matches a user glob from `ProtectConfig.paths` (with `~`
//!   expansion). For globs, ancestors of the literal (non-glob) leading components are also
//!   protected, as are paths matching a leading prefix of the pattern before any `**`.
//!
//! Paths are compared both lexically normalized and with their parent canonicalized (the final
//! component is never resolved, because removing a symlink must not be judged by its target).
//! On Windows comparisons are case-insensitive.

use std::path::{Component, Path, PathBuf};

use bclean_model::ProtectConfig;
use globset::{GlobBuilder, GlobMatcher};

/// Error building a [`Protector`].
#[derive(Debug, thiserror::Error)]
pub enum ProtectError {
    /// A protect glob failed to compile.
    #[error("invalid protect pattern `{pattern}`: {source}")]
    BadPattern {
        /// The offending pattern.
        pattern: String,
        /// Underlying glob error.
        source: globset::Error,
    },
}

#[derive(Debug, Clone)]
struct UserGlob {
    pattern: String,
    full: GlobMatcher,
    /// Matchers for leading prefixes (before any `**`), with their component counts.
    prefixes: Vec<(usize, GlobMatcher)>,
}

/// Decides whether a path may be acted on.
#[derive(Debug, Clone)]
pub struct Protector {
    exact: Vec<PathBuf>,
    subtrees: Vec<PathBuf>,
    globs: Vec<UserGlob>,
}

impl Default for Protector {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Protector {
    /// Protector with only the hardcoded never-touch paths (plus bclean's own config, data,
    /// and cache directories as reported by the platform).
    #[must_use]
    pub fn builtin() -> Self {
        let (exact, mut subtrees) = builtin_paths();
        for dir in [dirs::config_dir(), dirs::data_dir(), dirs::data_local_dir()]
            .into_iter()
            .flatten()
        {
            subtrees.push(dir.join("bclean"));
        }
        Self {
            exact: exact.iter().map(|p| key(p)).collect(),
            subtrees: subtrees.iter().map(|p| key(p)).collect(),
            globs: Vec::new(),
        }
    }

    /// Builds a protector from configuration plus the hardcoded never-touch paths.
    ///
    /// # Errors
    ///
    /// * A pattern in `config.paths` is not a valid glob.
    pub fn from_config(config: &ProtectConfig) -> Result<Self, ProtectError> {
        let mut protector = Self::builtin();
        for pattern in &config.paths {
            protector.add_pattern(pattern)?;
        }
        Ok(protector)
    }

    /// Adds a directory whose whole subtree (and ancestors) must never be touched, for example
    /// the daemon's state directory or the journal's directory.
    #[must_use]
    pub fn with_protected_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.subtrees.push(key(&normalize(dir.as_ref())));
        self
    }

    /// Adds a user pattern (glob with `~` expansion, or a literal path).
    ///
    /// # Errors
    ///
    /// * The pattern is not a valid glob.
    pub fn add_pattern(&mut self, pattern: &str) -> Result<(), ProtectError> {
        let expanded = expand_tilde(pattern);
        let text = expanded.to_string_lossy().replace('\\', "/");
        if !has_glob_meta(&text) {
            self.subtrees.push(key(&normalize(&expanded)));
            return Ok(());
        }
        let full = compile(&text, pattern)?;
        let parts: Vec<&str> = text.split('/').collect();
        let mut prefixes = Vec::new();
        for count in 1..parts.len() {
            let part = parts[count - 1];
            if part == "**" {
                break;
            }
            if !has_glob_meta(part) {
                // Literal prefixes are covered by the ancestor check below.
                continue;
            }
            let prefix = parts[..count].join("/");
            prefixes.push((component_count(&prefix), compile(&prefix, pattern)?));
        }
        let literal: Vec<&str> = parts
            .iter()
            .take_while(|part| !has_glob_meta(part))
            .copied()
            .collect();
        let literal = literal.join("/");
        if !literal.is_empty() {
            // Ancestors of the literal prefix contain potential matches.
            self.exact.push(key(&normalize(Path::new(&literal))));
            for ancestor in normalize(Path::new(&literal)).ancestors().skip(1) {
                self.exact.push(key(ancestor));
            }
        }
        self.globs.push(UserGlob {
            pattern: pattern.to_owned(),
            full,
            prefixes,
        });
        Ok(())
    }

    /// Returns why `path` is protected, or `None` if it may be acted on.
    #[must_use]
    pub fn check(&self, path: &Path) -> Option<String> {
        let lexical = normalize(path);
        let mut forms = vec![lexical.clone()];
        if let Some(canonical) = canonical_parent_form(&lexical)
            && canonical != lexical
        {
            forms.push(canonical);
        }
        forms.iter().find_map(|form| self.check_form(form))
    }

    /// Whether `path` is protected.
    #[must_use]
    pub fn is_protected(&self, path: &Path) -> bool {
        self.check(path).is_some()
    }

    fn check_form(&self, path: &Path) -> Option<String> {
        let k = key(path);
        if k.parent().is_none() || k.file_name().is_none() {
            return Some(format!("{} is a filesystem root", path.display()));
        }
        if let Some(hit) = self.exact.iter().find(|p| **p == k) {
            return Some(format!("{} is a protected location", hit.display()));
        }
        for subtree in &self.subtrees {
            if k.starts_with(subtree) {
                return Some(format!("inside protected path {}", subtree.display()));
            }
            if subtree.starts_with(&k) {
                return Some(format!("contains protected path {}", subtree.display()));
            }
        }
        let text = k.to_string_lossy().replace('\\', "/");
        for glob in &self.globs {
            for ancestor in Path::new(&text).ancestors() {
                if glob.full.is_match(ancestor) {
                    return Some(format!("matches protect pattern `{}`", glob.pattern));
                }
            }
            let count = component_count(&text);
            if glob
                .prefixes
                .iter()
                .any(|(n, m)| *n == count && m.is_match(Path::new(&text)))
            {
                return Some(format!("may contain matches of `{}`", glob.pattern));
            }
        }
        None
    }
}

fn compile(text: &str, original: &str) -> Result<GlobMatcher, ProtectError> {
    GlobBuilder::new(text)
        .literal_separator(true)
        .case_insensitive(cfg!(windows))
        .backslash_escape(!cfg!(windows))
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|source| ProtectError::BadPattern {
            pattern: original.to_owned(),
            source,
        })
}

fn has_glob_meta(text: &str) -> bool {
    text.contains(['*', '?', '[', ']', '{', '}'])
}

fn component_count(text: &str) -> usize {
    Path::new(text).components().count()
}

/// Expands a leading `~` to the home directory.
#[must_use]
pub fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(input));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(input)
}

/// Makes a path absolute and lexically resolves `.` and `..` (without touching the
/// filesystem).
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |cwd| cwd.join(path))
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop past the root/prefix.
                if out.file_name().is_some() {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The path with its parent directory canonicalized (symlinks resolved) but its final
/// component left as-is.
fn canonical_parent_form(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let parent = path.parent()?;
    let canonical = std::fs::canonicalize(parent).ok()?;
    Some(strip_verbatim(&canonical).join(name))
}

/// Removes the Windows `\\?\` verbatim prefix produced by `canonicalize`.
fn strip_verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

/// Comparison key: identity on Unix, lowercase on Windows.
fn key(path: &Path) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path.to_path_buf()
    }
}

/// Hardcoded `(exact, subtree)` never-touch paths for the current platform.
fn builtin_paths() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut exact: Vec<PathBuf> = Vec::new();
    let mut subtrees: Vec<PathBuf> = Vec::new();

    if cfg!(unix) {
        for p in [
            "/",
            "/Users",
            "/home",
            "/Volumes",
            "/mnt",
            "/media",
            "/tmp",
            "/var",
            "/var/tmp",
            "/var/folders",
            "/private",
            "/private/var",
            "/private/var/folders",
            "/private/tmp",
            "/opt",
            "/Applications",
            "/Library",
            "/Library/Caches",
            "/root",
            "/srv",
            "/run",
            "/nix",
        ] {
            exact.push(PathBuf::from(p));
        }
        for p in [
            "/System",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/private/etc",
            "/boot",
            "/dev",
            "/proc",
            "/sys",
            "/lib",
            "/lib32",
            "/lib64",
            "/libx32",
            "/nix/store",
            "/var/lib",
            "/var/db",
            "/private/var/db",
        ] {
            subtrees.push(PathBuf::from(p));
        }
    }

    windows_paths(&mut exact, &mut subtrees);
    home_paths(&mut exact, &mut subtrees);
    (exact, subtrees)
}

fn windows_paths(exact: &mut Vec<PathBuf>, subtrees: &mut Vec<PathBuf>) {
    if cfg!(windows) {
        let env_or = |name: &str, default: &str| {
            std::env::var_os(name).map_or_else(|| PathBuf::from(default), PathBuf::from)
        };
        subtrees.push(env_or("SystemRoot", r"C:\Windows"));
        subtrees.push(env_or("ProgramFiles", r"C:\Program Files"));
        subtrees.push(env_or("ProgramFiles(x86)", r"C:\Program Files (x86)"));
        subtrees.push(env_or("ProgramW6432", r"C:\Program Files"));
        exact.push(env_or("ProgramData", r"C:\ProgramData"));
        exact.push(PathBuf::from(r"C:\Users"));
        if let Some(public) = std::env::var_os("PUBLIC") {
            exact.push(PathBuf::from(public));
        }
    }
}

fn home_paths(exact: &mut Vec<PathBuf>, subtrees: &mut Vec<PathBuf>) {
    if let Some(home) = dirs::home_dir() {
        exact.push(home.clone());
        if let Some(parent) = home.parent() {
            exact.push(parent.to_path_buf());
        }
        for child in [
            "Library",
            "Library/Caches",
            "Library/Application Support",
            "Documents",
            "Desktop",
            "Downloads",
            "Pictures",
            "Music",
            "Movies",
            "Videos",
            "Public",
            ".cache",
            ".config",
            ".local",
            ".local/share",
            "AppData",
            "AppData/Local",
            "AppData/Roaming",
            "AppData/LocalLow",
        ] {
            exact.push(home.join(child));
        }
        for child in [".ssh", ".gnupg", "Library/Keychains", ".password-store"] {
            subtrees.push(home.join(child));
        }
    }
    for dir in [
        dirs::cache_dir(),
        dirs::config_dir(),
        dirs::data_dir(),
        dirs::data_local_dir(),
        dirs::document_dir(),
        dirs::desktop_dir(),
        dirs::download_dir(),
    ]
    .into_iter()
    .flatten()
    {
        exact.push(dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_and_home_are_protected() {
        let protector = Protector::builtin();
        #[cfg(unix)]
        {
            assert!(protector.is_protected(Path::new("/")));
            assert!(protector.is_protected(Path::new("/usr/local/bin")));
            assert!(protector.is_protected(Path::new("/System/Library")));
            assert!(protector.is_protected(Path::new("/tmp/../usr")));
        }
        #[cfg(windows)]
        {
            assert!(protector.is_protected(Path::new(r"C:\")));
            assert!(protector.is_protected(Path::new(r"c:\windows\system32")));
        }
        if let Some(home) = dirs::home_dir() {
            assert!(protector.is_protected(&home));
            assert!(protector.is_protected(&home.join(".ssh/id_ed25519")));
            assert!(protector.is_protected(&home.join("Documents")));
            assert!(!protector.is_protected(&home.join("Documents/project/target")));
        }
    }

    #[test]
    fn ancestors_of_protected_dirs_are_protected() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state/bclean");
        let protector = Protector::builtin().with_protected_dir(&state);
        assert!(protector.is_protected(&state));
        assert!(protector.is_protected(&state.join("journal.jsonl")));
        assert!(protector.is_protected(&dir.path().join("state")));
        assert!(!protector.is_protected(&dir.path().join("other")));
    }

    #[test]
    fn user_globs() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_string_lossy().replace('\\', "/");
        let config = ProtectConfig {
            paths: vec![format!("{base}/work/*/keep"), format!("{base}/literal")],
            ..ProtectConfig::default()
        };
        let protector = Protector::from_config(&config).unwrap();
        let p = dir.path();
        assert!(protector.is_protected(&p.join("work/a/keep")));
        assert!(protector.is_protected(&p.join("work/a/keep/deep/file")));
        // Could contain `keep`.
        assert!(protector.is_protected(&p.join("work/a")));
        assert!(protector.is_protected(&p.join("work")));
        assert!(!protector.is_protected(&p.join("work/a/target")));
        assert!(protector.is_protected(&p.join("literal/x")));
        assert!(!protector.is_protected(&p.join("literally")));
    }

    #[test]
    fn tilde_expansion() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/x"), home.join("x"));
        }
        assert_eq!(expand_tilde("/a/~"), PathBuf::from("/a/~"));
    }

    #[test]
    fn rejects_bad_glob() {
        let config = ProtectConfig {
            paths: vec!["/a/[".to_owned()],
            ..ProtectConfig::default()
        };
        assert!(Protector::from_config(&config).is_err());
    }
}
