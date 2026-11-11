//! Tree classification: finds rule matches (without descending into matched directories)
//! plus durable-stale suggestions, then evaluates staleness and scores in parallel.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use bclean_model::{
    ActionSpec, Classification, Confidence, EphemeralSpec, KeepSpec, OwnedSpec, Reason, RuleSpec,
    SignalKind, SizeModeSpec, Staleness,
};
use bclean_staleness::{StalenessEngine, StalenessPolicy, Target};
use bclean_tree::{EntryKind, NodeFlags, NodeId, SizeMode, Tree};
use rayon::prelude::*;

use crate::matcher::{ChildEntry, Contents, MatchResult, Probe, RuleSet, SiblingNames};
use crate::score::{regen_weight, score, staleness_factor};

/// Code of the reason recording a rule match.
pub const REASON_RULE_MATCH: &str = "RULE_MATCH";

/// One classified tree entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassifiedEntry {
    /// Tree node.
    pub node: NodeId,
    /// Absolute path.
    pub path: PathBuf,
    /// Classification.
    pub classification: Classification,
}

/// A match found during the walk, before staleness evaluation.
struct Candidate {
    node: NodeId,
    path: PathBuf,
    /// `None` = durable-stale suggestion.
    rule: Option<usize>,
    reasons: Vec<Reason>,
}

/// Classifies trees and paths with a compiled [`RuleSet`] and a [`StalenessEngine`].
#[derive(Debug, Clone, Copy)]
pub struct Classifier<'a> {
    pub(crate) rules: &'a RuleSet,
    pub(crate) engine: &'a StalenessEngine,
}

/// Owned sibling list for the tree root (read from the filesystem).
struct OwnedNames(Vec<OsString>);

impl SiblingNames for OwnedNames {
    fn any_name(&self, pred: &mut dyn FnMut(&OsStr) -> bool) -> bool {
        self.0.iter().any(|n| pred(n))
    }
}

pub fn list_names(dir: &Path) -> Vec<(OsString, EntryKind)> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .take(200_000)
                .map(|e| {
                    let kind = e.file_type().map_or(EntryKind::Other, |t| {
                        if t.is_symlink() {
                            EntryKind::Symlink
                        } else if t.is_dir() {
                            EntryKind::Dir
                        } else if t.is_file() {
                            EntryKind::File
                        } else {
                            EntryKind::Other
                        }
                    });
                    (e.file_name(), kind)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Lists the sibling anchors that satisfied a rule.
fn anchor_reason(any: &[String], all: &[String], probe: &Probe<'_>) -> Option<Reason> {
    let own = probe.name;
    let mut anchors: Vec<String> = Vec::new();
    for glob in any.iter().chain(all) {
        if let Ok(g) = globset::Glob::new(glob) {
            let matcher = g.compile_matcher();
            probe.siblings.any_name(&mut |n| {
                if n != own && matcher.is_match(Path::new(n)) {
                    anchors.push(n.to_string_lossy().into_owned());
                    true
                } else {
                    false
                }
            });
        }
    }
    if anchors.is_empty() {
        return None;
    }
    anchors.sort();
    anchors.dedup();
    Some(Reason::new(
        "ANCHOR_FOUND",
        format!("anchor sibling(s) present: {}", anchors.join(", ")),
    ))
}

/// Builds the explanation lines for a match.
pub fn match_reasons(rules: &RuleSet, result: &MatchResult, probe: &Probe<'_>) -> Vec<Reason> {
    let rule = rules.rule(result.winner);
    let spec = rule.spec();
    let mut reasons = vec![Reason::new(
        REASON_RULE_MATCH,
        format!(
            "matched rule `{}`{}{}",
            rule.id(),
            spec.category
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default(),
            spec.description
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default()
        ),
    )];
    let m = &spec.matcher;
    if !m.name.is_empty() {
        reasons.push(Reason::new(
            "NAME_MATCH",
            format!(
                "name `{}` matches {:?}",
                probe.name.to_string_lossy(),
                m.name
            ),
        ));
    }
    if !rule.path_patterns().is_empty() {
        reasons.push(Reason::new(
            "PATH_MATCH",
            format!("path matches {:?}", rule.path_patterns()),
        ));
    }
    if let Some(reason) = anchor_reason(&m.sibling_any, &m.sibling_all, probe) {
        reasons.push(reason);
    }
    if !m.contains_any.is_empty() {
        reasons.push(Reason::new(
            if rule.id() == crate::CACHEDIR_TAG_RULE {
                "CACHEDIR_TAG"
            } else {
                "MARKER_FOUND"
            },
            format!("contains one of {:?}", m.contains_any),
        ));
    }
    if let Some(expected) = m.git_ignored {
        reasons.push(Reason::new(
            "GIT_IGNORED",
            if expected {
                "ignored by git"
            } else {
                "not ignored by git"
            },
        ));
    }
    if !m.os.is_empty() {
        reasons.push(Reason::new("OS", format!("rule applies to {:?}", m.os)));
    }
    reasons.push(Reason::new(
        "CONFIDENCE",
        format!("{:?} confidence", spec.confidence),
    ));
    if !result.others.is_empty() {
        let others: Vec<String> = result
            .others
            .iter()
            .map(|&i| {
                let r = rules.rule(i);
                format!("{} (priority {})", r.id(), r.spec().priority)
            })
            .collect();
        reasons.push(Reason::new(
            "OTHER_RULES",
            format!(
                "also matched (lower precedence than priority {}): {}",
                spec.priority,
                others.join(", ")
            ),
        ));
    }
    reasons
}

fn fmt_duration(secs: u64) -> String {
    bclean_model::DurationSpec(std::time::Duration::from_secs(secs)).to_string()
}

/// Appends staleness, score, and action reasons.
pub fn outcome_reasons(
    reasons: &mut Vec<Reason>,
    staleness: &Staleness,
    size: u64,
    confidence: Confidence,
    ephemeral: Option<EphemeralSpec>,
    action: &ActionSpec,
    auto: bool,
) {
    match ephemeral {
        Some(e) => reasons.push(Reason::new(
            "EPHEMERAL",
            format!("ephemeral {:?}, regen cost {:?}", e.kind, e.regen_cost),
        )),
        None => reasons.push(Reason::new("DURABLE", "durable (not regenerable)")),
    }
    for signal in &staleness.signals {
        let when = signal
            .last_activity
            .map(|t| format!(" last activity {t}"))
            .unwrap_or_default();
        let forced = if signal.forces_stale {
            " [forces stale]"
        } else {
            ""
        };
        reasons.push(Reason::new(
            "SIGNAL",
            format!("{:?}:{when} {}{forced}", signal.kind, signal.detail),
        ));
    }
    let threshold = fmt_duration(staleness.threshold_secs);
    let verdict = staleness.idle_secs.map_or_else(
        || format!("no activity data (threshold {threshold})"),
        |idle| format!("idle {} vs threshold {threshold}", fmt_duration(idle)),
    );
    reasons.push(Reason::new(
        if staleness.stale { "STALE" } else { "FRESH" },
        verdict,
    ));
    reasons.push(Reason::new(
        "SCORE",
        format!(
            "score = {size} B × staleness {:.2} × confidence {:.2} ÷ regen {:.1}",
            staleness_factor(staleness),
            confidence.weight(),
            regen_weight(ephemeral)
        ),
    ));
    reasons.push(Reason::new(
        "ACTION",
        format!(
            "action {action:?}{}",
            if auto {
                ", eligible for auto-clean"
            } else {
                ""
            }
        ),
    ));
}

/// Switches `class`, a match of `spec` at `path`, to trimming inside a `[trim]` directory.
pub fn trim(rules: &RuleSet, path: &Path, spec: &RuleSpec, class: &mut Classification) {
    if let Some(keep) = rules.trim_keep(path, spec) {
        class.reasons.push(Reason::new(
            "TRIM",
            format!(
                "trimmed (inside a [trim] path): keeps {}",
                keep.iter()
                    .map(|k| if k.executable {
                        format!("executable {}", k.glob)
                    } else {
                        k.glob.clone()
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
        class.action = ActionSpec::Trim { keep };
    }
}

/// Keeps what `spec`'s tool did not create inside the match at `path` (see
/// [`RuleSpec::owned`]): the match is trimmed around it, and its size excludes it.
pub fn keep_foreign(rules: &RuleSet, path: &Path, spec: &RuleSpec, class: &mut Classification) {
    let Some(owned) = &spec.owned else {
        return;
    };
    let foreign = foreign_entries(path, owned);
    if foreign.is_empty() {
        return;
    }
    let allocated = matches!(rules.size_mode, bclean_model::SizeModeSpec::Allocated);
    let foreign_size: u64 = foreign
        .iter()
        .map(|name| crate::explain::measure(&path.join(name), allocated).0)
        .sum();
    let mut keep = match &class.action {
        ActionSpec::Trim { keep } => keep.clone(),
        _ => Vec::new(),
    };
    for name in &foreign {
        let name = globset::escape(name);
        keep.push(KeepSpec {
            glob: name.clone(),
            executable: false,
        });
        keep.push(KeepSpec {
            glob: format!("{name}/**"),
            executable: false,
        });
    }
    class.reasons.push(Reason::new(
        "FOREIGN",
        format!(
            "keeps {} ({}): not created by the rule's tool, so never cleaned with it",
            foreign.join(", "),
            bclean_model::ByteSize(foreign_size)
        ),
    ));
    class.size = class.size.saturating_sub(foreign_size);
    class.action = ActionSpec::Trim { keep };
}

/// Top-level entries of `dir` not created by the tool `owned` describes.
pub fn foreign_entries(dir: &Path, owned: &OwnedSpec) -> Vec<String> {
    let mut names = globset::GlobSetBuilder::new();
    for pattern in &owned.names {
        if let Ok(glob) = globset::Glob::new(pattern) {
            names.add(glob);
        }
    }
    let Ok(names) = names.build() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut foreign: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let tools = names.is_match(&name) || (is_dir && holds_marker(&path, &owned.markers));
            (!tools).then_some(name)
        })
        .collect();
    foreign.sort();
    foreign
}

/// Whether `dir` holds one of `markers` at most three levels down.
fn holds_marker(dir: &Path, markers: &[String]) -> bool {
    const MAX_DEPTH: usize = 3;
    const MAX_DIRS: usize = 256;
    let mut queue = std::collections::VecDeque::from([(dir.to_path_buf(), 0usize)]);
    let mut visited = 0;
    while let Some((current, depth)) = queue.pop_front() {
        if markers
            .iter()
            .any(|m| current.join(m).symlink_metadata().is_ok())
        {
            return true;
        }
        visited += 1;
        if depth + 1 >= MAX_DEPTH || visited >= MAX_DIRS {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                queue.push_back((entry.path(), depth + 1));
            }
        }
    }
    false
}

/// Staleness policy for a rule (or a durable suggestion when `spec` is `None`).
pub fn policy_for(rules: &RuleSet, spec: Option<&RuleSpec>) -> StalenessPolicy {
    let classify = &rules.classify;
    let ephemeral = spec.is_some_and(|s| s.ephemeral.is_some());
    let default_after = if ephemeral {
        classify.stale_after
    } else {
        classify.durable_stale_after
    };
    let signals: Vec<SignalKind> = spec
        .and_then(|s| s.stale.signals.clone())
        .unwrap_or_else(|| classify.signals.clone());
    StalenessPolicy {
        threshold_secs: spec
            .and_then(|s| s.stale.after)
            .unwrap_or(default_after)
            .as_secs(),
        project_idle_secs: spec.and_then(|s| {
            s.stale
                .project_idle
                .map(bclean_model::DurationSpec::as_secs)
        }),
        signals,
    }
}

pub const fn tree_size_mode(mode: SizeModeSpec) -> SizeMode {
    match mode {
        SizeModeSpec::Allocated => SizeMode::Allocated,
        SizeModeSpec::Apparent => SizeMode::Apparent,
    }
}

/// Assembles a classification from a rule (or durable suggestion) and evaluated staleness.
pub fn assemble(
    spec: Option<(&str, &RuleSpec)>,
    staleness: Staleness,
    size: u64,
    mut reasons: Vec<Reason>,
) -> Classification {
    let (rule_id, category, ephemeral, confidence, action, auto) = match spec {
        Some((id, s)) => (
            Some(id.to_owned()),
            s.category.clone(),
            s.ephemeral,
            s.confidence,
            s.action.clone(),
            s.auto,
        ),
        None => (
            None,
            None,
            None,
            Confidence::Medium,
            ActionSpec::Trash,
            false,
        ),
    };
    outcome_reasons(
        &mut reasons,
        &staleness,
        size,
        confidence,
        ephemeral,
        &action,
        auto,
    );
    Classification {
        rule_id,
        category,
        ephemeral,
        confidence,
        score: score(size, &staleness, confidence, ephemeral),
        staleness,
        size,
        action,
        auto,
        reasons,
    }
}

impl<'a> Classifier<'a> {
    /// Creates a classifier.
    #[must_use]
    pub const fn new(rules: &'a RuleSet, engine: &'a StalenessEngine) -> Self {
        Self { rules, engine }
    }

    /// Classifies the whole tree. See [`Classifier::classify_subtree`].
    #[must_use]
    pub fn classify_tree(&self, tree: &Tree) -> Vec<ClassifiedEntry> {
        self.classify_subtree(tree, NodeId::ROOT)
    }

    /// Classifies `start` and its descendants.
    ///
    /// * Every entry matched by a rule is reported once; matched directories are not
    ///   descended into (nested matches are part of the outer item).
    /// * Children of `classify.durable_roots` that matched nothing, are at least
    ///   `classify.durable_min_size`, and are stale (by `durable_stale_after`) are reported as
    ///   durable suggestions with a `TRASH` action.
    /// * `.git` directories are never descended into.
    ///
    /// Matching is sequential (it is cheap and answers anchors from the tree); staleness
    /// evaluation, which may touch the filesystem and git, runs in parallel with rayon, with
    /// project and worktree lookups cached across items. Results are sorted by score,
    /// highest first.
    #[must_use]
    pub fn classify_subtree(&self, tree: &Tree, start: NodeId) -> Vec<ClassifiedEntry> {
        let mut candidates = Vec::new();
        let mut matched: BTreeSet<NodeId> = BTreeSet::new();
        match self.match_start(tree, start) {
            (Some(c), _) if !self.splits(tree, c.node, &c.path, c.rule) => {
                matched.insert(c.node);
                candidates.push(c);
            }
            (_, true) => {}
            _ => self.walk(tree, start, &mut candidates, &mut matched),
        }
        self.durable_candidates(tree, start, &matched, &mut candidates);

        let mut out: Vec<ClassifiedEntry> = candidates
            .into_par_iter()
            .filter_map(|c| self.evaluate(tree, c))
            .collect();
        out.sort_by(|a, b| {
            b.classification
                .score
                .total_cmp(&a.classification.score)
                .then_with(|| a.path.cmp(&b.path))
        });
        out
    }

    /// Classifies a single tree node (matching it against its tree siblings), if a rule
    /// matches it.
    #[must_use]
    pub fn classify_node(&self, tree: &Tree, node: NodeId) -> Option<Classification> {
        let candidate = self.match_start(tree, node).0?;
        self.evaluate(tree, candidate).map(|e| e.classification)
    }

    const fn heuristics(&self) -> bool {
        self.rules.classify.gitignored_heuristic
    }

    /// Whether the directory matched as `candidate` is better matched entry by entry (see
    /// [`bclean_model::MatchSpec::split`]): it is in use itself, but something directly inside
    /// it is not.
    fn splits(&self, tree: &Tree, node: NodeId, path: &Path, rule: Option<usize>) -> bool {
        let Some(spec) = rule.map(|r| self.rules.rule(r).spec()) else {
            return false;
        };
        if !spec.matcher.split || tree.kind(node) != EntryKind::Dir || !tree.has_children(node) {
            return false;
        }
        let policy = policy_for(self.rules, Some(spec));
        let stale = |node: NodeId, path: &Path| {
            self.engine
                .evaluate(&Target::node(tree, node, path), &policy)
                .stale
        };
        !stale(node, path)
            && tree
                .children(node)
                .any(|c| stale(c, &path.join(tree.name(c))))
    }

    /// Matches `node` itself, using tree siblings or (for the tree root) a directory listing;
    /// also tells whether nothing under it may be matched (see [`RuleSet::match_sealed`]).
    fn match_start(&self, tree: &Tree, node: NodeId) -> (Option<Candidate>, bool) {
        let path = tree.path(node);
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return (None, false);
        };
        let kind = tree.kind(node);
        let names: Vec<OsString> = tree.parent(node).map_or_else(
            || list_names(dir).into_iter().map(|(n, _)| n).collect(),
            |parent| {
                tree.children(parent)
                    .map(|c| tree.name(c).to_os_string())
                    .collect()
            },
        );
        let owned = OwnedNames(names);
        let probe = Probe {
            dir,
            name,
            kind,
            siblings: &owned,
            contents: Contents::Tree(tree, node),
        };
        let (result, sealed) = self.rules.match_sealed(&probe, self.heuristics());
        let Some(result) = result else {
            return (None, sealed);
        };
        let reasons = match_reasons(self.rules, &result, &probe);
        let candidate = Candidate {
            node,
            path: path.clone(),
            rule: Some(result.winner),
            reasons,
        };
        (Some(candidate), false)
    }

    fn walk(
        &self,
        tree: &Tree,
        start: NodeId,
        candidates: &mut Vec<Candidate>,
        matched: &mut BTreeSet<NodeId>,
    ) {
        let mut stack = vec![(start, tree.path(start))];
        let mut children: Vec<(NodeId, ChildEntry<'_>)> = Vec::new();
        let mut entries: Vec<ChildEntry<'_>> = Vec::new();
        let heuristics = self.heuristics();
        while let Some((dir_node, dir_path)) = stack.pop() {
            if tree.kind(dir_node) != EntryKind::Dir {
                continue;
            }
            children.clear();
            children.extend(tree.children(dir_node).map(|c| {
                (
                    c,
                    ChildEntry {
                        name: tree.name(c),
                        kind: tree.kind(c),
                    },
                )
            }));
            entries.clear();
            entries.extend(children.iter().map(|(_, e)| *e));
            for &(child, entry) in &children {
                let flags = tree.flags(child);
                if flags.contains(NodeFlags::REMOVED) {
                    continue;
                }
                let probe = Probe {
                    dir: &dir_path,
                    name: entry.name,
                    kind: entry.kind,
                    siblings: &entries,
                    contents: Contents::Tree(tree, child),
                };
                let (result, sealed) = self.rules.match_sealed(&probe, heuristics);
                if let Some(result) = result {
                    let path = dir_path.join(entry.name);
                    if self.splits(tree, child, &path, Some(result.winner)) {
                        stack.push((child, path));
                        continue;
                    }
                    let reasons = match_reasons(self.rules, &result, &probe);
                    matched.insert(child);
                    candidates.push(Candidate {
                        node: child,
                        path,
                        rule: Some(result.winner),
                        reasons,
                    });
                    continue;
                }
                if !sealed
                    && entry.kind == EntryKind::Dir
                    && entry.name != ".git"
                    && tree.has_children(child)
                    && !flags.contains(NodeFlags::EXCLUDED)
                {
                    stack.push((child, dir_path.join(entry.name)));
                }
            }
        }
    }

    fn durable_candidates(
        &self,
        tree: &Tree,
        start: NodeId,
        matched: &BTreeSet<NodeId>,
        candidates: &mut Vec<Candidate>,
    ) {
        let min = self.rules.classify.durable_min_size.bytes();
        let mode = tree_size_mode(self.rules.size_mode);
        for root in &self.rules.durable_roots {
            let Some(root_node) = tree.find(root) else {
                continue;
            };
            // Only roots within the classified subtree.
            if root_node != start && !tree.ancestors(root_node).contains(&start) {
                continue;
            }
            if matched.contains(&root_node)
                || tree
                    .ancestors(root_node)
                    .iter()
                    .any(|a| matched.contains(a))
            {
                continue;
            }
            for child in tree.children(root_node) {
                if matched.contains(&child) || tree.size(child, mode) < min {
                    continue;
                }
                candidates.push(Candidate {
                    node: child,
                    path: tree.path(child),
                    rule: None,
                    reasons: vec![Reason::new(
                        "DURABLE_ROOT",
                        format!(
                            "unmatched item under durable root {} (at least {})",
                            root.display(),
                            self.rules.classify.durable_min_size
                        ),
                    )],
                });
            }
        }
    }

    fn evaluate(&self, tree: &Tree, c: Candidate) -> Option<ClassifiedEntry> {
        let spec = c.rule.map(|i| {
            let r = self.rules.rule(i);
            (r.id(), r.spec())
        });
        let policy = policy_for(self.rules, spec.map(|(_, s)| s));
        let target = Target::node(tree, c.node, &c.path);
        let staleness = self.engine.evaluate(&target, &policy);
        if c.rule.is_none() && !staleness.stale {
            return None;
        }
        let size = tree.size(c.node, tree_size_mode(self.rules.size_mode));
        let mut classification = assemble(spec, staleness, size, c.reasons);
        if let Some((_, spec)) = spec {
            trim(self.rules, &c.path, spec, &mut classification);
            keep_foreign(self.rules, &c.path, spec, &mut classification);
        }
        Some(ClassifiedEntry {
            node: c.node,
            path: c.path,
            classification,
        })
    }
}
