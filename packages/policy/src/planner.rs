//! Turning firings into cleanup plans and alert events.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::path::PathBuf;

use bclean_actions::protect::expand_tilde;
use bclean_actions::{Candidate, ExecutionReport, ItemOutcome, Plan};
use bclean_model::{
    ActionSpec, AlertEvent, BudgetSpec, PolicyMode, SelectOrder, SelectSpec, VolumeStats,
};
use serde::{Deserialize, Serialize};

use crate::engine::{Firing, PolicyEngine, TriggerKind, containing_volume, human};

/// Result of planning a firing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyPlan {
    /// The plan (`None` for `NOTIFY_ONLY` policies or blocked firings). Shadow plans have
    /// `shadow = true` and are executed as dry runs.
    pub plan: Option<Plan>,
    /// Alert describing the firing and the plan.
    pub event: AlertEvent,
}

/// Selects candidates according to `select`, ordered, and bounded by `needed` bytes (stop once
/// reached) and `budget` (hard caps).
///
/// * `volume`: when given (with all known `volumes`), only candidates on that volume are
///   considered.
/// * `REPORT` actions are never selected.
/// * Items larger than the remaining `max_bytes` budget are skipped (smaller ones may still
///   fit); selection stops at `max_items` or once `needed` is met.
#[must_use]
pub fn select<'a>(
    select: &SelectSpec,
    budget: &BudgetSpec,
    candidates: &'a [Candidate],
    volume: Option<(&VolumeStats, &[VolumeStats])>,
    needed: Option<u64>,
) -> Vec<&'a Candidate> {
    let roots: Vec<PathBuf> = select.roots.iter().map(|r| expand_tilde(r)).collect();
    let mut eligible: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| {
            let class = &c.classification;
            if matches!(class.action, ActionSpec::Report) {
                return false;
            }
            if select
                .ephemeral
                .is_some_and(|e| e != class.ephemeral.is_some())
            {
                return false;
            }
            if select.stale.is_some_and(|s| s != class.staleness.stale) {
                return false;
            }
            if select.auto_only && !class.auto {
                return false;
            }
            if !select.rules.is_empty()
                && !class
                    .rule_id
                    .as_ref()
                    .is_some_and(|r| select.rules.contains(r))
            {
                return false;
            }
            if !select.categories.is_empty()
                && !class
                    .category
                    .as_ref()
                    .is_some_and(|cat| select.categories.contains(cat))
            {
                return false;
            }
            if class.size < select.min_size.bytes() {
                return false;
            }
            if !roots.is_empty() && !roots.iter().any(|r| c.path.starts_with(r)) {
                return false;
            }
            if let Some((target, all)) = volume {
                let all = if all.iter().any(|v| v.mount_point == target.mount_point) {
                    all
                } else {
                    std::slice::from_ref(target)
                };
                if containing_volume(&c.path, all).map(|v| &v.mount_point)
                    != Some(&target.mount_point)
                {
                    return false;
                }
            }
            true
        })
        .collect();
    eligible.sort_by(|a, b| compare(select.order, a, b));

    if needed == Some(0) {
        return Vec::new();
    }
    let mut chosen = Vec::new();
    let mut total: u64 = 0;
    for candidate in eligible {
        if budget.max_items.is_some_and(|max| chosen.len() >= max) {
            break;
        }
        let size = candidate.classification.size;
        if let Some(max) = budget.max_bytes
            && total.saturating_add(size) > max.bytes()
        {
            continue;
        }
        total = total.saturating_add(size);
        chosen.push(candidate);
        if needed.is_some_and(|n| total >= n) {
            break;
        }
    }
    chosen
}

fn compare(order: SelectOrder, a: &Candidate, b: &Candidate) -> Ordering {
    let (ca, cb) = (&a.classification, &b.classification);
    let primary = match order {
        SelectOrder::Score => cb.score.total_cmp(&ca.score),
        SelectOrder::Size => cb.size.cmp(&ca.size),
        // Longest idle first; unknown idle sorts last.
        SelectOrder::Oldest => match (ca.staleness.idle_secs, cb.staleness.idle_secs) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    };
    primary
        .then_with(|| cb.size.cmp(&ca.size))
        .then_with(|| a.path.cmp(&b.path))
}

impl PolicyEngine {
    /// Plans the cleanup for a firing.
    ///
    /// * `NOTIFY_ONLY` policies and blocked firings produce no plan, only an alert.
    /// * `SHADOW` produces a plan with `shadow = true`; `ENFORCE` a real plan.
    /// * With a `target`, selection stops once the volume's available space would reach
    ///   `target.free_above` (estimated sizes are counted in full, including `TRASH` items,
    ///   which only free space once the trash is emptied). If the target is already met the
    ///   plan is empty.
    /// * `volume` defaults to the firing's volume; candidates are restricted to it.
    #[must_use]
    pub fn plan(
        &self,
        firing: &Firing,
        candidates: &[Candidate],
        volume: Option<&VolumeStats>,
    ) -> PolicyPlan {
        let Some(compiled) = self.policies.get(&firing.policy_id) else {
            return PolicyPlan {
                plan: None,
                event: firing_event(firing, "policy no longer configured"),
            };
        };
        let spec = &compiled.spec;
        if let Some(why) = &firing.blocked {
            return PolicyPlan {
                plan: None,
                event: firing_event(firing, &format!("not cleaning: {why}")),
            };
        }
        if spec.mode == PolicyMode::NotifyOnly {
            return PolicyPlan {
                plan: None,
                event: firing_event(firing, ""),
            };
        }
        let volume = volume.or(firing.volume.as_ref());
        let needed = match (spec.target, volume) {
            (Some(target), Some(v)) => Some(
                target
                    .free_above
                    .resolve(v.total)
                    .saturating_sub(v.available),
            ),
            _ => None,
        };
        // The engine's latest view of all volumes lets nested mounts resolve correctly.
        let restrict = volume.map(|v| (v, self.volumes.as_slice()));
        let chosen = select(&spec.select, &spec.budget, candidates, restrict, needed);
        let mut plan = Plan::from_candidates(chosen);
        plan.shadow = spec.mode == PolicyMode::Shadow;
        plan.policy_id = Some(firing.policy_id.clone());
        plan.created_at = firing.at;
        let verb = if plan.shadow {
            "would clean"
        } else {
            "cleaning"
        };
        let mut summary = if plan.is_empty() {
            if needed == Some(0) {
                "target already met; nothing to clean".to_owned()
            } else {
                "no eligible candidates".to_owned()
            }
        } else {
            format!(
                "{verb} {} item(s), ~{}",
                plan.len(),
                human(plan.estimated_bytes())
            )
        };
        if let Some(n) = needed
            && n > plan.estimated_bytes()
            && !plan.is_empty()
        {
            let _ = write!(
                summary,
                " (short of target by {})",
                human(n - plan.estimated_bytes())
            );
        }
        PolicyPlan {
            event: firing_event(firing, &summary),
            plan: Some(plan),
        }
    }
}

/// Alert for a firing, with an optional extra line (for example a plan summary).
#[must_use]
pub fn firing_event(firing: &Firing, extra: &str) -> AlertEvent {
    let title = match firing.trigger {
        TriggerKind::FreeBelow => format!("Low disk space on {}", firing.scope),
        TriggerKind::UsedAbove => format!("High disk usage on {}", firing.scope),
        TriggerKind::TimeToFull => format!("{} is filling up", firing.scope),
        TriggerKind::Growth => format!("{} is growing quickly", firing.scope),
        TriggerKind::Schedule => format!("Scheduled policy {}", firing.policy_id),
    };
    let mut body = firing.message.clone();
    if !extra.is_empty() {
        body.push('\n');
        body.push_str(extra);
    }
    AlertEvent {
        key: firing.key(),
        policy_id: Some(firing.policy_id.clone()),
        severity: firing.severity,
        title,
        body,
        volume: firing.volume.clone(),
        at: firing.at,
    }
}

/// Alert summarizing the execution of a policy plan.
#[must_use]
pub fn result_event(firing: &Firing, report: &ExecutionReport) -> AlertEvent {
    let count =
        |pred: fn(&ItemOutcome) -> bool| report.results.iter().filter(|r| pred(&r.outcome)).count();
    let done = count(|o| matches!(o, ItemOutcome::Done | ItemOutcome::DryRun));
    let skipped = count(|o| matches!(o, ItemOutcome::Skipped(_)));
    let failed = count(|o| matches!(o, ItemOutcome::Failed(_)));
    let verb = if report.dry_run {
        "would clean"
    } else {
        "cleaned"
    };
    let mut body = format!(
        "{verb} {done} item(s), ~{} estimated",
        human(report.estimated_freed)
    );
    if let Some(measured) = report.measured_freed {
        let sign = if measured < 0 { "-" } else { "" };
        let _ = write!(body, ", {sign}{} measured", human(measured.unsigned_abs()));
    }
    if skipped > 0 {
        let _ = write!(body, ", {skipped} skipped");
    }
    if failed > 0 {
        let _ = write!(body, ", {failed} failed");
    }
    if report.cancelled {
        body.push_str(" (cancelled)");
    }
    AlertEvent {
        key: format!("{}:result", firing.key()),
        policy_id: Some(firing.policy_id.clone()),
        severity: if failed > 0 {
            firing.severity.max(bclean_model::Severity::Warning)
        } else {
            bclean_model::Severity::Info
        },
        title: format!(
            "Policy {} {}",
            firing.policy_id,
            if report.dry_run { "dry run" } else { "ran" }
        ),
        body,
        volume: firing.volume.clone(),
        at: firing.at,
    }
}
