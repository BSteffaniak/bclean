#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Layered bclean configuration loading via `bmux_config_composition`.
//!
//! Layers, lowest first (later layers override earlier ones field by field; tables merge
//! recursively, arrays and scalars replace):
//!
//! 1. the built-in catalog (`bclean_catalog`), one in-memory text layer per category
//! 2. the global user file: `$BCLEAN_CONFIG` if set, else `<config dir>/config.toml`
//!    (`~/.config/bclean/config.toml` on macOS/Linux, `%APPDATA%\bclean\config.toml` on Windows)
//! 3. the nearest project `.bclean.toml`, found by walking up from the working directory
//! 4. an explicit `--config` file
//!
//! Because rules are keyed maps, a user layer containing `[rules.cargo_target] auto = false`
//! overrides only that field of the built-in rule. `catalog.enabled = false` and
//! `catalog.disable_categories` remove built-in rules after composition, except rules whose
//! `match` table was redefined by a user layer (those are effectively user rules).

pub mod paths;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use bclean_model::Config;
use bmux_config_composition::{
    DecodeDetail, Document, ErrorKind, ExplicitMode, Layer, LoadContext, Loader, LoaderBuilder,
    PathSegment, Requirement, SearchBoundary, SearchMatches, SearchOrder, SearchPolicy, SourceId,
    TextLocation, TomlCodec,
};

pub use paths::{AppDirs, ExpandError, Expander, Platform};

/// File name of per-project configuration files.
pub const PROJECT_FILE_NAME: &str = ".bclean.toml";

/// Environment variable overriding the global configuration file path.
pub const CONFIG_ENV: &str = "BCLEAN_CONFIG";

/// Maximum number of parent directories searched for a project file.
const PROJECT_SEARCH_DEPTH: usize = 64;

/// Layer-name prefix of catalog layers.
const CATALOG_LAYER_PREFIX: &str = "catalog/";

/// Which kind of layer a source came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerKind {
    /// Built-in schema defaults.
    Defaults,
    /// A built-in catalog category.
    Catalog,
    /// The global user file.
    Global,
    /// A project `.bclean.toml`.
    Project,
    /// An explicit `--config` file.
    Explicit,
}

/// One configuration source that contributed to the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSource {
    /// Layer kind.
    pub kind: LayerKind,
    /// Layer name (`"global"`, `"project"`, `"catalog/rust"`, ...).
    pub layer: String,
    /// File path, for file layers.
    pub path: Option<PathBuf>,
}

impl fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{} ({})", path.display(), self.layer),
            None => f.write_str(&self.layer),
        }
    }
}

/// Error loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The working directory could not be determined.
    #[error("cannot determine the working directory: {0}")]
    CurrentDir(#[source] std::io::Error),
    /// A path template could not be expanded.
    #[error(transparent)]
    Expand(#[from] ExpandError),
    /// A file named by `--config` or `$BCLEAN_CONFIG` does not exist.
    #[error("configuration file {} does not exist", path.display())]
    MissingFile {
        /// The missing file.
        path: PathBuf,
    },
    /// Reading, parsing, or composing a layer failed.
    #[error("{}{kind:?}{}", source_prefix(.origin.as_ref()), location_suffix(*.location))]
    Layer {
        /// Failure category.
        kind: ErrorKind,
        /// Offending source, when known.
        origin: Option<ConfigSource>,
        /// Line/column of a syntax error, when known.
        location: Option<TextLocation>,
    },
    /// The composed document does not match the configuration schema.
    #[error(
        "{}invalid value at `{key}`: {detail}",
        source_prefix(.origin.as_ref())
    )]
    Decode {
        /// Dotted key path of the offending value (`rules.cargo_target.auto`).
        key: String,
        /// What was expected.
        detail: String,
        /// Layer that supplied the offending value, when known.
        origin: Option<ConfigSource>,
    },
    /// A built-in catalog file is invalid (a bug in bclean).
    #[error(transparent)]
    Catalog(#[from] bclean_catalog::CatalogError),
}

fn source_prefix(source: Option<&ConfigSource>) -> String {
    source.map(|s| format!("{s}: ")).unwrap_or_default()
}

fn location_suffix(location: Option<TextLocation>) -> String {
    location
        .map(|l| format!(" at line {}, column {}", l.line, l.column))
        .unwrap_or_default()
}

/// Options controlling which layers are loaded.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    /// Working directory (absolute) used for the project search and relative paths.
    pub cwd: PathBuf,
    /// Home directory and environment snapshot.
    pub expander: Expander,
    /// Include the built-in catalog.
    pub catalog: bool,
    /// Include the global user file.
    pub global: bool,
    /// Search for a project `.bclean.toml` upward from `cwd`.
    pub project: bool,
    /// Explicit `--config` file, applied last (must exist).
    pub explicit: Option<PathBuf>,
}

impl LoadOptions {
    /// Default options for the current process (all layers, no explicit file).
    ///
    /// # Errors
    ///
    /// * [`ConfigError::CurrentDir`] if the working directory cannot be determined
    pub fn from_process() -> Result<Self, ConfigError> {
        Ok(Self::new(
            std::env::current_dir().map_err(ConfigError::CurrentDir)?,
            Expander::from_process(),
        ))
    }

    /// Default options (all layers, no explicit file) for an explicit context.
    #[must_use]
    pub const fn new(cwd: PathBuf, expander: Expander) -> Self {
        Self {
            cwd,
            expander,
            catalog: true,
            global: true,
            project: true,
            explicit: None,
        }
    }

    /// Sets the explicit `--config` file.
    #[must_use]
    pub fn with_explicit(mut self, path: Option<PathBuf>) -> Self {
        self.explicit = path;
        self
    }

    /// The global user file that will be consulted: `$BCLEAN_CONFIG` if set, else the platform
    /// default.
    ///
    /// # Errors
    ///
    /// * [`ConfigError::Expand`] if the home directory is unknown
    pub fn global_path(&self) -> Result<PathBuf, ConfigError> {
        if let Some(path) = self.expander.var(CONFIG_ENV) {
            let path = self.expander.expand_path(path)?;
            return Ok(if path.is_absolute() {
                path
            } else {
                self.cwd.join(path)
            });
        }
        Ok(self.expander.app_dirs()?.config_file())
    }
}

/// A loaded configuration plus provenance.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The composed configuration.
    pub config: Config,
    /// Sources that were present, lowest layer first.
    pub sources: Vec<ConfigSource>,
    /// Built-in rule ids still present in `config.rules`.
    pub builtin_rules: BTreeSet<String>,
    /// Built-in rule ids removed by `catalog.enabled = false` / `disable_categories`.
    pub disabled_builtin_rules: BTreeSet<String>,
    rule_origins: BTreeMap<String, ConfigSource>,
}

impl LoadedConfig {
    /// The layer that last defined any field of rule `id`.
    #[must_use]
    pub fn rule_origin(&self, id: &str) -> Option<&ConfigSource> {
        self.rule_origins.get(id)
    }

    /// Whether rule `id` comes from the built-in catalog.
    #[must_use]
    pub fn is_builtin(&self, id: &str) -> bool {
        self.builtin_rules.contains(id)
    }
}

/// Loads configuration for the current process with default options.
///
/// # Errors
///
/// * Any error from [`LoadOptions::from_process`] or [`load`]
pub fn load_default(explicit: Option<PathBuf>) -> Result<LoadedConfig, ConfigError> {
    load(&LoadOptions::from_process()?.with_explicit(explicit))
}

/// Assembles the ordered layers for `options`.
fn layers(options: &LoadOptions) -> Result<LoaderBuilder, ConfigError> {
    let mut builder = Loader::builder();
    if options.catalog {
        for file in bclean_catalog::FILES {
            builder = builder.layer(
                format!("{CATALOG_LAYER_PREFIX}{}", file.category),
                Layer::Text(file.source.to_owned()),
            );
        }
    }
    if options.global {
        let path = options.global_path()?;
        let requirement = if options.expander.var(CONFIG_ENV).is_some() {
            if !path.is_file() {
                return Err(ConfigError::MissingFile { path });
            }
            Requirement::Required
        } else {
            Requirement::Optional
        };
        builder = builder.layer("global", Layer::File { path, requirement });
    }
    if options.project {
        builder = builder.layer(
            "project",
            Layer::AncestorSearch {
                start: None,
                file_name: PROJECT_FILE_NAME.to_owned(),
                max_depth: PROJECT_SEARCH_DEPTH,
                policy: SearchPolicy {
                    matches: SearchMatches::Nearest,
                    order: SearchOrder::OuterFirst,
                    boundary: SearchBoundary::Filesystem,
                },
            },
        );
    }
    if let Some(explicit) = &options.explicit {
        let path = if explicit.is_absolute() {
            explicit.clone()
        } else {
            options.cwd.join(explicit)
        };
        if !path.is_file() {
            return Err(ConfigError::MissingFile { path });
        }
        builder = builder.explicit_file(path, Requirement::Required, ExplicitMode::Additive);
    }

    Ok(builder)
}

/// Loads and composes all configured layers.
///
/// # Errors
///
/// * [`ConfigError::MissingFile`] if the explicit file or `$BCLEAN_CONFIG` does not exist
/// * [`ConfigError::Layer`] if a file cannot be read or parsed (with line/column for syntax
///   errors), or a resource limit is exceeded
/// * [`ConfigError::Decode`] if the composed document does not match the schema (with the
///   dotted key path and the layer that supplied the offending value)
/// * [`ConfigError::Catalog`] if a built-in catalog file is invalid
/// * [`ConfigError::Expand`] if the home directory is needed but unknown
pub fn load(options: &LoadOptions) -> Result<LoadedConfig, ConfigError> {
    let builder = layers(options)?;
    let layer_err =
        |error: bmux_config_composition::Error, sources: &[ConfigSource]| ConfigError::Layer {
            kind: error.kind,
            origin: error.source.and_then(|id| sources.get(id.0).cloned()),
            location: error.location,
        };

    let loader = builder.build().map_err(|e| layer_err(e, &[]))?;
    let context = LoadContext::new(options.cwd.clone(), options.expander.env().clone())
        .map_err(|e| layer_err(e, &[]))?;
    let captured = loader.capture(&context).map_err(|e| layer_err(e, &[]))?;
    let sources: Vec<ConfigSource> = captured
        .sources()
        .iter()
        .map(|s| ConfigSource {
            kind: layer_kind(&s.layer),
            layer: s.layer.clone(),
            path: s.path.clone(),
        })
        .collect();
    let resolved = captured
        .resolve(&TomlCodec)
        .map_err(|e| layer_err(e, &sources))?;
    let mut config: Config = resolved
        .decode_located()
        .map_err(|failure| ConfigError::Decode {
            key: dotted(&failure.path),
            detail: describe(&failure.detail),
            origin: failure
                .error
                .source
                .and_then(|id| sources.get(id.0).cloned()),
        })?;

    let document = &resolved.document;
    let source_of = |id: SourceId| sources.get(id.0);
    let is_catalog = |id: Option<SourceId>| {
        id.and_then(source_of)
            .is_some_and(|s| s.kind == LayerKind::Catalog)
    };

    let (builtin_rules, disabled_builtin_rules) =
        filter_catalog(&mut config, document, &is_catalog, options.catalog)?;

    let rule_origins = config
        .rules
        .keys()
        .filter_map(|id| {
            let prefix = [
                PathSegment::Key("rules".to_owned()),
                PathSegment::Key(id.clone()),
            ];
            // Highest-layer source among every node of this rule.
            document
                .provenance
                .iter()
                .filter(|(path, _)| path.starts_with(&prefix))
                .map(|(_, source)| *source)
                .max()
                .and_then(source_of)
                .map(|s| (id.clone(), s.clone()))
        })
        .collect();

    Ok(LoadedConfig {
        config,
        sources: sources
            .into_iter()
            .filter(|s| s.kind != LayerKind::Defaults)
            .collect(),
        builtin_rules,
        disabled_builtin_rules,
        rule_origins,
    })
}

/// Removes built-in rules of disabled categories (unless a user layer redefined their
/// `match` table); returns the remaining and the removed built-in rule ids.
fn filter_catalog(
    config: &mut Config,
    document: &Document,
    is_catalog: &dyn Fn(Option<SourceId>) -> bool,
    catalog_loaded: bool,
) -> Result<(BTreeSet<String>, BTreeSet<String>), ConfigError> {
    let catalog_rules: BTreeMap<String, Option<String>> = if catalog_loaded {
        bclean_catalog::all_rules()?
            .into_iter()
            .map(|(id, rule)| (id, rule.category))
            .collect()
    } else {
        BTreeMap::new()
    };
    let disabled: BTreeSet<&str> = config
        .catalog
        .disable_categories
        .iter()
        .map(String::as_str)
        .collect();
    let mut builtin_rules = BTreeSet::new();
    let mut disabled_builtin_rules = BTreeSet::new();
    for (id, category) in &catalog_rules {
        if !config.rules.contains_key(id) {
            continue;
        }
        let match_path = [
            PathSegment::Key("rules".to_owned()),
            PathSegment::Key(id.clone()),
            PathSegment::Key("match".to_owned()),
        ];
        let match_from_catalog = is_catalog(document.origin(&match_path));
        let category_disabled =
            !config.catalog.enabled || category.as_deref().is_some_and(|c| disabled.contains(c));
        if !match_from_catalog {
            // A user layer redefined the matcher: treat it as a user rule.
            continue;
        }
        if category_disabled {
            config.rules.remove(id);
            disabled_builtin_rules.insert(id.clone());
        } else {
            builtin_rules.insert(id.clone());
        }
    }

    Ok((builtin_rules, disabled_builtin_rules))
}

/// Parses a single configuration document (no layering, no catalog).
///
/// # Errors
///
/// * [`ConfigError::Layer`] with a line/column on TOML syntax errors
/// * [`ConfigError::Decode`] with the dotted key path on schema errors
pub fn parse_str(text: &str) -> Result<Config, ConfigError> {
    let value = TomlCodec::parse(text).map_err(|e| ConfigError::Layer {
        kind: e.kind,
        origin: None,
        location: e.location,
    })?;
    bmux_config_composition::Document::new(value, SourceId(0))
        .decode_located()
        .map_err(|failure| ConfigError::Decode {
            key: dotted(&failure.path),
            detail: describe(&failure.detail),
            origin: None,
        })
}

fn layer_kind(layer: &str) -> LayerKind {
    match layer {
        "defaults" => LayerKind::Defaults,
        "global" => LayerKind::Global,
        "explicit" => LayerKind::Explicit,
        l if l.starts_with(CATALOG_LAYER_PREFIX) => LayerKind::Catalog,
        _ => LayerKind::Project,
    }
}

fn dotted(path: &[PathSegment]) -> String {
    let mut out = String::new();
    for segment in path {
        match segment {
            PathSegment::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            PathSegment::Index(i) => {
                out.push('[');
                out.push_str(&i.to_string());
                out.push(']');
            }
        }
    }
    if out.is_empty() {
        out.push_str("<root>");
    }
    out
}

fn describe(detail: &DecodeDetail) -> String {
    match detail {
        DecodeDetail::UnknownField { field, expected } => {
            format!("unknown field `{field}`, expected one of {expected:?}")
        }
        DecodeDetail::MissingField { field } => format!("missing field `{field}`"),
        DecodeDetail::InvalidType { expected } => format!("invalid type, expected {expected}"),
        DecodeDetail::InvalidValue { expected } => format!("invalid value, expected {expected}"),
        DecodeDetail::InvalidLength { expected } => format!("invalid length, expected {expected}"),
        DecodeDetail::UnknownVariant { expected } => {
            format!("unknown variant, expected one of {expected:?}")
        }
        _ => "invalid value".to_owned(),
    }
}

/// Whether `path` looks like a project configuration file name.
#[must_use]
pub fn is_project_file(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == PROJECT_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let cwd = home.join("proj").join("sub");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(home.join(".config").join("bclean")).unwrap();
        Fixture {
            _dir: dir,
            home,
            cwd,
        }
    }

    fn options(f: &Fixture, env: &[(&str, String)]) -> LoadOptions {
        let mut vars: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        vars.insert("HOME".to_owned(), f.home.display().to_string());
        // Force the XDG-style layout on every host so the fixture is portable.
        vars.insert(
            "XDG_CONFIG_HOME".to_owned(),
            f.home.join(".config").display().to_string(),
        );
        vars.insert(
            "APPDATA".to_owned(),
            f.home.join(".config").display().to_string(),
        );
        LoadOptions::new(f.cwd.clone(), Expander::new(Some(f.home.clone()), vars))
    }

    fn global_file(f: &Fixture) -> PathBuf {
        f.home.join(".config").join("bclean").join("config.toml")
    }

    fn some_builtin() -> (String, bclean_model::RuleSpec) {
        bclean_catalog::all_rules()
            .unwrap()
            .into_iter()
            .next()
            .expect("catalog has rules")
    }

    #[test]
    fn defaults_without_any_files() {
        let f = fixture();
        let loaded = load(&options(&f, &[])).unwrap();
        assert_eq!(
            loaded.config.classify,
            bclean_model::ClassifyConfig::default()
        );
        assert_eq!(
            loaded.config.rules.len(),
            bclean_catalog::all_rules().unwrap().len()
        );
        assert!(loaded.sources.iter().all(|s| s.kind == LayerKind::Catalog));
    }

    #[test]
    fn user_layer_overrides_single_builtin_field() {
        let f = fixture();
        let (id, builtin) = some_builtin();
        fs::write(
            global_file(&f),
            format!("[rules.{id}]\nauto = {}\npriority = 77\n", !builtin.auto),
        )
        .unwrap();
        let loaded = load(&options(&f, &[])).unwrap();
        let rule = &loaded.config.rules[&id];
        assert_eq!(rule.auto, !builtin.auto);
        assert_eq!(rule.priority, 77);
        assert_eq!(rule.matcher, builtin.matcher);
        assert_eq!(rule.action, builtin.action);
        assert_eq!(rule.ephemeral, builtin.ephemeral);
        assert!(loaded.is_builtin(&id));
        assert_eq!(loaded.rule_origin(&id).unwrap().kind, LayerKind::Global);
    }

    #[test]
    fn layer_precedence_global_project_explicit() {
        let f = fixture();
        fs::write(
            global_file(&f),
            "[classify]\nstale_after = \"10d\"\ndurable_min_size = \"1G\"\n",
        )
        .unwrap();
        fs::write(
            f.home.join("proj").join(PROJECT_FILE_NAME),
            "[classify]\nstale_after = \"20d\"\n",
        )
        .unwrap();
        let loaded = load(&options(&f, &[])).unwrap();
        assert_eq!(loaded.config.classify.stale_after.as_secs(), 20 * 86_400);
        assert_eq!(loaded.config.classify.durable_min_size.bytes(), 1 << 30);
        assert!(loaded.sources.iter().any(|s| s.kind == LayerKind::Project));

        let explicit = f.cwd.join("explicit.toml");
        fs::write(&explicit, "[classify]\nstale_after = \"5d\"\n").unwrap();
        let loaded =
            load(&options(&f, &[]).with_explicit(Some(PathBuf::from("explicit.toml")))).unwrap();
        assert_eq!(loaded.config.classify.stale_after.as_secs(), 5 * 86_400);
        assert_eq!(loaded.config.classify.durable_min_size.bytes(), 1 << 30);
    }

    #[test]
    fn env_override_replaces_global_path() {
        let f = fixture();
        fs::write(global_file(&f), "[classify]\nstale_after = \"10d\"\n").unwrap();
        let other = f.home.join("other.toml");
        fs::write(&other, "[classify]\nstale_after = \"3d\"\n").unwrap();
        let loaded = load(&options(&f, &[(CONFIG_ENV, other.display().to_string())])).unwrap();
        assert_eq!(loaded.config.classify.stale_after.as_secs(), 3 * 86_400);

        let missing = f.home.join("missing.toml");
        let err = load(&options(&f, &[(CONFIG_ENV, missing.display().to_string())])).unwrap_err();
        assert!(matches!(err, ConfigError::MissingFile { .. }), "{err}");
    }

    #[test]
    fn catalog_disable_respected() {
        let f = fixture();
        let (id, builtin) = some_builtin();
        let category = builtin.category.unwrap();
        fs::write(
            global_file(&f),
            format!(
                "[catalog]\ndisable_categories = [\"{category}\"]\n[rules.{id}]\nauto = true\n"
            ),
        )
        .unwrap();
        let loaded = load(&options(&f, &[])).unwrap();
        assert!(!loaded.config.rules.contains_key(&id));
        assert!(loaded.disabled_builtin_rules.contains(&id));
        assert!(
            loaded
                .config
                .rules
                .values()
                .all(|r| r.category.as_deref() != Some(category.as_str()))
        );

        fs::write(
            global_file(&f),
            "[catalog]\nenabled = false\n[rules.mine]\nmatch = { name = [\"junk\"] }\n",
        )
        .unwrap();
        let loaded = load(&options(&f, &[])).unwrap();
        assert_eq!(loaded.config.rules.keys().collect::<Vec<_>>(), vec!["mine"]);
        assert!(loaded.builtin_rules.is_empty());
    }

    #[test]
    fn user_redefined_matcher_survives_disabled_category() {
        let f = fixture();
        let (id, builtin) = some_builtin();
        let category = builtin.category.unwrap();
        fs::write(
            global_file(&f),
            format!(
                "[catalog]\ndisable_categories = [\"{category}\"]\n[rules.{id}.match]\nname = [\"x\"]\n"
            ),
        )
        .unwrap();
        let loaded = load(&options(&f, &[])).unwrap();
        assert_eq!(loaded.config.rules[&id].matcher.name, vec!["x"]);
        assert!(!loaded.is_builtin(&id));
    }

    #[test]
    fn syntax_error_has_location_and_source() {
        let f = fixture();
        fs::write(global_file(&f), "[classify]\nstale_after = \n").unwrap();
        let err = load(&options(&f, &[])).unwrap_err();
        match &err {
            ConfigError::Layer {
                kind,
                origin,
                location,
            } => {
                assert_eq!(*kind, ErrorKind::Parse);
                assert_eq!(origin.as_ref().unwrap().kind, LayerKind::Global);
                assert_eq!(location.unwrap().line, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn decode_error_has_key_path() {
        let f = fixture();
        fs::write(global_file(&f), "[rules.foo]\nauto = \"yes\"\n").unwrap();
        let err = load(&options(&f, &[])).unwrap_err();
        match &err {
            ConfigError::Decode { key, origin, .. } => {
                assert_eq!(key, "rules.foo.auto");
                assert_eq!(origin.as_ref().unwrap().kind, LayerKind::Global);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_explicit_file_errors() {
        let f = fixture();
        let err =
            load(&options(&f, &[]).with_explicit(Some(PathBuf::from("nope.toml")))).unwrap_err();
        assert!(matches!(err, ConfigError::MissingFile { .. }));
    }

    #[test]
    fn parse_str_reports_key() {
        let err = parse_str("[classify]\ncachedir_tag = 3\n").unwrap_err();
        assert!(
            matches!(err, ConfigError::Decode { ref key, .. } if key == "classify.cachedir_tag")
        );
        assert!(parse_str("[scan]\nthreads = 4\n").is_ok());
    }
}
