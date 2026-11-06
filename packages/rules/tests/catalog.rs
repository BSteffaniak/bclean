//! Every built-in catalog file must decode and compile, alone and composed.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use bclean_config::Expander;
use bclean_model::Config;
use bclean_rules::{ChildEntry, Contents, Probe, RuleSet};
use bclean_tree::EntryKind;

fn expander() -> Expander {
    let mut env = BTreeMap::new();
    for (k, v) in [
        ("LOCALAPPDATA", "C:/Users/u/AppData/Local"),
        ("APPDATA", "C:/Users/u/AppData/Roaming"),
        ("SystemRoot", "C:/Windows"),
        ("USERPROFILE", "C:/Users/u"),
    ] {
        env.insert(k.to_owned(), v.to_owned());
    }
    Expander::new(Some(PathBuf::from("/home/u")), env)
}

#[test]
fn every_builtin_file_compiles() {
    assert!(!bclean_catalog::FILES.is_empty());
    for file in bclean_catalog::FILES {
        let rules = file.rules().unwrap_or_else(|e| panic!("{e}"));
        let config = Config {
            rules,
            ..Config::default()
        };
        let set = RuleSet::compile(&config, &expander())
            .unwrap_or_else(|e| panic!("{}.toml: {e}", file.category));
        assert!(
            set.warnings().is_empty(),
            "{}.toml: {:?}",
            file.category,
            set.warnings()
        );
    }
    let config = Config {
        rules: bclean_catalog::all_rules().unwrap(),
        ..Config::default()
    };
    RuleSet::compile(&config, &expander()).unwrap();
}

#[test]
fn composed_default_config_compiles() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let mut env = BTreeMap::new();
    env.insert("HOME".to_owned(), home.display().to_string());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        home.join(".config").display().to_string(),
    );
    env.insert(
        "APPDATA".to_owned(),
        home.join(".config").display().to_string(),
    );
    let exp = Expander::new(Some(home.clone()), env);
    let loaded = bclean_config::load(&bclean_config::LoadOptions::new(home, exp.clone())).unwrap();
    RuleSet::compile(&loaded.config, &exp).unwrap();
}

fn winner(set: &RuleSet, dir: &str, name: &str, siblings: &[&str]) -> Option<String> {
    let mut entries: Vec<ChildEntry<'_>> = siblings
        .iter()
        .map(|n| ChildEntry {
            name: OsStr::new(n),
            kind: if n.contains('.') {
                EntryKind::File
            } else {
                EntryKind::Dir
            },
        })
        .collect();
    entries.push(ChildEntry {
        name: OsStr::new(name),
        kind: EntryKind::Dir,
    });
    let probe = Probe {
        dir: Path::new(dir),
        name: OsStr::new(name),
        kind: EntryKind::Dir,
        siblings: &entries,
        contents: Contents::Fs,
    };
    set.match_entry(&probe, false)
        .map(|r| set.rule(r.winner).id().to_owned())
}

#[test]
fn builtin_spot_checks() {
    let config = Config {
        rules: bclean_catalog::all_rules().unwrap(),
        ..Config::default()
    };
    let set = RuleSet::compile(&config, &expander()).unwrap();
    assert_eq!(
        winner(&set, "/w/proj", "target", &["Cargo.toml", "src"]).as_deref(),
        Some("cargo_target")
    );
    assert_eq!(winner(&set, "/w/proj", "target", &["README.md"]), None);
    assert_eq!(
        winner(&set, "/w/web", "node_modules", &["package.json"]).as_deref(),
        Some("node_modules")
    );
    assert_eq!(
        winner(&set, "/w/py/pkg", "__pycache__", &["mod.py"]).as_deref(),
        Some("python_pycache")
    );
    if cfg!(target_os = "macos") {
        assert_eq!(
            winner(&set, "/home/u/Library/Caches", "Homebrew", &["pip"]).as_deref(),
            Some("homebrew_cache")
        );
        assert!(winner(&set, "/home/u/Library/Caches", "SomeRandomApp", &[]).is_some());
    }
}
