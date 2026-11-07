//! [`Engine::dashboard`]: the dashboard summary, computed from the in-memory index, volume
//! history, and the action journal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use bclean_actions::{Candidate, JournalOutcome, PerformedAction};
use bclean_model::{
    ActivitySummary, BudgetSpec, CategoryBucket, Classification, DashboardItem, DashboardSummary,
    EphemeralKind, GrowthItem, KindBucket, LastAction, ProjectSummary, RootSummary, SelectSpec,
    Severity, VolumeStats, VolumeSummary,
};
use bclean_staleness::unix_now;
use bclean_tree::{NodeId, SizeMode, Tree};

use crate::{Engine, Result};

/// Candidates listed in [`DashboardSummary::top_items`].
const TOP_ITEMS: usize = 50;
/// Projects listed in [`DashboardSummary::projects`].
const TOP_PROJECTS: usize = 30;
/// Directories listed per growth window.
const TOP_GROWTH: usize = 10;
/// Points in a volume trend.
const TREND_POINTS: usize = 60;
/// Volume history shown as a trend.
const TREND_SECS: i64 = 30 * 86_400;
/// History used for the growth rate and forecast.
const RATE_SECS: i64 = 7 * 86_400;
/// Minimum history span before a growth rate is reported.
const MIN_RATE_SPAN_SECS: i64 = 6 * 3_600;
/// How long a computed volume trend is reused.
const TREND_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Files and directories that mark a project directory.
const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "go.mod",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "mix.exs",
    "Package.swift",
    "pubspec.yaml",
    "composer.json",
    "CMakeLists.txt",
    "stack.yaml",
    "deno.json",
    "flake.nix",
];

/// Directory names whose contents are tool-managed, never a user's project (package stores,
/// caches, application support).
const NOT_PROJECT_PARENTS: &[&str] = &[
    "node_modules",
    ".pnpm-store",
    ".pnpm",
    ".npm",
    ".yarn",
    ".cache",
    "Caches",
    "Library",
    ".cargo",
    ".rustup",
    ".gradle",
    ".m2",
    "site-packages",
    ".venv",
    "vendor",
];

/// Cached trend of one volume.
#[derive(Debug, Clone)]
pub struct VolumeTrend {
    computed: Instant,
    trend: Vec<u64>,
    span_secs: u64,
    bytes_per_sec: Option<f64>,
}

impl Engine {
    /// Computes the dashboard summary over the index roots (`monitor.index_roots`) plus
    /// `extra_roots` (for example the root a client is browsing), counting each path once:
    /// a path inside nested roots is taken from the deepest one. Only loaded roots are
    /// included; nothing is scanned.
    ///
    /// # Errors
    ///
    /// * [`crate::EngineError::Volume`] if volumes cannot be listed
    /// * [`crate::EngineError::History`] / [`crate::EngineError::Journal`] if history or the
    ///   journal cannot be read
    pub async fn dashboard(&self, extra_roots: &[PathBuf]) -> Result<DashboardSummary> {
        let now = unix_now();
        let mut wanted: BTreeSet<PathBuf> = self.index_roots().into_iter().collect();
        for root in extra_roots {
            if let Ok(root) = std::path::absolute(root)
                && let Some(loaded) = self.root_for(&root)
            {
                wanted.insert(loaded);
            }
        }
        let scope = self.scoped_roots(&wanted);

        let mut summary = DashboardSummary {
            generated_at: now,
            ..DashboardSummary::default()
        };
        let items = collect_items(&scope);
        let safe = safe_paths(&items);
        summarize_items(&mut summary, &items, &safe, &scope);

        let volumes = self.volumes().await.unwrap_or_default();
        for stats in volumes {
            let trend = self.volume_trend(&stats, now).await;
            summary
                .volumes
                .push(volume_summary(stats, &scope, &items, trend));
        }
        summary
            .volumes
            .sort_by(|a, b| b.stats.used_fraction().total_cmp(&a.stats.used_fraction()));

        summary.roots = scope
            .iter()
            .map(|r| RootSummary {
                path: r.path.clone(),
                size: r.tree.size(NodeId::ROOT, SizeMode::Allocated),
                scanned_at: r.scanned_at,
                scanning: r.scanning,
            })
            .collect();

        for (window, out) in [
            (86_400, &mut summary.growth_day),
            (7 * 86_400, &mut summary.growth_week),
        ] {
            *out = self.growth(&scope, now - window).await;
        }
        summary.activity = self.activity(now).await;
        Ok(summary)
    }

    /// Loaded roots among `wanted`, outermost first.
    fn scoped_roots(&self, wanted: &BTreeSet<PathBuf>) -> Vec<ScopedRoot> {
        let mut scope: Vec<ScopedRoot> = self
            .slots()
            .into_iter()
            .filter(|slot| wanted.contains(&slot.root))
            .filter_map(|slot| {
                let state = slot.state.read();
                Some(ScopedRoot {
                    path: slot.root.clone(),
                    tree: state.tree.clone()?,
                    classes: state.classes.clone(),
                    scanned_at: state.scanned_at,
                    scanning: state.progress.is_some(),
                })
            })
            .collect();
        scope.sort_by_key(|r| r.path.components().count());
        scope
    }

    /// Volume trend and growth rate, cached for [`TREND_TTL`].
    async fn volume_trend(&self, stats: &VolumeStats, now: i64) -> VolumeTrend {
        if let Some(cached) = self.inner.volume_trends.lock().get(&stats.mount_point)
            && cached.computed.elapsed() < TREND_TTL
        {
            return cached.clone();
        }
        let samples = self
            .volume_history(&stats.mount_point, now - TREND_SECS)
            .await
            .unwrap_or_default();
        let points: Vec<(i64, u64)> = samples.iter().map(|s| (s.at, s.used())).collect();
        let (trend, span_secs) = trend_points(&points, (now, stats.used()));
        let trend = VolumeTrend {
            computed: Instant::now(),
            trend,
            span_secs,
            bytes_per_sec: growth_rate(&points, now),
        };
        self.inner
            .volume_trends
            .lock()
            .insert(stats.mount_point.clone(), trend.clone());
        trend
    }

    /// Largest growth since `since` across the scoped roots.
    async fn growth(&self, scope: &[ScopedRoot], since: i64) -> Vec<GrowthItem> {
        let mut out: BTreeMap<PathBuf, GrowthItem> = BTreeMap::new();
        for root in scope {
            for entry in self
                .top_growth(&root.path, since, TOP_GROWTH)
                .await
                .unwrap_or_default()
            {
                out.entry(entry.path.clone()).or_insert(GrowthItem {
                    path: entry.path,
                    from: entry.from,
                    to: entry.to,
                });
            }
        }
        let mut out: Vec<GrowthItem> = out.into_values().filter(|g| g.delta() > 0).collect();
        out.sort_by_key(|g| std::cmp::Reverse(g.delta()));
        // Drop entries already explained by a listed descendant or ancestor with the same
        // growth (rollups record every level of a growing path).
        let mut kept: Vec<GrowthItem> = Vec::new();
        for item in out {
            let redundant = kept.iter().any(|k| {
                (k.path.starts_with(&item.path) || item.path.starts_with(&k.path))
                    && k.delta().abs_diff(item.delta()) <= item.delta().unsigned_abs() / 10
            });
            if !redundant {
                kept.push(item);
            }
            if kept.len() == TOP_GROWTH {
                break;
            }
        }
        kept
    }

    /// Journal totals for the last week, alerts, and daemon health.
    async fn activity(&self, now: i64) -> ActivitySummary {
        let entries = self.journal(1_000).await.unwrap_or_default();
        let undone: BTreeSet<&str> = entries.iter().filter_map(|e| e.undoes.as_deref()).collect();
        let mut activity = ActivitySummary::default();
        for entry in &entries {
            if entry.outcome != JournalOutcome::Done
                || entry.undoes.is_some()
                || undone.contains(entry.id.as_str())
            {
                continue;
            }
            let freed = match entry.performed {
                Some(PerformedAction::Trash | PerformedAction::Undo) | None => 0,
                Some(_) => entry
                    .measured_freed
                    .and_then(|m| u64::try_from(m).ok())
                    .filter(|m| *m > 0)
                    .unwrap_or(entry.size),
            };
            if activity.last_action.is_none() {
                activity.last_action = Some(LastAction {
                    id: entry.id.clone(),
                    at: entry.at,
                    path: entry.path.clone(),
                    freed,
                    reversible: entry.undo.is_some(),
                });
            }
            if entry.at >= now - 7 * 86_400 {
                activity.freed_week += freed;
                activity.actions_week += 1;
            }
        }
        let alerts = self.alerts();
        activity.alerts = alerts.len() as u64;
        activity.top_alert = alerts
            .iter()
            .max_by_key(|a| (severity_rank(a.severity), a.at))
            .map(|a| a.title.clone());
        for root in self.root_statuses() {
            activity.full_scans += root.full_scans;
            activity.refreshes += root.refreshes;
        }
        activity.watchers.clone_from(&self.inner.watchers.lock());
        activity
    }
}

const fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Info => 0,
        Severity::Warning => 1,
        Severity::Critical => 2,
    }
}

/// A loaded root in the dashboard's scope.
struct ScopedRoot {
    path: PathBuf,
    tree: std::sync::Arc<Tree>,
    classes: BTreeMap<PathBuf, Classification>,
    scanned_at: Option<i64>,
    scanning: bool,
}

/// A reclaimable, top-level classified path and the root it was taken from.
struct Item<'a> {
    path: &'a Path,
    class: &'a Classification,
    root: usize,
}

/// Reclaimable classified paths, each from the deepest root containing it, without paths
/// nested inside another listed path.
fn collect_items(scope: &[ScopedRoot]) -> Vec<Item<'_>> {
    let deepest = |path: &Path| scope.iter().rposition(|r| path.starts_with(&r.path));
    let mut items: Vec<Item<'_>> = scope
        .iter()
        .enumerate()
        .flat_map(|(index, root)| {
            root.classes
                .iter()
                .filter(move |(path, class)| class.is_reclaimable() && deepest(path) == Some(index))
                .map(move |(path, class)| Item {
                    path,
                    class,
                    root: index,
                })
        })
        .collect();
    items.sort_by(|a, b| a.path.cmp(b.path));
    let mut top: Vec<Item<'_>> = Vec::with_capacity(items.len());
    for item in items {
        if !top.last().is_some_and(|t| item.path.starts_with(t.path)) {
            top.push(item);
        }
    }
    top
}

/// Paths an automatic clean (the default policy selection) would pick.
fn safe_paths(items: &[Item<'_>]) -> BTreeSet<PathBuf> {
    let candidates: Vec<Candidate> = items
        .iter()
        .map(|i| Candidate::new(i.path, None, i.class.clone()))
        .collect();
    bclean_policy::select(
        &SelectSpec::default(),
        &BudgetSpec::default(),
        &candidates,
        None,
        None,
    )
    .into_iter()
    .map(|c| c.path.clone())
    .collect()
}

fn summarize_items(
    summary: &mut DashboardSummary,
    items: &[Item<'_>],
    safe: &BTreeSet<PathBuf>,
    scope: &[ScopedRoot],
) {
    let mut kinds: BTreeMap<Option<EphemeralKind>, KindBucket> = BTreeMap::new();
    let mut categories: BTreeMap<Option<String>, CategoryBucket> = BTreeMap::new();
    let mut projects: BTreeMap<PathBuf, ProjectSummary> = BTreeMap::new();
    let mut project_entries: BTreeMap<PathBuf, Vec<(u64, String)>> = BTreeMap::new();
    let mut marker_cache: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let reclaim = &mut summary.reclaimable;
    for item in items {
        let class = item.class;
        let size = class.size;
        let is_safe = safe.contains(item.path);
        let safe_bytes = if is_safe { size } else { 0 };
        reclaim.total += size;
        reclaim.items += 1;
        if is_safe {
            reclaim.safe += size;
            reclaim.safe_items += 1;
        }
        match (class.ephemeral.is_some(), class.staleness.stale) {
            (true, true) => reclaim.ephemeral_stale += size,
            (true, false) => reclaim.ephemeral_fresh += size,
            (false, _) => reclaim.durable_stale += size,
        }
        let kind = class.ephemeral.as_ref().map(|e| e.kind);
        let bucket = kinds.entry(kind).or_insert(KindBucket {
            kind,
            bytes: 0,
            items: 0,
            safe: 0,
        });
        bucket.bytes += size;
        bucket.items += 1;
        bucket.safe += safe_bytes;
        let bucket = categories
            .entry(class.category.clone())
            .or_insert_with(|| CategoryBucket {
                category: class.category.clone(),
                bytes: 0,
                items: 0,
                safe: 0,
            });
        bucket.bytes += size;
        bucket.items += 1;
        bucket.safe += safe_bytes;

        let root = &scope[item.root];
        if let Some(project) = project_of(&root.tree, &root.path, item.path, &mut marker_cache) {
            let entry = projects
                .entry(project.clone())
                .or_insert_with(|| ProjectSummary {
                    path: project.clone(),
                    bytes: 0,
                    items: 0,
                    entries: Vec::new(),
                    paths: Vec::new(),
                    last_activity: None,
                });
            entry.bytes += size;
            entry.items += 1;
            entry.paths.push(item.path.to_path_buf());
            entry.last_activity = entry.last_activity.max(class.staleness.last_activity);
            project_entries.entry(project).or_default().push((
                size,
                item.path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            ));
        }
    }

    summary.by_kind = kinds.into_values().collect();
    summary.by_kind.sort_by_key(|b| std::cmp::Reverse(b.bytes));
    summary.by_category = categories.into_values().collect();
    summary
        .by_category
        .sort_by_key(|b| std::cmp::Reverse(b.bytes));
    summary.top_items = top_items(items, safe);
    let mut safe_items: Vec<&Item<'_>> = items.iter().filter(|i| safe.contains(i.path)).collect();
    safe_items.sort_by_key(|i| std::cmp::Reverse(i.class.size));
    summary.reclaimable.safe_paths = safe_items.iter().map(|i| i.path.to_path_buf()).collect();
    summary.projects = top_projects(projects, project_entries);
}

/// The best candidates by score.
fn top_items(items: &[Item<'_>], safe: &BTreeSet<PathBuf>) -> Vec<DashboardItem> {
    let mut top: Vec<&Item<'_>> = items.iter().collect();
    top.sort_by(|a, b| b.class.score.total_cmp(&a.class.score));
    top.into_iter()
        .take(TOP_ITEMS)
        .map(|item| DashboardItem {
            path: item.path.to_path_buf(),
            size: item.class.size,
            category: item.class.category.clone(),
            kind: item.class.ephemeral.as_ref().map(|e| e.kind),
            idle_secs: item.class.staleness.idle_secs,
            stale: item.class.staleness.stale,
            score: item.class.score,
            safe: safe.contains(item.path),
        })
        .collect()
}

/// The largest projects, each with the names of its largest entries.
fn top_projects(
    projects: BTreeMap<PathBuf, ProjectSummary>,
    mut project_entries: BTreeMap<PathBuf, Vec<(u64, String)>>,
) -> Vec<ProjectSummary> {
    let mut projects: Vec<ProjectSummary> = projects
        .into_values()
        .map(|mut p| {
            if let Some(mut names) = project_entries.remove(&p.path) {
                names.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
                let mut seen = BTreeSet::new();
                p.entries = names
                    .into_iter()
                    .filter(|(_, n)| seen.insert(n.clone()))
                    .map(|(_, n)| n)
                    .take(4)
                    .collect();
            }
            p
        })
        .collect();
    projects.sort_by_key(|p| std::cmp::Reverse(p.bytes));
    projects.truncate(TOP_PROJECTS);
    projects
}

/// The nearest directory above `path` (inside `root`, excluding `root` itself) that holds a
/// project marker, looked up in the in-memory tree.
fn project_of(
    tree: &Tree,
    root: &Path,
    path: &Path,
    cache: &mut BTreeMap<PathBuf, bool>,
) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    let managed = |dir: &Path| {
        dir.strip_prefix(root).is_ok_and(|rel| {
            rel.components().any(|c| {
                c.as_os_str()
                    .to_str()
                    .is_some_and(|name| NOT_PROJECT_PARENTS.contains(&name))
            })
        })
    };
    if managed(dir) {
        return None;
    }
    while dir.starts_with(root) && dir != root {
        let is_project = *cache.entry(dir.to_path_buf()).or_insert_with(|| {
            tree.find(dir).is_some_and(|id| {
                tree.children(id).any(|child| {
                    tree.name(child)
                        .to_str()
                        .is_some_and(|n| PROJECT_MARKERS.contains(&n))
                })
            })
        });
        if is_project {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
    None
}

fn volume_summary(
    stats: VolumeStats,
    scope: &[ScopedRoot],
    items: &[Item<'_>],
    trend: VolumeTrend,
) -> VolumeSummary {
    let device = device_of(Path::new(&stats.mount_point));
    let on_volume = |path: &Path| {
        device.map_or_else(
            || path.starts_with(&stats.mount_point),
            |dev| device_of(path) == Some(dev),
        )
    };
    let counted: Vec<usize> = (0..scope.len())
        .filter(|&i| on_volume(&scope[i].path))
        .collect();
    // Outermost counted roots only, so nested roots are not counted twice.
    let indexed = counted
        .iter()
        .filter(|&&i| {
            !counted
                .iter()
                .any(|&j| j != i && scope[i].path.starts_with(&scope[j].path))
        })
        .map(|&i| scope[i].tree.size(NodeId::ROOT, SizeMode::Allocated))
        .sum();
    let reclaimable = items
        .iter()
        .filter(|item| counted.contains(&item.root))
        .map(|item| item.class.size)
        .sum();
    let growth_per_day = trend.bytes_per_sec.map(|rate| {
        #[allow(clippy::cast_possible_truncation)] // bytes/day fits comfortably in i64
        let per_day = (rate * 86_400.0).round() as i64;
        per_day
    });
    let full_in_secs = trend.bytes_per_sec.filter(|rate| *rate > 0.0).map(|rate| {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )] // a positive, finite estimate in seconds
        let secs = (stats.available as f64 / rate) as u64;
        secs
    });
    VolumeSummary {
        stats,
        indexed,
        reclaimable,
        trend: trend.trend,
        trend_span_secs: trend.span_secs,
        growth_per_day,
        full_in_secs,
    }
}

#[cfg(unix)]
fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
const fn device_of(_path: &Path) -> Option<u64> {
    None
}

/// Resamples `(at, used)` points to at most [`TREND_POINTS`] evenly spaced values ending
/// with `latest`. Returns the values and the time they span.
fn trend_points(points: &[(i64, u64)], latest: (i64, u64)) -> (Vec<u64>, u64) {
    let Some(&(first, _)) = points.first() else {
        return (vec![latest.1], 0);
    };
    let span = (latest.0 - first).max(1);
    let buckets = TREND_POINTS.min(points.len().max(1));
    let mut out = vec![None; buckets];
    for &(at, used) in points {
        let offset = u64::try_from(at - first).unwrap_or(0);
        let index = usize::try_from(offset * buckets as u64 / u64::try_from(span).unwrap_or(1))
            .unwrap_or(0)
            .min(buckets - 1);
        out[index] = Some(used);
    }
    let mut last = points[0].1;
    let mut values: Vec<u64> = out
        .into_iter()
        .map(|v| {
            last = v.unwrap_or(last);
            last
        })
        .collect();
    if let Some(end) = values.last_mut() {
        *end = latest.1;
    }
    (values, u64::try_from(span).unwrap_or(0))
}

/// Least-squares growth of used bytes per second over the last [`RATE_SECS`], when the
/// samples span at least [`MIN_RATE_SPAN_SECS`].
#[allow(clippy::cast_precision_loss)] // regression over byte counts; f64 precision suffices
fn growth_rate(points: &[(i64, u64)], now: i64) -> Option<f64> {
    let recent: Vec<(f64, f64)> = points
        .iter()
        .filter(|(at, _)| *at >= now - RATE_SECS)
        .map(|&(at, used)| ((at - now) as f64, used as f64))
        .collect();
    let (first, last) = (recent.first()?, recent.last()?);
    if recent.len() < 3 || last.0 - first.0 < MIN_RATE_SPAN_SECS as f64 {
        return None;
    }
    let n = recent.len() as f64;
    let mean_x = recent.iter().map(|p| p.0).sum::<f64>() / n;
    let mean_y = recent.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (x, y) in &recent {
        sxy = (x - mean_x).mul_add(y - mean_y, sxy);
        sxx = (x - mean_x).mul_add(x - mean_x, sxx);
    }
    (sxx > 0.0).then(|| sxy / sxx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trend_resamples_and_ends_at_latest() {
        let points: Vec<(i64, u64)> = (0..1_000_u64)
            .map(|i| (i64::try_from(i * 60).unwrap(), 1_000 + i))
            .collect();
        let (trend, span) = trend_points(&points, (60_000, 5_000));
        assert_eq!(trend.len(), TREND_POINTS);
        assert!(trend.windows(2).all(|w| w[0] <= w[1]), "{trend:?}");
        assert_eq!(*trend.last().unwrap(), 5_000);
        assert_eq!(span, 60_000);
        assert_eq!(trend_points(&[], (10, 7)), (vec![7], 0));
    }

    #[test]
    fn projects_are_marker_dirs_outside_tool_managed_trees() {
        use bclean_tree::{EntryKind, EntryMeta};
        use std::ffi::OsStr;
        let mut tree = Tree::new("/h", EntryMeta::dir());
        let dir = |tree: &mut Tree, parent, name: &str| {
            tree.add_child_raw(parent, OsStr::new(name), EntryMeta::dir())
        };
        let file = |tree: &mut Tree, parent, name: &str| {
            tree.add_child_raw(
                parent,
                OsStr::new(name),
                EntryMeta::new(EntryKind::File, 1, 1, 0),
            );
        };
        let github = dir(&mut tree, NodeId::ROOT, "GitHub");
        let project = dir(&mut tree, github, "p");
        file(&mut tree, project, "Cargo.toml");
        let target = dir(&mut tree, project, "target");
        dir(&mut tree, target, "debug");
        let cache = dir(&mut tree, NodeId::ROOT, ".cache");
        let pkg = dir(&mut tree, cache, "pkg");
        file(&mut tree, pkg, "package.json");
        dir(&mut tree, pkg, "node_modules");
        file(&mut tree, NodeId::ROOT, "flake.nix");

        let mut cache_map = BTreeMap::new();
        let root = Path::new("/h");
        assert_eq!(
            project_of(&tree, root, Path::new("/h/GitHub/p/target"), &mut cache_map),
            Some(PathBuf::from("/h/GitHub/p"))
        );
        assert_eq!(
            project_of(
                &tree,
                root,
                Path::new("/h/GitHub/p/target/debug"),
                &mut cache_map
            ),
            Some(PathBuf::from("/h/GitHub/p"))
        );
        // Inside a cache: not a project, even with a manifest.
        assert_eq!(
            project_of(
                &tree,
                root,
                Path::new("/h/.cache/pkg/node_modules"),
                &mut cache_map
            ),
            None
        );
        // The root itself (a dotfiles repo, say) never counts.
        assert_eq!(
            project_of(&tree, root, Path::new("/h/GitHub"), &mut cache_map),
            None
        );
    }

    #[test]
    fn growth_rate_needs_enough_history() {
        let now = 1_000_000;
        // 1 MB per hour for a day.
        let points: Vec<(i64, u64)> = (0..=24_u64)
            .map(|h: u64| {
                (
                    now - (24 - i64::try_from(h).unwrap()) * 3_600,
                    10_000_000 + h * 1_000_000,
                )
            })
            .collect();
        let rate = growth_rate(&points, now).unwrap();
        assert!(rate.mul_add(3_600.0, -1_000_000.0).abs() < 1.0, "{rate}");
        assert_eq!(growth_rate(&points[20..], now), None, "only 4 hours");
    }
}
