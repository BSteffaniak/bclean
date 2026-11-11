//! Command implementations.

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use bclean_actions::{ExecutionReport, ItemOutcome, JournalEntry, Plan};
use bclean_engine::{Selection, SortOrder, latest_undoable, sort_items};
use bclean_model::{ByteSize, DurationSpec, SizeModeSpec, VolumeStats};
use bclean_protocol::{ClassifiedPath, DaemonStatus, Request, Response};
use bclean_staleness::unix_now;
use bclean_tree::{EntryKind, NodeFlags, NodeId, SizeMode, Tree};
use serde_json::json;

use crate::args::{self, Format, SortBy};
use crate::conn::{Conn, Ctx, VERSION, unexpected};
use crate::output::{self, Align, Table, action_name, age, ago, print_json, quadrant, size};

/// Absolute form of `path` (default: the current directory).
///
/// # Errors
///
/// * If the current directory cannot be determined
pub fn abs(path: Option<&Path>) -> anyhow::Result<PathBuf> {
    let path = path.map_or_else(std::env::current_dir, |p| Ok(p.to_path_buf()))?;
    Ok(std::path::absolute(path)?)
}

/// Converts CLI selection flags.
///
/// # Errors
///
/// * If `--stale` or `--min-size` don't parse
pub fn selection(s: &args::Selection) -> anyhow::Result<Selection> {
    let stale = match s.stale.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(age) => Some(Some(
            age.parse::<DurationSpec>()
                .map_err(|e| anyhow!("invalid --stale age {age:?}: {e}"))?
                .as_secs(),
        )),
    };
    let min_size = match &s.min_size {
        None => 0,
        Some(v) => v
            .parse::<ByteSize>()
            .map_err(|e| anyhow!("invalid --min-size {v:?}: {e}"))?
            .bytes(),
    };
    Ok(Selection {
        ephemeral: s.ephemeral,
        stale,
        rules: s.rules.clone(),
        categories: s.categories.clone(),
        min_size,
        auto_only: s.auto_only,
    })
}

const fn sort_order(by: SortBy) -> SortOrder {
    match by {
        SortBy::Score => SortOrder::Score,
        SortBy::Size => SortOrder::Size,
        SortBy::Age => SortOrder::Age,
        SortBy::Path => SortOrder::Path,
    }
}

async fn classifications(conn: &mut Conn, path: &Path) -> anyhow::Result<Vec<ClassifiedPath>> {
    match conn
        .request(Request::Classifications {
            root: path.to_path_buf(),
        })
        .await?
    {
        Response::Classifications { items } => Ok(items),
        other => Err(unexpected(&other)),
    }
}

fn size_mode(ctx: &Ctx) -> SizeMode {
    match ctx.settings.config().scan.size_mode {
        SizeModeSpec::Allocated => SizeMode::Allocated,
        SizeModeSpec::Apparent => SizeMode::Apparent,
    }
}

// ----- scan -----

/// `bclean scan`.
///
/// # Errors
///
/// * If scanning fails
pub async fn scan(
    ctx: &Ctx,
    path: Option<&Path>,
    format: Format,
    top: usize,
    force: bool,
) -> anyhow::Result<()> {
    let path = abs(path)?;
    let mut conn = ctx.connect().await?;
    let started = Instant::now();
    let snapshot = conn.scan(&path, force).await?;
    let elapsed = started.elapsed();
    let tree = conn.load_tree(&snapshot)?;
    let node = tree
        .find(&path)
        .ok_or_else(|| anyhow!("{} is not in the scanned tree", path.display()))?;
    let classes = classifications(&mut conn, &path).await?;
    print_scan(ctx, &tree, node, &classes, elapsed, format, top)
}

fn print_scan(
    ctx: &Ctx,
    tree: &Tree,
    node: NodeId,
    classes: &[ClassifiedPath],
    elapsed: Duration,
    format: Format,
    top: usize,
) -> anyhow::Result<()> {
    let mode = size_mode(ctx);
    let info = tree.info(node);
    let ephemeral: Vec<&ClassifiedPath> = classes
        .iter()
        .filter(|c| c.classification.ephemeral.is_some())
        .collect();
    let stale: Vec<&ClassifiedPath> = classes
        .iter()
        .filter(|c| c.classification.staleness.stale)
        .collect();
    let sum = |v: &[&ClassifiedPath]| v.iter().map(|c| c.classification.size).sum::<u64>();
    let mut children = tree.sorted_children(node, bclean_tree::SortKey::Size(mode));
    children.truncate(top);
    match format {
        Format::Json => print_json(&json!({
            "root": tree.path(node),
            "total_allocated": info.total_allocated,
            "total_apparent": info.total_apparent,
            "items": info.items,
            "elapsed_secs": elapsed.as_secs_f64(),
            "reclaimable": {
                "ephemeral": { "items": ephemeral.len(), "bytes": sum(&ephemeral) },
                "stale": { "items": stale.len(), "bytes": sum(&stale) },
            },
            "top": children.iter().map(|&c| {
                let ci = tree.info(c);
                json!({
                    "path": tree.path(c),
                    "size": tree.size(c, mode),
                    "items": ci.items,
                    "dir": ci.kind == EntryKind::Dir,
                    "summarized": ci.flags.contains(NodeFlags::SUMMARIZED),
                })
            }).collect::<Vec<_>>(),
        })),
        Format::Table => {
            println!(
                "{}  {}  ({} items, scanned in {:.2?})",
                tree.path(node).display(),
                size(tree.size(node, mode)),
                info.items,
                elapsed
            );
            println!(
                "reclaimable: ephemeral {} in {} item(s); stale {} in {} item(s)",
                size(sum(&ephemeral)),
                ephemeral.len(),
                size(sum(&stale)),
                stale.len()
            );
            println!();
            let mut table = Table::new(&[
                ("SIZE", Align::Right),
                ("ITEMS", Align::Right),
                ("NAME", Align::Left),
            ]);
            for c in children {
                let ci = tree.info(c);
                let mut name = tree.name(c).to_string_lossy().into_owned();
                if ci.kind == EntryKind::Dir {
                    name.push('/');
                }
                if ci.flags.contains(NodeFlags::SUMMARIZED) {
                    name.push_str("  [summarized]");
                }
                table.row(vec![size(tree.size(c, mode)), ci.items.to_string(), name]);
            }
            print!("{}", table.render());
            Ok(())
        }
    }
}

// ----- ls -----

/// `bclean ls`.
///
/// # Errors
///
/// * If scanning or classification fails
pub async fn ls(
    ctx: &Ctx,
    path: Option<&Path>,
    sel: &args::Selection,
    sort: SortBy,
    format: Format,
    limit: Option<usize>,
) -> anyhow::Result<()> {
    let path = abs(path)?;
    let selection = selection(sel)?;
    let mut conn = ctx.connect().await?;
    if conn.is_local() {
        conn.scan(&path, true).await?;
    }
    let mut items = selection.apply(classifications(&mut conn, &path).await?);
    sort_items(&mut items, sort_order(sort));
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    match format {
        Format::Json => print_json(&items),
        Format::Table => {
            print_items(&items, &path);
            Ok(())
        }
    }
}

fn print_items(items: &[ClassifiedPath], base: &Path) {
    if items.is_empty() {
        println!("No classified items under {}.", base.display());
        return;
    }
    let mut table = Table::new(&[
        ("SIZE", Align::Right),
        ("SCORE", Align::Right),
        ("IDLE", Align::Right),
        ("CLASS", Align::Left),
        ("RULE", Align::Left),
        ("ACTION", Align::Left),
        ("PATH", Align::Left),
    ]);
    let mut total = 0_u64;
    for item in items {
        let c = &item.classification;
        total += c.size;
        table.row(vec![
            size(c.size),
            format_score(c.score),
            c.staleness.idle_secs.map_or_else(|| "-".to_owned(), age),
            quadrant(c).to_owned(),
            c.rule_id.clone().unwrap_or_else(|| "-".to_owned()),
            action_name(&c.action).to_owned(),
            display_path(&item.path, base),
        ]);
    }
    print!("{}", table.render());
    println!("{} item(s), {} total", items.len(), size(total));
}

fn format_score(score: f64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let mib = score / (1u64 << 20) as f64;
    format!("{mib:.1}")
}

fn display_path(path: &Path, base: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => path.display().to_string(),
    }
}

// ----- clean -----

/// `bclean clean`.
///
/// # Errors
///
/// * If planning or execution fails, or confirmation is required but unavailable
pub async fn clean(
    ctx: &Ctx,
    paths: &[PathBuf],
    sel: &args::Selection,
    dry_run: bool,
    yes: bool,
    format: Format,
) -> anyhow::Result<bool> {
    let selection = selection(sel)?;
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![abs(None)?]
    } else {
        paths
            .iter()
            .map(|p| abs(Some(p)))
            .collect::<anyhow::Result<_>>()?
    };
    let mut conn = ctx.connect().await?;
    let mut chosen: Vec<PathBuf> = Vec::new();
    for root in &roots {
        if conn.is_local() {
            conn.scan(root, true).await?;
        }
        let items = selection.apply(classifications(&mut conn, root).await?);
        chosen.extend(items.into_iter().map(|i| i.path));
    }
    chosen.sort();
    chosen.dedup();
    // Never plan a path together with one of its descendants.
    let mut top: Vec<PathBuf> = Vec::new();
    for path in chosen {
        if !top.iter().any(|t| path.starts_with(t)) {
            top.push(path);
        }
    }
    if top.is_empty() {
        if format == Format::Json {
            print_json(&json!({ "plan": Plan::default(), "report": null }))?;
        } else {
            println!("Nothing to clean.");
        }
        return Ok(true);
    }
    let plan: Plan = match conn.request(Request::Plan { paths: top }).await? {
        Response::Plan { plan } => serde_json::from_value(plan)?,
        other => return Err(unexpected(&other)),
    };
    if format == Format::Table {
        print_plan(&plan);
    }
    let estimated = plan.estimated_bytes();
    let confirm_above = ctx.settings.config().protect.confirm_above.bytes();
    if !dry_run {
        let needs_confirm = !yes || estimated > confirm_above;
        if needs_confirm && !confirm(estimated, confirm_above, yes)? {
            println!("Aborted.");
            return Ok(true);
        }
    }
    let report: ExecutionReport = match conn
        .request(Request::Execute {
            plan: serde_json::to_value(&plan)?,
            dry_run,
        })
        .await?
    {
        Response::Actions { value } => serde_json::from_value(value)?,
        other => return Err(unexpected(&other)),
    };
    let ok = report
        .results
        .iter()
        .all(|r| !matches!(r.outcome, ItemOutcome::Failed(_)));
    match format {
        Format::Json => print_json(&json!({ "plan": plan, "report": report }))?,
        Format::Table => print_report(&report),
    }
    Ok(ok)
}

fn confirm(estimated: u64, confirm_above: u64, yes: bool) -> anyhow::Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        if yes {
            bail!(
                "plan frees ~{} which exceeds protect.confirm_above ({}); interactive confirmation is required",
                size(estimated),
                size(confirm_above)
            );
        }
        bail!("confirmation required: re-run with --yes (or --dry-run to preview)");
    }
    print!("Proceed with cleaning ~{}? [y/N] ", size(estimated));
    std::io::stdout().flush()?;
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "YES"))
}

fn print_plan(plan: &Plan) {
    let mut table = Table::new(&[
        ("ACTION", Align::Left),
        ("SIZE", Align::Right),
        ("RULE", Align::Left),
        ("PATH", Align::Left),
    ]);
    for item in &plan.items {
        table.row(vec![
            action_name(&item.action).to_owned(),
            size(item.size),
            item.rule_id.clone().unwrap_or_else(|| "-".to_owned()),
            item.path.display().to_string(),
        ]);
    }
    println!(
        "Plan: {} item(s), ~{}",
        plan.len(),
        size(plan.estimated_bytes())
    );
    print!("{}", table.render());
}

fn outcome_name(outcome: &ItemOutcome) -> (String, Option<String>) {
    match outcome {
        ItemOutcome::Done => ("done".to_owned(), None),
        ItemOutcome::DryRun => ("dry-run".to_owned(), None),
        ItemOutcome::Skipped(reason) => ("skipped".to_owned(), Some(reason.to_string())),
        ItemOutcome::Failed(e) => ("failed".to_owned(), Some(e.clone())),
        ItemOutcome::Cancelled => ("cancelled".to_owned(), None),
    }
}

fn print_report(report: &ExecutionReport) {
    let mut table = Table::new(&[
        ("OUTCOME", Align::Left),
        ("PATH", Align::Left),
        ("DETAIL", Align::Left),
    ]);
    for r in &report.results {
        let (name, detail) = outcome_name(&r.outcome);
        table.row(vec![
            name,
            r.path.display().to_string(),
            detail
                .or_else(|| r.detail.clone())
                .unwrap_or_default()
                .replace('\n', " "),
        ]);
    }
    println!();
    print!("{}", table.render());
    let verb = if report.dry_run {
        "would free"
    } else {
        "freed"
    };
    let measured = report
        .measured_freed
        .map(|m| format!(" (measured {})", output::signed_size(m)))
        .unwrap_or_default();
    println!("{verb} ~{}{measured}", size(report.estimated_freed));
}

// ----- explain -----

/// `bclean explain`.
///
/// # Errors
///
/// * If the path cannot be classified
pub async fn explain(ctx: &Ctx, path: &Path, format: Format) -> anyhow::Result<()> {
    let path = abs(Some(path))?;
    let mut conn = ctx.connect().await?;
    let (item, extra) = match &conn {
        Conn::Local(engine) => {
            let e = engine.explain(&path).await?;
            let mut extra = Vec::new();
            if let Some(enc) = &e.enclosing {
                extra.push(format!(
                    "inside {} (rule {})",
                    enc.path.display(),
                    enc.rule_id
                ));
            }
            for miss in &e.near_misses {
                extra.push(format!("near miss {}: {}", miss.rule_id, miss.reason));
            }
            (
                ClassifiedPath {
                    path: e.path,
                    classification: e.classification,
                },
                extra,
            )
        }
        Conn::Daemon(_) => match conn.request(Request::Explain { path }).await? {
            Response::Explain(item) => (item, Vec::new()),
            other => return Err(unexpected(&other)),
        },
    };
    if format == Format::Json {
        return print_json(&item);
    }
    let c = &item.classification;
    let now = unix_now();
    let mut table = Table::new(&[("FIELD", Align::Left), ("VALUE", Align::Left)]);
    let mut kv = |k: &str, v: String| table.row(vec![k.to_owned(), v]);
    kv("path", item.path.display().to_string());
    kv(
        "rule",
        c.rule_id.clone().unwrap_or_else(|| "(none)".to_owned()),
    );
    kv(
        "category",
        c.category.clone().unwrap_or_else(|| "-".to_owned()),
    );
    kv(
        "ephemeral",
        c.ephemeral.map_or_else(
            || "no (durable)".to_owned(),
            |e| format!("{:?} (regen {:?})", e.kind, e.regen_cost),
        ),
    );
    kv("class", quadrant(c).to_owned());
    kv("confidence", format!("{:?}", c.confidence));
    kv("size", size(c.size));
    kv("score", format_score(c.score));
    kv(
        "stale",
        format!(
            "{} (idle {}, threshold {})",
            c.staleness.stale,
            c.staleness
                .idle_secs
                .map_or_else(|| "unknown".to_owned(), age),
            age(c.staleness.threshold_secs)
        ),
    );
    kv(
        "action",
        format!("{} (auto={})", action_name(&c.action), c.auto),
    );
    for s in &c.staleness.signals {
        kv(
            &format!("signal {:?}", s.kind),
            format!(
                "{}{} {}",
                s.last_activity
                    .map_or_else(|| "-".to_owned(), |a| ago(a, now)),
                if s.forces_stale {
                    " (forces stale)"
                } else {
                    ""
                },
                s.detail
            ),
        );
    }
    for r in &c.reasons {
        kv(&format!("reason {}", r.code), r.message.clone());
    }
    for e in extra {
        kv("note", e);
    }
    print!("{}", table.render());
    Ok(())
}

// ----- status -----

fn print_volumes(volumes: &[VolumeStats]) {
    let mut table = Table::new(&[
        ("MOUNT", Align::Left),
        ("FS", Align::Left),
        ("SIZE", Align::Right),
        ("USED", Align::Right),
        ("AVAIL", Align::Right),
        ("USE%", Align::Right),
    ]);
    for v in volumes {
        table.row(vec![
            v.mount_point.clone(),
            v.fs_type.clone(),
            size(v.total),
            size(v.used()),
            size(v.available),
            format!("{:.0}%", v.used_fraction() * 100.0),
        ]);
    }
    print!("{}", table.render());
}

fn print_reclaimable(summary: &bclean_model::DashboardSummary) {
    let r = &summary.reclaimable;
    if r.items == 0 {
        println!("reclaimable: nothing found in the index yet");
        return;
    }
    println!(
        "reclaimable: {} in {} items ({} safe to clean now, {} to review; `bclean` opens the dashboard)",
        size(r.total),
        r.items,
        size(r.safe),
        size(r.review())
    );
    let kinds: Vec<String> = summary
        .by_kind
        .iter()
        .take(4)
        .map(|b| format!("{} {}", bclean_model::kind_label(b.kind), size(b.bytes)))
        .collect();
    println!("  by kind: {}", kinds.join(" · "));
    for volume in &summary.volumes {
        if let (Some(rate), Some(full)) = (volume.growth_per_day, volume.full_in_secs)
            && rate > 0
        {
            println!(
                "  {}: growing {}/day, full in ~{}",
                volume.stats.mount_point,
                size(rate.unsigned_abs()),
                age(full)
            );
        }
    }
}

fn print_daemon_status(status: &DaemonStatus) {
    let now = unix_now();
    println!(
        "daemon: pid {} version {} up {} | policies {} | active alerts {} | watchers {}",
        status.pid,
        status.version,
        age(u64::try_from(now - status.started_at).unwrap_or(0)),
        status.policies,
        status.active_alerts,
        if status.watchers.is_empty() {
            "-".to_owned()
        } else {
            status.watchers.join(",")
        }
    );
    if status.roots.is_empty() {
        return;
    }
    let mut table = Table::new(&[
        ("ROOT", Align::Left),
        ("SIZE", Align::Right),
        ("NODES", Align::Right),
        ("GEN", Align::Right),
        ("FULL SCANS", Align::Right),
        ("REFRESHES", Align::Right),
        ("SCANNED", Align::Left),
        ("STATE", Align::Left),
    ]);
    for r in &status.roots {
        table.row(vec![
            r.root.display().to_string(),
            size(r.total_allocated),
            r.nodes.to_string(),
            r.generation.to_string(),
            r.full_scans.to_string(),
            r.refreshes.to_string(),
            r.scanned_at
                .map_or_else(|| "never".to_owned(), |t| ago(t, now)),
            r.progress.map_or_else(
                || "idle".to_owned(),
                |p| format!("scanning ({} files)", p.files),
            ),
        ]);
    }
    print!("{}", table.render());
}

/// `bclean status`.
///
/// # Errors
///
/// * If the engine/daemon cannot be reached
pub async fn status(ctx: &Ctx, format: Format) -> anyhow::Result<()> {
    let mut conn = ctx.connect().await?;
    let volumes = match conn.request(Request::Volumes).await? {
        Response::Volumes { volumes } => volumes,
        other => return Err(unexpected(&other)),
    };
    let daemon = if conn.is_local() {
        None
    } else {
        match conn.request(Request::Status).await? {
            Response::Status(s) => Some(s),
            other => return Err(unexpected(&other)),
        }
    };
    let alerts = match conn.request(Request::Alerts).await? {
        Response::Alerts { alerts } => alerts,
        other => return Err(unexpected(&other)),
    };
    let dashboard = match conn
        .request(Request::Dashboard {
            roots: Vec::new(),
            one_filesystem: false,
            safe_after_secs: None,
        })
        .await?
    {
        Response::Dashboard(summary) => summary,
        other => return Err(unexpected(&other)),
    };
    if format == Format::Json {
        return print_json(&json!({
            "volumes": volumes,
            "daemon": daemon,
            "alerts": alerts,
            "dashboard": dashboard,
        }));
    }
    print_volumes(&volumes);
    println!();
    print_reclaimable(&dashboard);
    println!();
    match &daemon {
        Some(status) => print_daemon_status(status),
        None => println!("daemon: not used (--no-daemon)"),
    }
    if alerts.is_empty() {
        println!("alerts: none");
    } else {
        println!("alerts:");
        for a in &alerts {
            println!("  [{:?}] {}: {}", a.severity, a.title, a.body);
        }
    }
    Ok(())
}

// ----- history -----

/// `bclean history`.
///
/// # Errors
///
/// * If `--since` is invalid or the query fails
pub async fn history(
    ctx: &Ctx,
    path: Option<&Path>,
    since: &str,
    limit: usize,
    format: Format,
) -> anyhow::Result<()> {
    let path = abs(path)?;
    let window = since
        .parse::<DurationSpec>()
        .map_err(|e| anyhow!("invalid --since {since:?}: {e}"))?;
    let since_at = unix_now() - i64::try_from(window.as_secs()).unwrap_or(i64::MAX / 2);
    let mut conn = ctx.connect().await?;
    let entries = match conn
        .request(Request::TopGrowth {
            root: path.clone(),
            since: since_at,
            limit,
        })
        .await?
    {
        Response::Growth { entries } => entries,
        other => return Err(unexpected(&other)),
    };
    if format == Format::Json {
        return print_json(&entries);
    }
    if entries.is_empty() {
        println!(
            "No growth recorded under {} in the last {window} (the daemon records history every monitor.history_interval).",
            path.display()
        );
        return Ok(());
    }
    let mut table = Table::new(&[
        ("GROWTH", Align::Right),
        ("FROM", Align::Right),
        ("TO", Align::Right),
        ("PATH", Align::Left),
    ]);
    for e in &entries {
        table.row(vec![
            format!("+{}", size(e.to.saturating_sub(e.from))),
            size(e.from),
            size(e.to),
            display_path(&e.path, &path),
        ]);
    }
    print!("{}", table.render());
    Ok(())
}

// ----- journal / undo -----

async fn journal_entries(conn: &mut Conn, limit: usize) -> anyhow::Result<Vec<JournalEntry>> {
    match conn.request(Request::Journal { limit }).await? {
        Response::Actions { value } => Ok(serde_json::from_value(value)?),
        other => Err(unexpected(&other)),
    }
}

/// `bclean journal`.
///
/// # Errors
///
/// * If the journal cannot be read
pub async fn journal(ctx: &Ctx, limit: usize, format: Format) -> anyhow::Result<()> {
    let mut conn = ctx.connect().await?;
    let entries = journal_entries(&mut conn, limit).await?;
    if format == Format::Json {
        return print_json(&entries);
    }
    if entries.is_empty() {
        println!("The journal is empty.");
        return Ok(());
    }
    let now = unix_now();
    let mut table = Table::new(&[
        ("WHEN", Align::Right),
        ("ID", Align::Left),
        ("ACTION", Align::Left),
        ("OUTCOME", Align::Left),
        ("SIZE", Align::Right),
        ("UNDO", Align::Left),
        ("PATH", Align::Left),
    ]);
    for e in &entries {
        let action = e.performed.map_or_else(
            || action_name(&e.action).to_owned(),
            |p| format!("{p:?}").to_uppercase(),
        );
        table.row(vec![
            ago(e.at, now),
            e.id.clone(),
            action,
            format!("{:?}", e.outcome).to_uppercase(),
            size(e.size),
            if e.undo.is_some() { "yes" } else { "-" }.to_owned(),
            e.path.display().to_string(),
        ]);
    }
    print!("{}", table.render());
    Ok(())
}

/// `bclean undo`.
///
/// # Errors
///
/// * If there is nothing to undo or the undo fails
pub async fn undo(ctx: &Ctx, id: Option<&str>) -> anyhow::Result<()> {
    let mut conn = ctx.connect().await?;
    let id = if let Some(id) = id {
        id.to_owned()
    } else {
        let entries = journal_entries(&mut conn, usize::MAX).await?;
        latest_undoable(&entries)
            .map(|e| e.id.clone())
            .ok_or_else(|| anyhow!("nothing to undo: no reversible journal entries"))?
    };
    let record: JournalEntry = match conn.request(Request::Undo { id: id.clone() }).await? {
        Response::Actions { value } => serde_json::from_value(value)?,
        other => return Err(unexpected(&other)),
    };
    println!("Undid {id}: restored {}", record.path.display());
    Ok(())
}

// ----- rules / config -----

/// `bclean rules list`.
///
/// # Errors
///
/// * If JSON output fails
pub fn rules_list(ctx: &Ctx, format: Format) -> anyhow::Result<()> {
    let loaded = &ctx.settings.loaded;
    if format == Format::Json {
        return print_json(&loaded.config.rules);
    }
    let mut table = Table::new(&[
        ("ID", Align::Left),
        ("CATEGORY", Align::Left),
        ("KIND", Align::Left),
        ("ACTION", Align::Left),
        ("AUTO", Align::Left),
        ("ENABLED", Align::Left),
        ("ORIGIN", Align::Left),
    ]);
    for (id, rule) in &loaded.config.rules {
        table.row(vec![
            id.clone(),
            rule.category.clone().unwrap_or_else(|| "-".to_owned()),
            rule.ephemeral
                .map_or_else(|| "DURABLE".to_owned(), |e| format!("{:?}", e.kind)),
            action_name(&rule.action).to_owned(),
            rule.auto.to_string(),
            rule.enabled.to_string(),
            loaded
                .rule_origin(id)
                .map_or_else(|| "-".to_owned(), ToString::to_string),
        ]);
    }
    print!("{}", table.render());
    println!("{} rule(s)", loaded.config.rules.len());
    Ok(())
}

/// `bclean rules show`.
///
/// # Errors
///
/// * If the rule does not exist
pub fn rules_show(ctx: &Ctx, id: &str) -> anyhow::Result<()> {
    let loaded = &ctx.settings.loaded;
    let rule = loaded
        .config
        .rules
        .get(id)
        .ok_or_else(|| anyhow!("no rule named {id:?}"))?;
    if let Some(origin) = loaded.rule_origin(id) {
        println!("# from {origin}");
    }
    let mut doc = toml::Table::new();
    doc.insert(id.to_owned(), toml::Value::try_from(rule)?);
    let mut outer = toml::Table::new();
    outer.insert("rules".to_owned(), toml::Value::Table(doc));
    print!("{}", toml::to_string_pretty(&outer)?);
    Ok(())
}

/// `bclean rules test`.
///
/// # Errors
///
/// * If the path cannot be read
pub fn rules_test(ctx: &Ctx, path: &Path) -> anyhow::Result<()> {
    let path = abs(Some(path))?;
    let staleness = bclean_staleness::StalenessEngine::with_defaults(unix_now());
    let e = bclean_rules::Classifier::new(&ctx.settings.rules, &staleness)
        .explain(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    match &e.classification.rule_id {
        Some(rule) => println!(
            "{} matches rule {rule} ({}, action {})",
            path.display(),
            quadrant(&e.classification),
            action_name(&e.classification.action)
        ),
        None => println!("{}: no rule matches", path.display()),
    }
    if let Some(enc) = &e.enclosing {
        println!("  inside {} (rule {})", enc.path.display(), enc.rule_id);
    }
    for m in &e.near_misses {
        println!("  near miss {}: {}", m.rule_id, m.reason);
    }
    Ok(())
}

/// `bclean config show`.
///
/// # Errors
///
/// * If serialization fails
pub fn config_show(ctx: &Ctx) -> anyhow::Result<()> {
    for source in &ctx.settings.loaded.sources {
        println!("# layer: {source}");
    }
    print!("{}", toml::to_string_pretty(ctx.settings.config())?);
    Ok(())
}

/// `bclean config paths`.
///
/// # Errors
///
/// * If the global config path cannot be determined
pub fn config_paths(ctx: &Ctx) -> anyhow::Result<()> {
    let global = ctx.options.load.global_path()?;
    let project = ctx
        .settings
        .loaded
        .sources
        .iter()
        .find(|s| s.kind == bclean_config::LayerKind::Project)
        .and_then(|s| s.path.clone());
    let mut table = Table::new(&[("WHAT", Align::Left), ("PATH", Align::Left)]);
    let mut row = |k: &str, v: String| table.row(vec![k.to_owned(), v]);
    row(
        "global config",
        format!(
            "{}{}",
            global.display(),
            if global.is_file() { "" } else { " (missing)" }
        ),
    );
    row(
        "project config",
        project.map_or_else(|| "(none)".to_owned(), |p| p.display().to_string()),
    );
    if let Some(explicit) = &ctx.config_file {
        row("explicit config", explicit.display().to_string());
    }
    row("data dir", ctx.dirs.data.display().to_string());
    row(
        "snapshots",
        ctx.dirs.data.join("snapshots").display().to_string(),
    );
    row(
        "history db",
        ctx.dirs.data.join("history.sqlite3").display().to_string(),
    );
    row(
        "journal db",
        ctx.dirs.data.join("journal.sqlite3").display().to_string(),
    );
    row("runtime dir", ctx.dirs.runtime.display().to_string());
    row("daemon socket", ctx.endpoint().to_string());
    row("daemon log", ctx.log_path().display().to_string());
    print!("{}", table.render());
    Ok(())
}

/// Package version string.
#[must_use]
pub const fn version() -> &'static str {
    VERSION
}
