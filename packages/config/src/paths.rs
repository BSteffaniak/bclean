//! Path helpers: `~` and environment-variable expansion, plus per-platform application
//! directories (configuration, data, cache, and daemon runtime locations).
//!
//! Everything here is computed from an explicit [`Expander`] (home directory + environment
//! snapshot) so it is deterministic in tests; [`Expander::from_process`] captures the real
//! process state.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Application directory name used under every base directory.
pub const APP_NAME: &str = "bclean";

/// Error expanding a path template.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// `~` was used but the home directory is unknown.
    #[error("cannot expand `~` in `{input}`: home directory is unknown")]
    NoHome {
        /// The template being expanded.
        input: String,
    },
    /// A referenced environment variable is not set.
    #[error("cannot expand `{input}`: environment variable `{variable}` is not set")]
    UnsetVariable {
        /// The template being expanded.
        input: String,
        /// The missing variable.
        variable: String,
    },
}

/// Expands `~`, `$VAR`, `${VAR}`, and `%VAR%` in path templates.
///
/// * `~` and `~/...` (or `~\...`) expand to the home directory. `~user` is left untouched.
/// * `$VAR` / `${VAR}` expand on every platform; `%VAR%` expands on every platform too so that
///   Windows-style catalog paths can be expanded (and tested) anywhere.
/// * Variable lookup falls back to a case-insensitive match (Windows variables are
///   case-insensitive), and `HOME` / `USERPROFILE` fall back to the known home directory.
/// * A lone `$` or `%` that does not start a valid reference is kept literally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expander {
    home: Option<PathBuf>,
    env: BTreeMap<String, String>,
}

impl Expander {
    /// Creates an expander from an explicit home directory and environment.
    #[must_use]
    pub const fn new(home: Option<PathBuf>, env: BTreeMap<String, String>) -> Self {
        Self { home, env }
    }

    /// Captures the current process's home directory and environment.
    #[must_use]
    pub fn from_process() -> Self {
        Self::new(dirs::home_dir(), std::env::vars().collect())
    }

    /// The home directory, if known.
    #[must_use]
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// The captured environment.
    #[must_use]
    pub const fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Looks up an environment variable (exact match first, then case-insensitive).
    /// Empty values are treated as unset.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<&str> {
        let value = self.env.get(name).map(String::as_str).or_else(|| {
            self.env
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        });
        match value {
            Some(v) if !v.is_empty() => Some(v),
            _ if name.eq_ignore_ascii_case("HOME") || name.eq_ignore_ascii_case("USERPROFILE") => {
                self.home.as_deref().and_then(Path::to_str)
            }
            _ => None,
        }
    }

    fn var_path(&self, name: &str) -> Option<PathBuf> {
        self.var(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    /// Expands a template into a string.
    ///
    /// # Errors
    ///
    /// * [`ExpandError::NoHome`] if `~` is used and the home directory is unknown
    /// * [`ExpandError::UnsetVariable`] if a referenced variable is not set
    pub fn expand(&self, input: &str) -> Result<String, ExpandError> {
        self.expand_escaped(input, &|value| value.to_owned())
    }

    /// Expands a template, passing every substituted value (home directory and variable
    /// values, never the template's own text) through `escape`. Used to build glob patterns
    /// whose substituted parts must match literally.
    ///
    /// # Errors
    ///
    /// * [`ExpandError::NoHome`] if `~` is used and the home directory is unknown
    /// * [`ExpandError::UnsetVariable`] if a referenced variable is not set
    pub fn expand_escaped(
        &self,
        input: &str,
        escape: &dyn Fn(&str) -> String,
    ) -> Result<String, ExpandError> {
        let mut out = String::with_capacity(input.len() + 16);
        let mut rest = input;
        if let Some(after) = rest.strip_prefix('~')
            && (after.is_empty() || after.starts_with('/') || after.starts_with('\\'))
        {
            let home =
                self.home
                    .as_deref()
                    .and_then(Path::to_str)
                    .ok_or_else(|| ExpandError::NoHome {
                        input: input.to_owned(),
                    })?;
            out.push_str(&escape(home.trim_end_matches(['/', '\\'])));
            rest = after;
        }
        let bytes = rest.as_bytes();
        let mut i = 0;
        let mut literal_start = 0;
        while i < bytes.len() {
            let reference = match bytes[i] {
                b'$' => parse_dollar(rest, i),
                b'%' => parse_percent(rest, i),
                _ => None,
            };
            if let Some((name, end)) = reference {
                out.push_str(&rest[literal_start..i]);
                let value = self.var(name).ok_or_else(|| ExpandError::UnsetVariable {
                    input: input.to_owned(),
                    variable: name.to_owned(),
                })?;
                out.push_str(&escape(value));
                i = end;
                literal_start = end;
            } else {
                i += 1;
            }
        }
        out.push_str(&rest[literal_start..]);
        Ok(out)
    }

    /// Expands a template into a path.
    ///
    /// # Errors
    ///
    /// * Any error from [`Expander::expand`]
    pub fn expand_path(&self, input: &str) -> Result<PathBuf, ExpandError> {
        self.expand(input).map(PathBuf::from)
    }

    /// Resolves the per-platform application directories.
    ///
    /// # Errors
    ///
    /// * [`ExpandError::NoHome`] if the home directory is unknown and no override variable
    ///   provides the needed base directory
    pub fn app_dirs(&self) -> Result<AppDirs, ExpandError> {
        AppDirs::resolve(self, Platform::current())
    }
}

const fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

const fn is_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Parses `$NAME` or `${NAME}` at `start`; returns the name and the end offset.
fn parse_dollar(s: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = s.as_bytes();
    let next = *bytes.get(start + 1)?;
    if next == b'{' {
        let close = s[start + 2..].find('}')? + start + 2;
        let name = &s[start + 2..close];
        let valid =
            name.bytes().next().is_some_and(is_name_start) && name.bytes().all(is_name_char);
        return valid.then_some((name, close + 1));
    }
    if !is_name_start(next) {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && is_name_char(bytes[end]) {
        end += 1;
    }
    Some((&s[start + 1..end], end))
}

/// Parses `%NAME%` at `start`; returns the name and the end offset.
fn parse_percent(s: &str, start: usize) -> Option<(&str, usize)> {
    let close = s[start + 1..].find('%')? + start + 1;
    let name = &s[start + 1..close];
    let valid = name.bytes().next().is_some_and(is_name_start)
        && name
            .bytes()
            .all(|b| is_name_char(b) || b == b'(' || b == b')');
    valid.then_some((name, close + 1))
}

/// Target platform for directory conventions (exposed for testing other platforms' layouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS.
    Macos,
    /// Linux and other Unix-likes (XDG conventions).
    Linux,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was compiled for (non-macOS, non-Windows map to `Linux`).
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// Per-platform application directories.
///
/// | dir       | macOS                                   | Linux                                   | Windows                          |
/// | --------- | --------------------------------------- | --------------------------------------- | -------------------------------- |
/// | `config`  | `$XDG_CONFIG_HOME/bclean` or `~/.config/bclean` | same                            | `%APPDATA%\bclean`               |
/// | `data`    | `~/Library/Application Support/bclean`  | `$XDG_DATA_HOME/bclean` or `~/.local/share/bclean` | `%LOCALAPPDATA%\bclean\data` |
/// | `cache`   | `~/Library/Caches/bclean`               | `$XDG_CACHE_HOME/bclean` or `~/.cache/bclean` | `%LOCALAPPDATA%\bclean\cache` |
/// | `runtime` | `<data>/run`                            | `$XDG_RUNTIME_DIR/bclean` or `<data>/run` | `%LOCALAPPDATA%\bclean\run`    |
///
/// The configuration directory deliberately uses `~/.config` on macOS (like most CLI tools)
/// rather than `~/Library/Application Support`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// Platform the layout was resolved for.
    pub platform: Platform,
    /// Configuration directory (holds `config.toml`).
    pub config: PathBuf,
    /// Persistent data (index snapshots, history, action journal).
    pub data: PathBuf,
    /// Disposable caches.
    pub cache: PathBuf,
    /// Runtime files (daemon socket, pid/lock files).
    pub runtime: PathBuf,
    user: String,
}

impl AppDirs {
    /// Resolves the layout for `platform` from the expander's home and environment.
    ///
    /// # Errors
    ///
    /// * [`ExpandError::NoHome`] if a needed base directory cannot be determined
    pub fn resolve(expander: &Expander, platform: Platform) -> Result<Self, ExpandError> {
        let no_home = || ExpandError::NoHome {
            input: "<app dirs>".to_owned(),
        };
        let home = || expander.home().map(Path::to_path_buf).ok_or_else(no_home);
        let user = expander
            .var("USER")
            .or_else(|| expander.var("USERNAME"))
            .unwrap_or("user")
            .to_owned();
        let dirs = match platform {
            Platform::Windows => {
                let roaming = expander
                    .var_path("APPDATA")
                    .map_or_else(|| home().map(|h| h.join("AppData").join("Roaming")), Ok)?;
                let local = expander
                    .var_path("LOCALAPPDATA")
                    .map_or_else(|| home().map(|h| h.join("AppData").join("Local")), Ok)?
                    .join(APP_NAME);
                Self {
                    platform,
                    config: roaming.join(APP_NAME),
                    data: local.join("data"),
                    cache: local.join("cache"),
                    runtime: local.join("run"),
                    user,
                }
            }
            Platform::Macos | Platform::Linux => {
                let config = expander
                    .var_path("XDG_CONFIG_HOME")
                    .map_or_else(|| home().map(|h| h.join(".config")), Ok)?
                    .join(APP_NAME);
                let (data, cache) = if platform == Platform::Macos {
                    let library = home()?.join("Library");
                    (
                        library.join("Application Support").join(APP_NAME),
                        library.join("Caches").join(APP_NAME),
                    )
                } else {
                    (
                        expander
                            .var_path("XDG_DATA_HOME")
                            .map_or_else(|| home().map(|h| h.join(".local").join("share")), Ok)?
                            .join(APP_NAME),
                        expander
                            .var_path("XDG_CACHE_HOME")
                            .map_or_else(|| home().map(|h| h.join(".cache")), Ok)?
                            .join(APP_NAME),
                    )
                };
                let runtime = match (platform, expander.var_path("XDG_RUNTIME_DIR")) {
                    (Platform::Linux, Some(dir)) => dir.join(APP_NAME),
                    _ => data.join("run"),
                };
                Self {
                    platform,
                    config,
                    data,
                    cache,
                    runtime,
                    user,
                }
            }
        };
        Ok(dirs)
    }

    /// The global user configuration file (`<config>/config.toml`).
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// The persistent index database path.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.data.join("index.sqlite")
    }

    /// The action journal path.
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.data.join("journal.sqlite")
    }

    /// The daemon IPC endpoint: a Unix socket path, or a named pipe path on Windows
    /// (`\\.\pipe\bclean-<user>`, with non-alphanumeric user characters replaced by `_`; this
    /// must match `bclean_protocol::Endpoint::default_for`).
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        match self.platform {
            Platform::Windows => PathBuf::from(format!(
                r"\\.\pipe\{APP_NAME}-{}",
                pipe_safe_user(&self.user)
            )),
            Platform::Macos | Platform::Linux => self.runtime.join(format!("{APP_NAME}.sock")),
        }
    }
}

/// User name made safe for a named-pipe name (same rule as `bclean_protocol`).
fn pipe_safe_user(user: &str) -> String {
    user.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expander(vars: &[(&str, &str)]) -> Expander {
        Expander::new(
            Some(PathBuf::from("/home/u")),
            vars.iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    #[test]
    fn expands_tilde_and_vars() {
        let e = expander(&[
            ("CACHE", "/c"),
            ("LocalAppData", "C:/Users/u/AppData/Local"),
        ]);
        assert_eq!(e.expand("~").unwrap(), "/home/u");
        assert_eq!(e.expand("~/x/y").unwrap(), "/home/u/x/y");
        assert_eq!(e.expand("~user/x").unwrap(), "~user/x");
        assert_eq!(e.expand("$CACHE/a").unwrap(), "/c/a");
        assert_eq!(e.expand("${CACHE}b").unwrap(), "/cb");
        assert_eq!(
            e.expand("%LOCALAPPDATA%/Temp").unwrap(),
            "C:/Users/u/AppData/Local/Temp"
        );
        assert_eq!(e.expand("$HOME/z").unwrap(), "/home/u/z");
        assert_eq!(e.expand("100% $ sure").unwrap(), "100% $ sure");
        assert_eq!(e.expand("a$1").unwrap(), "a$1");
    }

    #[test]
    fn unset_variables_error() {
        let e = expander(&[]);
        assert_eq!(
            e.expand("$NOPE/x"),
            Err(ExpandError::UnsetVariable {
                input: "$NOPE/x".to_owned(),
                variable: "NOPE".to_owned()
            })
        );
        let no_home = Expander::new(None, BTreeMap::new());
        assert!(matches!(
            no_home.expand("~/x"),
            Err(ExpandError::NoHome { .. })
        ));
    }

    #[test]
    fn app_dirs_per_platform() {
        let e = expander(&[("XDG_RUNTIME_DIR", "/run/user/1"), ("USER", "u")]);
        let linux = AppDirs::resolve(&e, Platform::Linux).unwrap();
        assert_eq!(
            linux.config_file(),
            PathBuf::from("/home/u/.config/bclean/config.toml")
        );
        assert_eq!(linux.data, PathBuf::from("/home/u/.local/share/bclean"));
        assert_eq!(
            linux.socket_path(),
            PathBuf::from("/run/user/1/bclean/bclean.sock")
        );

        let mac = AppDirs::resolve(&e, Platform::Macos).unwrap();
        assert_eq!(mac.config, PathBuf::from("/home/u/.config/bclean"));
        assert_eq!(
            mac.data,
            PathBuf::from("/home/u/Library/Application Support/bclean")
        );
        assert!(mac.socket_path().starts_with(&mac.data));

        let w = expander(&[
            ("APPDATA", "/w/Roaming"),
            ("LOCALAPPDATA", "/w/Local"),
            ("USERNAME", "bob"),
        ]);
        let win = AppDirs::resolve(&w, Platform::Windows).unwrap();
        assert_eq!(win.config, PathBuf::from("/w/Roaming/bclean"));
        assert_eq!(
            win.index_path(),
            PathBuf::from("/w/Local/bclean/data/index.sqlite")
        );
        assert_eq!(win.socket_path(), PathBuf::from(r"\\.\pipe\bclean-bob"));
    }
}
