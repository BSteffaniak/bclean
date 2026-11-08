#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Built-in bclean rule catalog of known ephemeral and cache directories.
//!
//! Every category lives in its own embedded TOML file under `packages/catalog/rules/`, written
//! in exactly the same format as a user configuration's `[rules.<id>]` tables. The files are
//! the lowest configuration layer: `bclean_config` composes user layers on top of them with
//! keyed-override semantics, so `[rules.cargo_target] auto = false` in a user file tweaks only
//! that field of the built-in rule.
//!
//! Every rule in a file must set `category` to the file's category name.

use std::collections::BTreeMap;

use bclean_model::{Config, RuleSpec};

/// One embedded catalog file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogFile {
    /// Category name (the file stem, for example `"rust"`).
    pub category: &'static str,
    /// Raw TOML source.
    pub source: &'static str,
}

macro_rules! catalog_files {
    ($($category:literal),* $(,)?) => {
        /// Every embedded catalog file, in a stable order.
        pub const FILES: &[CatalogFile] = &[
            $(CatalogFile {
                category: $category,
                source: include_str!(concat!("../rules/", $category, ".toml")),
            },)*
        ];
    };
}

catalog_files![
    "bazel",
    "browsers",
    "cmake",
    "containers",
    "dart",
    "elixir",
    "go",
    "haskell",
    "homebrew",
    "ide",
    "jvm",
    "nix",
    "node",
    "os",
    "python",
    "rust",
    "terraform",
    "trash",
    "xcode",
    "zig",
];

/// Error decoding an embedded catalog file.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The file is not valid TOML or does not match the configuration schema.
    #[error("built-in catalog file `{category}.toml` is invalid: {source}")]
    Parse {
        /// Category of the offending file.
        category: &'static str,
        /// Underlying TOML error (with line and column).
        #[source]
        source: toml::de::Error,
    },
    /// A rule's `category` does not match its file.
    #[error("built-in rule `{rule}` in `{category}.toml` has category {found:?}")]
    CategoryMismatch {
        /// Category of the file.
        category: &'static str,
        /// Offending rule id.
        rule: String,
        /// Category the rule declared.
        found: Option<String>,
    },
    /// The same rule id is defined in two files.
    #[error("built-in rule `{rule}` is defined in both `{first}.toml` and `{second}.toml`")]
    DuplicateRule {
        /// Duplicated rule id.
        rule: String,
        /// First file defining it.
        first: &'static str,
        /// Second file defining it.
        second: &'static str,
    },
}

impl CatalogFile {
    /// Decodes this file's rules.
    ///
    /// # Errors
    ///
    /// * [`CatalogError::Parse`] if the TOML is malformed or does not match the schema
    /// * [`CatalogError::CategoryMismatch`] if a rule's `category` differs from the file's
    pub fn rules(&self) -> Result<BTreeMap<String, RuleSpec>, CatalogError> {
        let config: Config = toml::from_str(self.source).map_err(|source| CatalogError::Parse {
            category: self.category,
            source,
        })?;
        for (id, rule) in &config.rules {
            if rule.category.as_deref() != Some(self.category) {
                return Err(CatalogError::CategoryMismatch {
                    category: self.category,
                    rule: id.clone(),
                    found: rule.category.clone(),
                });
            }
        }
        Ok(config.rules)
    }
}

/// Names of every built-in category.
#[must_use]
pub fn categories() -> Vec<&'static str> {
    FILES.iter().map(|f| f.category).collect()
}

/// Returns the catalog file for `category`, if it exists.
#[must_use]
pub fn file(category: &str) -> Option<&'static CatalogFile> {
    FILES.iter().find(|f| f.category == category)
}

/// Decodes every built-in rule, keyed by id.
///
/// # Errors
///
/// * Any error from [`CatalogFile::rules`]
/// * [`CatalogError::DuplicateRule`] if two files define the same rule id
pub fn all_rules() -> Result<BTreeMap<String, RuleSpec>, CatalogError> {
    let mut out: BTreeMap<String, RuleSpec> = BTreeMap::new();
    let mut owner: BTreeMap<String, &'static str> = BTreeMap::new();
    for file in FILES {
        for (id, rule) in file.rules()? {
            if let Some(first) = owner.insert(id.clone(), file.category) {
                return Err(CatalogError::DuplicateRule {
                    rule: id,
                    first,
                    second: file.category,
                });
            }
            out.insert(id, rule);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_decodes() {
        for file in FILES {
            let rules = file.rules().unwrap_or_else(|e| panic!("{e}"));
            assert!(!rules.is_empty(), "{} has no rules", file.category);
        }
        all_rules().unwrap();
    }

    /// The schema does not deny unknown fields, so catch typos here.
    #[test]
    fn no_unknown_keys() {
        const RULE_KEYS: &[&str] = &[
            "enabled",
            "description",
            "category",
            "match",
            "ephemeral",
            "stale",
            "action",
            "auto",
            "priority",
            "confidence",
            "keep",
        ];
        const MATCH_KEYS: &[&str] = &[
            "kind",
            "name",
            "path",
            "sibling_any",
            "sibling_all",
            "contains_any",
            "not_under",
            "os",
            "git_ignored",
        ];
        const STALE_KEYS: &[&str] = &["after", "signals", "project_idle"];
        const EPHEMERAL_KEYS: &[&str] = &["kind", "regen_cost"];
        const ACTION_KEYS: &[&str] = &["kind", "run", "cwd", "fallback", "dest"];
        for file in FILES {
            let doc: toml::Table = toml::from_str(file.source).unwrap();
            assert_eq!(
                doc.keys().collect::<Vec<_>>(),
                vec!["rules"],
                "{}",
                file.category
            );
            for (id, rule) in doc["rules"].as_table().unwrap() {
                let rule = rule.as_table().unwrap();
                for (key, value) in rule {
                    assert!(RULE_KEYS.contains(&key.as_str()), "{id}: unknown key {key}");
                    let allowed = match key.as_str() {
                        "match" => MATCH_KEYS,
                        "stale" => STALE_KEYS,
                        "ephemeral" => EPHEMERAL_KEYS,
                        "action" => ACTION_KEYS,
                        _ => continue,
                    };
                    for sub in value.as_table().unwrap().keys() {
                        assert!(
                            allowed.contains(&sub.as_str()),
                            "{id}.{key}: unknown key {sub}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rules_are_well_formed_and_safe() {
        use bclean_model::{ActionSpec, Confidence, EphemeralKind};

        for (id, rule) in all_rules().unwrap() {
            assert!(rule.description.is_some(), "{id}: missing description");
            assert!(
                !rule.matcher.name.is_empty() || !rule.matcher.path.is_empty(),
                "{id}: needs `name` or `path`"
            );
            if rule.auto {
                assert!(
                    rule.ephemeral.is_some(),
                    "{id}: auto rule must be ephemeral"
                );
                assert_eq!(rule.confidence, Confidence::High, "{id}: auto needs HIGH");
                assert!(
                    !matches!(rule.action, ActionSpec::Report),
                    "{id}: auto REPORT is pointless"
                );
            }
            if rule.ephemeral.is_none() {
                assert!(
                    matches!(rule.action, ActionSpec::Report),
                    "{id}: durable must REPORT"
                );
            }
            if let Some(eph) = rule.ephemeral
                && matches!(eph.kind, EphemeralKind::Container | EphemeralKind::VmImage)
            {
                assert!(
                    matches!(rule.action, ActionSpec::Report | ActionSpec::Command { .. }),
                    "{id}: never delete container/VM images directly"
                );
                if let ActionSpec::Command { fallback, .. } = rule.action {
                    assert_eq!(fallback, bclean_model::FallbackAction::None, "{id}");
                }
            }
        }
    }
}
