//! Rule compilation and matching.
//!
//! # Matching semantics
//!
//! A rule matches an entry when every non-empty condition of its `match` table holds:
//!
//! * `kind`: `DIR` (default) matches directories only, `FILE` regular files only, `ANY` every
//!   entry kind (including symlinks, which are never followed)
//! * `name`: any basename glob matches
//! * `path`: any full-path glob matches, after expanding `~`, `$VAR`, `${VAR}`, and `%VAR%`
//!   (substituted values are glob-escaped). `*` does not cross `/`; `**` does. A glob
//!   referencing an unset variable is skipped (with a [`CompileWarning`]).
//! * `sibling_any` / `sibling_all`: globs over the *other* names in the same directory; the
//!   caller supplies them, so anchors cost no syscalls
//! * `contains_any`: names inside the directory; answered from the tree when the directory's
//!   children are materialized, otherwise by an existence probe (literal names) or a listing
//!   (globs)
//! * `not_under`: path globs; the rule does not match when the entry or any ancestor matches
//! * `os`: restricts the rule to these operating systems
//! * `git_ignored`: requires the entry to be (or not be) ignored by git
//!
//! Globs are case-insensitive on Windows and case-sensitive elsewhere.
//!
//! # Priority resolution
//!
//! When several rules match the same entry, the winner has the highest `priority`, then the
//! highest `confidence`, then the lexicographically smallest rule id (deterministic).
//!
//! # Synthetic rules
//!
//! * `cachedir_tag` (when `classify.cachedir_tag`): any directory containing a valid
//!   `CACHEDIR.TAG` (<https://bford.info/cachedir/>) is a high-confidence ephemeral cache, at
//!   priority -100 so specific rules win.
//! * `gitignored` (when `classify.gitignored_heuristic`): a directory ignored by git inside a
//!   repository is a low-confidence ephemeral guess, at priority -1000. It is consulted only by
//!   the classifier (never for scan pruning).

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};

use bclean_config::Expander;
use bclean_model::{
    ActionSpec, ClassifyConfig, Confidence, Config, EphemeralKind, EphemeralSpec, KeepSpec,
    MatchKind, MatchSpec, Os, RegenCost, RuleSpec, SizeModeSpec,
};
use bclean_tree::{EntryKind, NodeFlags, NodeId, Tree};
use globset::{Glob, GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

use crate::ignore::GitIgnoreCache;

/// Id of the synthetic `CACHEDIR.TAG` rule.
pub const CACHEDIR_TAG_RULE: &str = "cachedir_tag";
/// Id of the synthetic gitignored-directory rule.
pub const GITIGNORED_RULE: &str = "gitignored";
/// Marker file name of the Cache Directory Tagging Specification.
pub const CACHEDIR_TAG: &str = "CACHEDIR.TAG";
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// Error compiling rules.
#[derive(Debug, thiserror::Error)]
pub enum RuleError {
    /// A glob pattern is invalid.
    #[error("rule `{rule}`: invalid {field} glob `{pattern}`: {source}")]
    Glob {
        /// Rule id.
        rule: String,
        /// Field (`name`, `path`, ...).
        field: &'static str,
        /// Offending pattern.
        pattern: String,
        /// Underlying error.
        #[source]
        source: globset::Error,
    },
    /// An enabled rule has neither `name` nor `path` conditions.
    #[error("rule `{rule}` needs at least one `match.name` or `match.path` glob")]
    NoMatcher {
        /// Rule id.
        rule: String,
    },
}

/// Non-fatal compilation issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileWarning {
    /// Rule id.
    pub rule: String,
    /// Description.
    pub message: String,
}

/// One child of a directory, as supplied by the caller (scanner or classifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildEntry<'a> {
    /// Basename.
    pub name: &'a OsStr,
    /// Entry kind.
    pub kind: EntryKind,
}

/// Where `contains_any` markers are looked up.
#[derive(Debug, Clone, Copy)]
pub enum Contents<'a> {
    /// Probe the filesystem.
    Fs,
    /// Use the materialized children of this tree node (falls back to the filesystem for
    /// summarized directories).
    Tree(&'a Tree, NodeId),
}

/// Why a rule did not match an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Miss {
    /// Entry kind differs.
    Kind(MatchKind),
    /// No name glob matched.
    Name,
    /// No path glob matched.
    Path,
    /// No `sibling_any` anchor present.
    SiblingAny(Vec<String>),
    /// A `sibling_all` anchor is missing.
    SiblingAll(String),
    /// No `contains_any` marker present.
    Contains(Vec<String>),
    /// The entry is under an excluded path.
    NotUnder(String),
    /// Git-ignored state differs.
    GitIgnored {
        /// Required state.
        expected: bool,
    },
    /// `CACHEDIR.TAG` exists but lacks the required signature.
    BadCachedirTag,
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Kind(kind) => write!(f, "entry kind is not {kind:?}"),
            Self::Name => f.write_str("no name glob matched"),
            Self::Path => f.write_str("no path glob matched"),
            Self::SiblingAny(globs) => write!(f, "no sibling matching any of {globs:?}"),
            Self::SiblingAll(glob) => write!(f, "required sibling `{glob}` is missing"),
            Self::Contains(globs) => write!(f, "contains none of {globs:?}"),
            Self::NotUnder(glob) => write!(f, "excluded by not_under `{glob}`"),
            Self::GitIgnored { expected: true } => f.write_str("not ignored by git"),
            Self::GitIgnored { expected: false } => f.write_str("ignored by git"),
            Self::BadCachedirTag => f.write_str("CACHEDIR.TAG lacks the required signature"),
        }
    }
}

/// Which synthetic rule a compiled rule is, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Synthetic {
    CachedirTag,
    GitIgnored,
}

/// A compiled rule.
#[derive(Debug)]
pub struct CompiledRule {
    id: String,
    spec: RuleSpec,
    pub(crate) synthetic: Option<Synthetic>,
    names: Option<GlobSet>,
    paths: Option<GlobSet>,
    path_patterns: Vec<String>,
    sibling_any: Option<GlobSet>,
    sibling_all: Vec<(String, GlobMatcher)>,
    contains_literals: Vec<OsString>,
    contains_globs: Option<GlobSet>,
    not_under: Vec<(String, GlobSet)>,
}

impl CompiledRule {
    /// Rule id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The rule specification.
    #[must_use]
    pub const fn spec(&self) -> &RuleSpec {
        &self.spec
    }

    /// Expanded path globs (for diagnostics).
    #[must_use]
    pub fn path_patterns(&self) -> &[String] {
        &self.path_patterns
    }

    const fn rank(&self) -> (i32, Confidence, Reverse<&str>) {
        (
            self.spec.priority,
            self.spec.confidence,
            Reverse(self.id.as_str()),
        )
    }
}

/// Access to the names in a directory, used for sibling anchors.
pub trait SiblingNames {
    /// Returns `true` if `pred` holds for any name.
    fn any_name(&self, pred: &mut dyn FnMut(&OsStr) -> bool) -> bool;
}

impl SiblingNames for Vec<ChildEntry<'_>> {
    fn any_name(&self, pred: &mut dyn FnMut(&OsStr) -> bool) -> bool {
        self.iter().any(|c| pred(c.name))
    }
}

struct SliceNames<'a, T, F> {
    items: &'a [T],
    name: F,
}

impl<T, F: Fn(&T) -> &OsStr> SiblingNames for SliceNames<'_, T, F> {
    fn any_name(&self, pred: &mut dyn FnMut(&OsStr) -> bool) -> bool {
        self.items.iter().any(|item| pred((self.name)(item)))
    }
}

/// The entry being matched.
pub struct Probe<'a> {
    /// Parent directory.
    pub dir: &'a Path,
    /// Basename.
    pub name: &'a OsStr,
    /// Entry kind.
    pub kind: EntryKind,
    /// Names in the parent directory (may include the entry itself).
    pub siblings: &'a dyn SiblingNames,
    /// Where to look for `contains_any` markers.
    pub contents: Contents<'a>,
}

impl Probe<'_> {
    fn has_sibling(&self, set: &GlobSet) -> bool {
        let own = self.name;
        self.siblings
            .any_name(&mut |n| n != own && set.is_match(Path::new(n)))
    }

    fn has_sibling_matcher(&self, m: &GlobMatcher) -> bool {
        let own = self.name;
        self.siblings
            .any_name(&mut |n| n != own && m.is_match(Path::new(n)))
    }
}

/// Result of matching one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    /// Index of the winning rule.
    pub winner: usize,
    /// Other matching rules (losers of priority resolution), best first.
    pub others: Vec<usize>,
}

/// Compiled, ready-to-match rules plus classification settings.
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<CompiledRule>,
    names: GlobSet,
    name_owner: Vec<usize>,
    by_parent: BTreeMap<PathBuf, Vec<usize>>,
    by_prefix: Vec<(PathBuf, usize)>,
    cachedir_rule: Option<usize>,
    gitignored_rule: Option<usize>,
    summarize: bool,
    pub(crate) classify: ClassifyConfig,
    pub(crate) size_mode: SizeModeSpec,
    pub(crate) durable_roots: Vec<PathBuf>,
    pub(crate) expander: Expander,
    pub(crate) ignore: GitIgnoreCache,
    /// `[trim]` directories.
    trim: GlobSet,
    warnings: Vec<CompileWarning>,
}

fn glob(pattern: &str, literal_separator: bool) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(literal_separator)
        .case_insensitive(cfg!(windows))
        .backslash_escape(!cfg!(windows))
        .build()
}

fn glob_set<'a>(
    rule: &str,
    field: &'static str,
    patterns: impl IntoIterator<Item = &'a str>,
    literal_separator: bool,
) -> Result<Option<GlobSet>, RuleError> {
    let mut builder = GlobSetBuilder::new();
    let mut any = false;
    for pattern in patterns {
        let g = glob(pattern, literal_separator).map_err(|source| RuleError::Glob {
            rule: rule.to_owned(),
            field,
            pattern: pattern.to_owned(),
            source,
        })?;
        builder.add(g);
        any = true;
    }
    if !any {
        return Ok(None);
    }
    builder.build().map(Some).map_err(|source| RuleError::Glob {
        rule: rule.to_owned(),
        field,
        pattern: String::new(),
        source,
    })
}

fn has_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', '{', '\\'])
}

/// Normalizes a path key for the path indexes (case-folded on Windows; borrowed elsewhere).
fn index_key(path: &Path) -> Cow<'_, Path> {
    if cfg!(windows) {
        Cow::Owned(PathBuf::from(path.to_string_lossy().to_lowercase()))
    } else {
        Cow::Borrowed(path)
    }
}

fn synthetic_cachedir() -> RuleSpec {
    RuleSpec {
        description: Some(
            "Directory tagged with CACHEDIR.TAG (Cache Directory Tagging Specification)".to_owned(),
        ),
        category: Some("cache".to_owned()),
        matcher: MatchSpec {
            kind: MatchKind::Dir,
            contains_any: vec![CACHEDIR_TAG.to_owned()],
            ..MatchSpec::default()
        },
        ephemeral: Some(EphemeralSpec {
            kind: EphemeralKind::ToolCache,
            regen_cost: RegenCost::Network,
        }),
        action: ActionSpec::Trash,
        priority: -100,
        confidence: Confidence::High,
        ..RuleSpec::default()
    }
}

fn synthetic_gitignored() -> RuleSpec {
    RuleSpec {
        description: Some("Directory ignored by git inside a repository (heuristic)".to_owned()),
        category: Some("git".to_owned()),
        matcher: MatchSpec {
            kind: MatchKind::Dir,
            git_ignored: Some(true),
            ..MatchSpec::default()
        },
        ephemeral: Some(EphemeralSpec {
            kind: EphemeralKind::Other,
            regen_cost: RegenCost::Expensive,
        }),
        action: ActionSpec::Trash,
        priority: -1000,
        confidence: Confidence::Low,
        ..RuleSpec::default()
    }
}

/// Candidate indexes over compiled rules.
struct Indexes {
    names: GlobSet,
    name_owner: Vec<usize>,
    by_parent: BTreeMap<PathBuf, Vec<usize>>,
    by_prefix: Vec<(PathBuf, usize)>,
    cachedir_rule: Option<usize>,
    gitignored_rule: Option<usize>,
}

impl Indexes {
    /// Indexes name globs in one glob set and path globs by their literal parent directory
    /// (or, when the parent contains wildcards, by its literal prefix).
    fn build(rules: &[CompiledRule]) -> Result<Self, RuleError> {
        let mut names = GlobSetBuilder::new();
        let mut name_owner = Vec::new();
        let mut by_parent: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        let mut by_prefix = Vec::new();
        let mut cachedir_rule = None;
        let mut gitignored_rule = None;
        for (index, rule) in rules.iter().enumerate() {
            match rule.synthetic {
                Some(Synthetic::CachedirTag) => cachedir_rule = Some(index),
                Some(Synthetic::GitIgnored) => gitignored_rule = Some(index),
                None => {}
            }
            for pattern in &rule.spec.matcher.name {
                // Already validated in compile_rule.
                if let Ok(g) = glob(pattern, false) {
                    names.add(g);
                    name_owner.push(index);
                }
            }
            if rule.spec.matcher.name.is_empty() {
                for pattern in &rule.path_patterns {
                    let (parent, _) = pattern.rsplit_once('/').unwrap_or(("", pattern));
                    if !parent.is_empty() && !has_meta(parent) {
                        by_parent
                            .entry(index_key(Path::new(parent)).into_owned())
                            .or_default()
                            .push(index);
                    } else {
                        let prefix: PathBuf = parent
                            .split('/')
                            .take_while(|c| !has_meta(c))
                            .collect::<Vec<_>>()
                            .join("/")
                            .into();
                        by_prefix.push((index_key(&prefix).into_owned(), index));
                    }
                }
            }
        }
        for list in by_parent.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        by_prefix.sort();
        by_prefix.dedup();
        let names = names.build().map_err(|source| RuleError::Glob {
            rule: "<all>".to_owned(),
            field: "name",
            pattern: String::new(),
            source,
        })?;
        Ok(Self {
            names,
            name_owner,
            by_parent,
            by_prefix,
            cachedir_rule,
            gitignored_rule,
        })
    }
}

impl RuleSet {
    /// Compiles the rules and classification settings of `config` using the process
    /// environment for path expansion.
    ///
    /// # Errors
    ///
    /// * Any error from [`RuleSet::compile`]
    pub fn from_config(config: &Config) -> Result<Self, RuleError> {
        Self::compile(config, &Expander::from_process())
    }

    /// Compiles the rules and classification settings of `config`.
    ///
    /// Disabled rules and rules restricted to other operating systems are dropped.
    ///
    /// # Errors
    ///
    /// * [`RuleError::Glob`] if any glob is invalid
    /// * [`RuleError::NoMatcher`] if an enabled rule has neither `name` nor `path` globs
    pub fn compile(config: &Config, expander: &Expander) -> Result<Self, RuleError> {
        let current = Os::current();
        let mut warnings = Vec::new();
        let mut rules = Vec::new();
        for (id, spec) in &config.rules {
            if !spec.enabled {
                continue;
            }
            if !spec.matcher.os.is_empty()
                && !current.is_some_and(|os| spec.matcher.os.contains(&os))
            {
                continue;
            }
            for keep in &spec.keep {
                glob(&keep.glob, true).map_err(|source| RuleError::Glob {
                    rule: id.clone(),
                    field: "keep",
                    pattern: keep.glob.clone(),
                    source,
                })?;
            }
            if let Some(rule) = compile_rule(id, spec, expander, &mut warnings)? {
                rules.push(rule);
            }
        }
        let mut trim = GlobSetBuilder::new();
        for pattern in &config.trim.paths {
            let dir = match expander.expand_path(pattern) {
                Ok(path) => path.to_string_lossy().into_owned(),
                Err(e) => {
                    warnings.push(CompileWarning {
                        rule: "trim.paths".to_owned(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            let pattern_glob = glob(dir.trim_end_matches(['/', '\\']), true).map_err(|source| {
                RuleError::Glob {
                    rule: "trim".to_owned(),
                    field: "paths",
                    pattern: pattern.clone(),
                    source,
                }
            })?;
            trim.add(pattern_glob);
        }
        let trim = trim.build().map_err(|source| RuleError::Glob {
            rule: "trim".to_owned(),
            field: "paths",
            pattern: config.trim.paths.join(", "),
            source,
        })?;
        let classify = config.classify.clone();
        if classify.cachedir_tag && !config.rules.contains_key(CACHEDIR_TAG_RULE) {
            let mut rule =
                compile_rule_unchecked(CACHEDIR_TAG_RULE, synthetic_cachedir(), expander)?;
            rule.synthetic = Some(Synthetic::CachedirTag);
            rules.push(rule);
        }
        if classify.gitignored_heuristic && !config.rules.contains_key(GITIGNORED_RULE) {
            let mut rule =
                compile_rule_unchecked(GITIGNORED_RULE, synthetic_gitignored(), expander)?;
            rule.synthetic = Some(Synthetic::GitIgnored);
            rules.push(rule);
        }

        let indexes = Indexes::build(&rules)?;
        let durable_roots = classify
            .durable_roots
            .iter()
            .filter_map(|root| match expander.expand_path(root) {
                Ok(path) => Some(path),
                Err(e) => {
                    warnings.push(CompileWarning {
                        rule: "classify.durable_roots".to_owned(),
                        message: e.to_string(),
                    });
                    None
                }
            })
            .collect();
        Ok(Self {
            rules,
            names: indexes.names,
            name_owner: indexes.name_owner,
            by_parent: indexes.by_parent,
            by_prefix: indexes.by_prefix,
            cachedir_rule: indexes.cachedir_rule,
            gitignored_rule: indexes.gitignored_rule,
            summarize: config.scan.summarize_ephemeral,
            classify,
            size_mode: config.scan.size_mode,
            durable_roots,
            ignore: GitIgnoreCache::new(expander),
            expander: expander.clone(),
            trim,
            warnings,
        })
    }

    /// What cleaning `path`, a match of `spec`, keeps: `spec.keep` inside a `[trim]`
    /// directory, else nothing (`None`: the rule's own action applies).
    #[must_use]
    pub fn trim_keep(&self, path: &Path, spec: &RuleSpec) -> Option<Vec<KeepSpec>> {
        (!spec.keep.is_empty() && path.ancestors().any(|dir| self.trim.is_match(dir)))
            .then(|| spec.keep.clone())
    }

    /// Non-fatal issues found while compiling (skipped globs, unexpandable roots).
    #[must_use]
    pub fn warnings(&self) -> &[CompileWarning] {
        &self.warnings
    }

    /// All active compiled rules (including synthetic ones).
    #[must_use]
    pub fn rules(&self) -> &[CompiledRule] {
        &self.rules
    }

    /// The compiled rule at `index`.
    #[must_use]
    pub fn rule(&self, index: usize) -> &CompiledRule {
        &self.rules[index]
    }

    /// Finds an active rule by id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<usize> {
        self.rules.iter().position(|r| r.id == id)
    }

    /// The expander used for path globs and durable roots.
    #[must_use]
    pub const fn expander(&self) -> &Expander {
        &self.expander
    }

    /// Resolved `classify.durable_roots`.
    #[must_use]
    pub fn durable_roots(&self) -> &[PathBuf] {
        &self.durable_roots
    }

    /// The gitignore oracle used for `git_ignored` conditions.
    #[must_use]
    pub const fn git_ignore(&self) -> &GitIgnoreCache {
        &self.ignore
    }

    /// Collects candidate rule indices for an entry (cheap prefilter), sorted and deduplicated.
    fn candidates(&self, dir: &Path, name: &OsStr, scratch: &mut Vec<usize>, out: &mut Vec<usize>) {
        out.clear();
        self.names.matches_into(Path::new(name), scratch);
        out.extend(scratch.iter().map(|&g| self.name_owner[g]));
        if !self.by_parent.is_empty() || !self.by_prefix.is_empty() {
            let key = index_key(dir);
            if let Some(list) = self.by_parent.get(key.as_ref()) {
                out.extend_from_slice(list);
            }
            for (prefix, index) in &self.by_prefix {
                if key.starts_with(prefix) {
                    out.push(*index);
                }
            }
        }
        if out.len() > 1 {
            out.sort_unstable();
            out.dedup();
        }
    }

    /// Checks every condition of rule `index` against `probe`.
    ///
    /// # Errors
    ///
    /// * The first [`Miss`] encountered (conditions are checked cheapest first)
    pub fn check(&self, index: usize, probe: &Probe<'_>) -> Result<(), Miss> {
        let rule = &self.rules[index];
        let m = &rule.spec.matcher;
        let kind_ok = match m.kind {
            MatchKind::Dir => probe.kind == EntryKind::Dir,
            MatchKind::File => probe.kind == EntryKind::File,
            MatchKind::Any => true,
        };
        if !kind_ok {
            return Err(Miss::Kind(m.kind));
        }
        if let Some(names) = &rule.names
            && !names.is_match(Path::new(probe.name))
        {
            return Err(Miss::Name);
        }
        if let Some(set) = &rule.sibling_any
            && !probe.has_sibling(set)
        {
            return Err(Miss::SiblingAny(m.sibling_any.clone()));
        }
        for (raw, matcher) in &rule.sibling_all {
            if !probe.has_sibling_matcher(matcher) {
                return Err(Miss::SiblingAll(raw.clone()));
            }
        }
        let needs_path =
            rule.paths.is_some() || !rule.not_under.is_empty() || m.git_ignored.is_some();
        let full = needs_path.then(|| probe.dir.join(probe.name));
        if let (Some(paths), Some(full)) = (&rule.paths, &full)
            && !paths.is_match(full)
        {
            return Err(Miss::Path);
        }
        if let Some(full) = &full {
            for (raw, set) in &rule.not_under {
                if set.is_match(full) {
                    return Err(Miss::NotUnder(raw.clone()));
                }
            }
        }
        if !(rule.contains_literals.is_empty() && rule.contains_globs.is_none()) {
            let found = Self::contains(rule, probe);
            if !found {
                return Err(Miss::Contains(m.contains_any.clone()));
            }
            if rule.synthetic == Some(Synthetic::CachedirTag)
                && !valid_cachedir_tag(&probe.dir.join(probe.name).join(CACHEDIR_TAG))
            {
                return Err(Miss::BadCachedirTag);
            }
        }
        if let (Some(expected), Some(full)) = (m.git_ignored, &full) {
            let ignored = self
                .ignore
                .is_ignored(full, probe.kind == EntryKind::Dir, true)
                .unwrap_or(false);
            if ignored != expected {
                return Err(Miss::GitIgnored { expected });
            }
        }
        Ok(())
    }

    fn contains(rule: &CompiledRule, probe: &Probe<'_>) -> bool {
        let materialized = match probe.contents {
            Contents::Tree(tree, node)
                if tree.has_children(node) || !tree.flags(node).contains(NodeFlags::SUMMARIZED) =>
            {
                Some((tree, node))
            }
            _ => None,
        };
        if let Some((tree, node)) = materialized {
            return tree.children(node).any(|c| {
                let name = tree.name(c);
                rule.contains_literals.iter().any(|l| l == name)
                    || rule
                        .contains_globs
                        .as_ref()
                        .is_some_and(|g| g.is_match(Path::new(name)))
            });
        }
        let dir = probe.dir.join(probe.name);
        if rule
            .contains_literals
            .iter()
            .any(|l| dir.join(l).symlink_metadata().is_ok())
        {
            return true;
        }
        rule.contains_globs.as_ref().is_some_and(|globs| {
            std::fs::read_dir(&dir).is_ok_and(|entries| {
                entries
                    .flatten()
                    .take(100_000)
                    .any(|e| globs.is_match(Path::new(&e.file_name())))
            })
        })
    }

    /// Matches one entry against every candidate rule (excluding the gitignored heuristic
    /// unless `heuristics` is set) and resolves priority.
    #[must_use]
    pub fn match_entry(&self, probe: &Probe<'_>, heuristics: bool) -> Option<MatchResult> {
        let mut scratch = Vec::new();
        let mut candidates = Vec::new();
        self.match_with(probe, heuristics, &mut scratch, &mut candidates)
    }

    fn match_with(
        &self,
        probe: &Probe<'_>,
        heuristics: bool,
        scratch: &mut Vec<usize>,
        candidates: &mut Vec<usize>,
    ) -> Option<MatchResult> {
        self.candidates(probe.dir, probe.name, scratch, candidates);
        if probe.kind == EntryKind::Dir
            && let Some(c) = self.cachedir_rule
        {
            candidates.push(c);
        }
        let mut matched: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| self.check(i, probe).is_ok())
            .collect();
        if matched.is_empty()
            && heuristics
            && probe.kind == EntryKind::Dir
            && probe.name != ".git"
            && let Some(g) = self.gitignored_rule
            && self.check(g, probe).is_ok()
        {
            matched.push(g);
        }
        if matched.is_empty() {
            return None;
        }
        matched.sort_by(|&a, &b| self.rules[b].rank().cmp(&self.rules[a].rank()));
        matched.dedup();
        let winner = matched.remove(0);
        Some(MatchResult {
            winner,
            others: matched,
        })
    }

    /// Lists every rule that was a candidate for `probe` (by name or path index) together with
    /// its outcome; used by `explain` to report near misses.
    #[must_use]
    pub fn diagnose(&self, probe: &Probe<'_>) -> Vec<(usize, Result<(), Miss>)> {
        let mut scratch = Vec::new();
        let mut candidates = Vec::new();
        self.candidates(probe.dir, probe.name, &mut scratch, &mut candidates);
        candidates.extend(self.cachedir_rule);
        candidates.extend(self.gitignored_rule);
        candidates.dedup();
        candidates
            .into_iter()
            .map(|i| (i, self.check(i, probe)))
            .collect()
    }

    /// Scanner prune-oracle entry point: appends to `out` the indices of `children` (entries
    /// of directory `dir`) that the scanner should summarize instead of descending into —
    /// directories whose winning rule is ephemeral with at least medium confidence.
    ///
    /// Additionally, when `dir` itself contains a valid `CACHEDIR.TAG` (and
    /// `classify.cachedir_tag` is on), every child directory is reported, which summarizes the
    /// tagged cache's contents.
    ///
    /// Cost: one glob-set lookup per child plus an index lookup per directory; anchors are
    /// checked against `children` without syscalls. Allocation-free in the common case where
    /// nothing matches (scratch buffers are thread-local). Never consults git.
    pub fn ephemeral_children(
        &self,
        dir: &Path,
        children: &[ChildEntry<'_>],
        out: &mut Vec<usize>,
    ) {
        self.ephemeral_children_by(dir, children, |c| c.name, |c| c.kind, out);
    }

    /// Like [`RuleSet::ephemeral_children`] for an arbitrary child type.
    pub fn ephemeral_children_by<T>(
        &self,
        dir: &Path,
        children: &[T],
        name_of: impl Fn(&T) -> &OsStr,
        kind_of: impl Fn(&T) -> EntryKind,
        out: &mut Vec<usize>,
    ) {
        if !self.summarize {
            return;
        }
        if self.cachedir_rule.is_some()
            && children
                .iter()
                .any(|c| kind_of(c) == EntryKind::File && name_of(c) == CACHEDIR_TAG)
            && valid_cachedir_tag(&dir.join(CACHEDIR_TAG))
        {
            out.extend(
                children
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| kind_of(c) == EntryKind::Dir)
                    .map(|(i, _)| i),
            );
            return;
        }
        thread_local! {
            static SCRATCH: std::cell::RefCell<(Vec<usize>, Vec<usize>)> =
                const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
        }
        let siblings = SliceNames {
            items: children,
            name: &name_of,
        };
        SCRATCH.with(|cell| {
            let mut guard = cell.borrow_mut();
            let (scratch, candidates) = &mut *guard;
            for (i, child) in children.iter().enumerate() {
                let kind = kind_of(child);
                if kind != EntryKind::Dir {
                    continue;
                }
                let name = name_of(child);
                self.candidates(dir, name, scratch, candidates);
                if candidates.is_empty() {
                    continue;
                }
                let probe = Probe {
                    dir,
                    name,
                    kind,
                    siblings: &siblings,
                    contents: Contents::Fs,
                };
                let best = candidates
                    .iter()
                    .copied()
                    .filter(|&r| self.check(r, &probe).is_ok())
                    .max_by(|&a, &b| self.rules[a].rank().cmp(&self.rules[b].rank()));
                if let Some(best) = best {
                    let spec = &self.rules[best].spec;
                    if spec.ephemeral.is_some() && spec.confidence >= Confidence::Medium {
                        out.push(i);
                    }
                }
            }
        });
    }
}

/// Whether `path` is a `CACHEDIR.TAG` file starting with the specification's signature.
fn valid_cachedir_tag(path: &Path) -> bool {
    let mut buf = [0u8; CACHEDIR_SIGNATURE.len()];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok_and(|()| buf == CACHEDIR_SIGNATURE)
}

fn compile_rule(
    id: &str,
    spec: &RuleSpec,
    expander: &Expander,
    warnings: &mut Vec<CompileWarning>,
) -> Result<Option<CompiledRule>, RuleError> {
    let m = &spec.matcher;
    if m.name.is_empty() && m.path.is_empty() {
        return Err(RuleError::NoMatcher {
            rule: id.to_owned(),
        });
    }
    let mut path_patterns = Vec::new();
    for raw in &m.path {
        match expand_glob(raw, expander) {
            Ok(p) => path_patterns.push(p),
            Err(e) => warnings.push(CompileWarning {
                rule: id.to_owned(),
                message: format!("skipping path glob `{raw}`: {e}"),
            }),
        }
    }
    if !m.path.is_empty() && path_patterns.is_empty() {
        // Every path glob is unavailable on this machine: the rule can never match.
        return Ok(None);
    }
    let mut rule = compile_rule_unchecked(id, spec.clone(), expander)?;
    rule.path_patterns = path_patterns;
    rule.paths = glob_set(
        id,
        "path",
        rule.path_patterns.iter().map(String::as_str),
        true,
    )?;
    Ok(Some(rule))
}

fn expand_glob(raw: &str, expander: &Expander) -> Result<String, bclean_config::ExpandError> {
    let glob_text = expander.expand_escaped(raw, &|v| {
        let v = if cfg!(windows) {
            v.replace('\\', "/")
        } else {
            v.to_owned()
        };
        globset::escape(&v)
    })?;
    Ok(if cfg!(windows) {
        glob_text.replace('\\', "/")
    } else {
        glob_text
    })
}

fn compile_rule_unchecked(
    id: &str,
    spec: RuleSpec,
    expander: &Expander,
) -> Result<CompiledRule, RuleError> {
    let m = &spec.matcher;
    let names = glob_set(id, "name", m.name.iter().map(String::as_str), false)?;
    let sibling_any = glob_set(
        id,
        "sibling_any",
        m.sibling_any.iter().map(String::as_str),
        false,
    )?;
    let sibling_all = m
        .sibling_all
        .iter()
        .map(|p| {
            glob(p, false)
                .map(|g| (p.clone(), g.compile_matcher()))
                .map_err(|source| RuleError::Glob {
                    rule: id.to_owned(),
                    field: "sibling_all",
                    pattern: p.clone(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (literals, globs): (Vec<&String>, Vec<&String>) =
        m.contains_any.iter().partition(|p| !has_meta(p));
    let contains_globs = glob_set(id, "contains_any", globs.iter().map(|s| s.as_str()), false)?;
    let mut not_under = Vec::new();
    for raw in &m.not_under {
        let Ok(pattern_text) = expand_glob(raw, expander) else {
            continue;
        };
        let trimmed = pattern_text.trim_end_matches('/');
        let descendants = format!("{trimmed}/**");
        if let Some(set) = glob_set(id, "not_under", [trimmed, descendants.as_str()], true)? {
            not_under.push((raw.clone(), set));
        }
    }
    Ok(CompiledRule {
        id: id.to_owned(),
        synthetic: None,
        names,
        paths: None,
        path_patterns: Vec::new(),
        sibling_any,
        sibling_all,
        contains_literals: literals.into_iter().map(OsString::from).collect(),
        contains_globs,
        not_under,
        spec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    pub fn expander(home: &Path) -> Expander {
        Expander::new(Some(home.to_path_buf()), BTreeMap::new())
    }

    fn config(toml_rules: &str) -> Config {
        bclean_config::parse_str(toml_rules).unwrap()
    }

    fn entries<'a>(items: &'a [(&'a str, EntryKind)]) -> Vec<ChildEntry<'a>> {
        items
            .iter()
            .map(|(n, k)| ChildEntry {
                name: OsStr::new(n),
                kind: *k,
            })
            .collect()
    }

    const RULES: &str = r#"
[classify]
gitignored_heuristic = false

[rules.cargo_target]
match = { name = ["target"], sibling_any = ["Cargo.toml"], not_under = ["~/.cargo/**"] }
ephemeral = { kind = "BUILD_ARTIFACT", regen_cost = "CHEAP" }

[rules.maven_target]
match = { name = ["target"], sibling_any = ["pom.xml"] }
ephemeral = { kind = "BUILD_ARTIFACT", regen_cost = "CHEAP" }
priority = 5

[rules.venv]
match = { name = [".venv", "venv"], contains_any = ["pyvenv.cfg"] }
ephemeral = { kind = "DEPENDENCIES" }

[rules.caches_all]
match = { path = ["~/Library/Caches/*"] }
ephemeral = { kind = "TOOL_CACHE" }
priority = -10

[rules.brew]
match = { path = ["~/Library/Caches/Homebrew"] }
ephemeral = { kind = "PACKAGE_CACHE" }
priority = 10

[rules.electron]
match = { name = ["Cache"], path = ["~/Library/Application Support/*/*"] }
ephemeral = { kind = "BROWSER_CACHE" }

[rules.archives]
match = { name = ["Archives"] }
action = { kind = "REPORT" }

[rules.weak]
match = { name = ["dist"], sibling_any = ["package.json"] }
ephemeral = { kind = "BUILD_ARTIFACT" }
confidence = "LOW"

[rules.windows_only]
match = { name = ["target"], os = ["WINDOWS"] }
ephemeral = { kind = "TEMP" }
priority = 100

[rules.off]
enabled = false
match = { name = ["target"] }
priority = 1000
"#;

    #[test]
    fn prune_oracle_selects_anchored_ephemeral_dirs() {
        let home = Path::new("/home/u");
        let set = RuleSet::compile(&config(RULES), &expander(home)).unwrap();
        let children = entries(&[
            ("Cargo.toml", EntryKind::File),
            ("target", EntryKind::Dir),
            ("src", EntryKind::Dir),
            ("dist", EntryKind::Dir),
            ("package.json", EntryKind::File),
            ("Archives", EntryKind::Dir),
        ]);
        let mut out = Vec::new();
        set.ephemeral_children(Path::new("/w/proj"), &children, &mut out);
        // target (anchored), not dist (LOW confidence), not Archives (durable).
        assert_eq!(out, vec![1]);

        out.clear();
        let unanchored = entries(&[("target", EntryKind::Dir), ("README", EntryKind::File)]);
        set.ephemeral_children(Path::new("/w/other"), &unanchored, &mut out);
        assert_eq!(out, Vec::<usize>::new());

        // not_under excludes ~/.cargo.
        out.clear();
        set.ephemeral_children(
            Path::new("/home/u/.cargo/git/checkouts/x"),
            &children,
            &mut out,
        );
        assert_eq!(out, Vec::<usize>::new());
    }

    #[test]
    fn path_rules_and_priority() {
        let home = Path::new("/home/u");
        let set = RuleSet::compile(&config(RULES), &expander(home)).unwrap();
        let children = entries(&[("Homebrew", EntryKind::Dir), ("pip", EntryKind::Dir)]);
        let caches = Path::new("/home/u/Library/Caches");
        let probe = |name: &'static str| Probe {
            dir: caches,
            name: OsStr::new(name),
            kind: EntryKind::Dir,
            siblings: &children,
            contents: Contents::Fs,
        };
        let r = set.match_entry(&probe("Homebrew"), false).unwrap();
        assert_eq!(set.rule(r.winner).id(), "brew");
        assert_eq!(set.rule(r.others[0]).id(), "caches_all");
        let r = set.match_entry(&probe("pip"), false).unwrap();
        assert_eq!(set.rule(r.winner).id(), "caches_all");

        // Generic path glob with a wildcard parent plus a name condition.
        let app = Path::new("/home/u/Library/Application Support/Slack");
        let p = Probe {
            dir: app,
            name: OsStr::new("Cache"),
            kind: EntryKind::Dir,
            siblings: &children,
            contents: Contents::Fs,
        };
        assert_eq!(
            set.rule(set.match_entry(&p, false).unwrap().winner).id(),
            "electron"
        );
        let deeper = Path::new("/home/u/Library/Application Support/Slack/x");
        let p = Probe { dir: deeper, ..p };
        assert!(set.match_entry(&p, false).is_none());
    }

    #[test]
    fn tie_breaks_and_os_filter() {
        let home = Path::new("/home/u");
        let set = RuleSet::compile(&config(RULES), &expander(home)).unwrap();
        assert!(set.find("off").is_none());
        assert_eq!(set.find("windows_only").is_some(), cfg!(windows));
        let children = entries(&[
            ("Cargo.toml", EntryKind::File),
            ("pom.xml", EntryKind::File),
            ("target", EntryKind::Dir),
        ]);
        let p = Probe {
            dir: Path::new("/w/p"),
            name: OsStr::new("target"),
            kind: EntryKind::Dir,
            siblings: &children,
            contents: Contents::Fs,
        };
        let r = set.match_entry(&p, false).unwrap();
        let expected = if cfg!(windows) {
            "windows_only"
        } else {
            "maven_target"
        };
        assert_eq!(set.rule(r.winner).id(), expected);
    }

    #[test]
    fn contains_probe_and_cachedir_tag() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("venv")).unwrap();
        fs::create_dir_all(root.join(".venv")).unwrap();
        fs::write(root.join(".venv/pyvenv.cfg"), "").unwrap();
        fs::create_dir_all(root.join("cache/sub")).unwrap();
        fs::write(
            root.join("cache/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# comment",
        )
        .unwrap();
        fs::create_dir_all(root.join("fake")).unwrap();
        fs::write(root.join("fake/CACHEDIR.TAG"), "nope").unwrap();

        let set = RuleSet::compile(&config(RULES), &expander(Path::new("/home/u"))).unwrap();
        let children = entries(&[
            ("venv", EntryKind::Dir),
            (".venv", EntryKind::Dir),
            ("cache", EntryKind::Dir),
            ("fake", EntryKind::Dir),
        ]);
        let mut out = Vec::new();
        set.ephemeral_children(root, &children, &mut out);
        assert_eq!(out, vec![1]);
        let probe = |name: &'static str| Probe {
            dir: root,
            name: OsStr::new(name),
            kind: EntryKind::Dir,
            siblings: &children,
            contents: Contents::Fs,
        };
        let r = set.match_entry(&probe("cache"), false).unwrap();
        assert_eq!(set.rule(r.winner).id(), CACHEDIR_TAG_RULE);
        assert!(set.match_entry(&probe("fake"), false).is_none());
        assert!(
            set.diagnose(&probe("fake"))
                .iter()
                .any(|(_, r)| *r == Err(Miss::BadCachedirTag))
        );

        // Inside a tagged cache, the oracle summarizes every child directory.
        out.clear();
        let inner = entries(&[("CACHEDIR.TAG", EntryKind::File), ("sub", EntryKind::Dir)]);
        set.ephemeral_children(&root.join("cache"), &inner, &mut out);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn rejects_bad_rules_and_warns_on_unset_vars() {
        let err = RuleSet::compile(
            &config("[rules.x]\nmatch = { sibling_any = [\"a\"] }\n"),
            &expander(Path::new("/h")),
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::NoMatcher { .. }));
        let err = RuleSet::compile(
            &config("[rules.x]\nmatch = { name = [\"[\"] }\n"),
            &expander(Path::new("/h")),
        )
        .unwrap_err();
        assert!(matches!(err, RuleError::Glob { .. }), "{err}");
        let set = RuleSet::compile(
            &config("[rules.x]\nmatch = { path = [\"%NOPE_VAR%/Temp\"] }\n"),
            &expander(Path::new("/h")),
        )
        .unwrap();
        assert!(set.find("x").is_none());
        assert_eq!(set.warnings().len(), 1);
    }

    #[test]
    fn home_with_glob_metacharacters_is_escaped() {
        let home = Path::new("/home/we[ir]d");
        let set = RuleSet::compile(&config(RULES), &expander(home)).unwrap();
        let children = entries(&[("Homebrew", EntryKind::Dir)]);
        let p = Probe {
            dir: Path::new("/home/we[ir]d/Library/Caches"),
            name: OsStr::new("Homebrew"),
            kind: EntryKind::Dir,
            siblings: &children,
            contents: Contents::Fs,
        };
        assert_eq!(
            set.rule(set.match_entry(&p, false).unwrap().winner).id(),
            "brew"
        );
    }
}
