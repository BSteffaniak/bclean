//! Trigger evaluation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use bclean_actions::protect::expand_tilde;
use bclean_model::{PolicyMode, PolicySpec, Severity, TriggerSpec, VolumeStats};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::conditions::{PlatformProbe, SystemProbe};
use crate::history::{GrowthHistory, VolumeHistory, linear_fit};

/// Shortest lookback used to fit `TIME_TO_FULL` trends.
const MIN_TTF_LOOKBACK: i64 = 3_600;
/// Longest lookback used to fit `TIME_TO_FULL` trends.
const MAX_TTF_LOOKBACK: i64 = 7 * 86_400;
/// Minimum number of samples (including the current reading) for a `TIME_TO_FULL` fit.
const MIN_TTF_SAMPLES: usize = 3;

/// Error building a [`PolicyEngine`].
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// A `SCHEDULE` trigger has an invalid cron expression.
    #[error("policy `{policy}`: invalid cron expression `{cron}`: {reason}")]
    BadCron {
        /// Policy id.
        policy: String,
        /// The expression.
        cron: String,
        /// Parser message.
        reason: String,
    },
}

/// Trigger kind of a firing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TriggerKind {
    /// `FREE_BELOW`.
    FreeBelow,
    /// `USED_ABOVE`.
    UsedAbove,
    /// `TIME_TO_FULL`.
    TimeToFull,
    /// `GROWTH`.
    Growth,
    /// `SCHEDULE`.
    Schedule,
}

/// A policy whose trigger fired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Firing {
    /// Policy id.
    pub policy_id: String,
    /// Trigger kind.
    pub trigger: TriggerKind,
    /// What fired: a mount point, a directory path, or empty for schedules.
    pub scope: String,
    /// Related volume (volume triggers).
    pub volume: Option<VolumeStats>,
    /// Related directory (growth triggers).
    pub path: Option<PathBuf>,
    /// Evaluation time (Unix seconds).
    pub at: i64,
    /// Policy mode.
    pub mode: PolicyMode,
    /// Policy severity.
    pub severity: Severity,
    /// Alert sink ids to notify.
    pub notify: Vec<String>,
    /// Human-readable description of why it fired.
    pub message: String,
    /// Set when the policy's conditions (AC power, idle) are not met: the firing should only be
    /// reported, never acted on. Blocked firings do not start the cooldown.
    pub blocked: Option<String>,
}

impl Firing {
    /// Stable alert dedupe key (`policy:scope`).
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}:{}", self.policy_id, self.scope)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct TriggerState {
    active: bool,
    last_fired: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub spec: PolicySpec,
    schedule: Option<cron::Schedule>,
}

/// Result of checking one trigger scope.
struct Check {
    scope: String,
    volume: Option<VolumeStats>,
    path: Option<PathBuf>,
    /// Condition to become active.
    enter: bool,
    /// Condition to become inactive (hysteresis release).
    release: bool,
    message: String,
}

/// Evaluates policy triggers and plans auto-cleans.
pub struct PolicyEngine {
    pub(crate) policies: BTreeMap<String, Compiled>,
    state: BTreeMap<(String, String), TriggerState>,
    schedule_cursor: BTreeMap<String, i64>,
    hysteresis: f64,
    utc: bool,
    pub(crate) volumes: Vec<VolumeStats>,
    probe: Box<dyn SystemProbe + Send + Sync>,
}

impl std::fmt::Debug for PolicyEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyEngine")
            .field("policies", &self.policies.keys().collect::<Vec<_>>())
            .field("hysteresis", &self.hysteresis)
            .field("utc", &self.utc)
            .finish_non_exhaustive()
    }
}

/// Normalizes a 5-field (standard) cron expression to the 6-field form the `cron` crate
/// expects by prepending a seconds field.
fn normalize_cron(expr: &str) -> String {
    if expr.split_whitespace().count() == 5 {
        format!("0 {}", expr.trim())
    } else {
        expr.trim().to_owned()
    }
}

impl PolicyEngine {
    /// Builds an engine from configured policies, with the platform condition probe, local-time
    /// schedules, and 5% hysteresis.
    ///
    /// # Errors
    ///
    /// * A `SCHEDULE` trigger's cron expression does not parse.
    pub fn new(policies: BTreeMap<String, PolicySpec>) -> Result<Self, PolicyError> {
        let mut compiled = BTreeMap::new();
        for (id, spec) in policies {
            let schedule = match &spec.trigger {
                TriggerSpec::Schedule { cron } => Some(
                    cron::Schedule::from_str(&normalize_cron(cron)).map_err(|e| {
                        PolicyError::BadCron {
                            policy: id.clone(),
                            cron: cron.clone(),
                            reason: e.to_string(),
                        }
                    })?,
                ),
                _ => None,
            };
            compiled.insert(id, Compiled { spec, schedule });
        }
        Ok(Self {
            policies: compiled,
            state: BTreeMap::new(),
            schedule_cursor: BTreeMap::new(),
            hysteresis: 0.05,
            utc: false,
            volumes: Vec::new(),
            probe: Box::new(PlatformProbe),
        })
    }

    /// Replaces the condition probe.
    #[must_use]
    pub fn with_probe(mut self, probe: impl SystemProbe + Send + Sync + 'static) -> Self {
        self.probe = Box::new(probe);
        self
    }

    /// Sets the hysteresis fraction (default `0.05`).
    ///
    /// A `FREE_BELOW` trigger with threshold `t` on a volume of `total` bytes, once active,
    /// stays active until free space exceeds `t + max(t * h, total * h / 10)` (and
    /// symmetrically for `USED_ABOVE`); `TIME_TO_FULL` releases once the prediction exceeds
    /// `within * (1 + h)`, and `GROWTH` once growth drops to `by * (1 - h)`.
    #[must_use]
    pub const fn with_hysteresis(mut self, fraction: f64) -> Self {
        self.hysteresis = fraction;
        self
    }

    /// Evaluates cron schedules in UTC instead of local time.
    #[must_use]
    pub const fn with_utc_schedules(mut self, utc: bool) -> Self {
        self.utc = utc;
        self
    }

    /// Policy specs by id.
    pub fn policies(&self) -> impl Iterator<Item = (&String, &PolicySpec)> {
        self.policies.iter().map(|(id, c)| (id, &c.spec))
    }

    /// Evaluates every enabled policy at `now` (Unix seconds).
    ///
    /// Volume triggers fire while active (entered past the threshold and not yet released by
    /// hysteresis), at most once per cooldown per (policy, volume). `SCHEDULE` triggers fire
    /// when a scheduled time falls in `(previous evaluation, now]`; the first evaluation only
    /// initializes the cursor.
    pub fn evaluate(
        &mut self,
        now: i64,
        volumes: &[VolumeStats],
        history: &dyn VolumeHistory,
        growth: &dyn GrowthHistory,
    ) -> Vec<Firing> {
        self.volumes = volumes.to_vec();
        let ids: Vec<String> = self.policies.keys().cloned().collect();
        let mut firings = Vec::new();
        for id in ids {
            let compiled = &self.policies[&id];
            if !compiled.spec.enabled {
                continue;
            }
            let spec = compiled.spec.clone();
            let (kind, checks) = self.checks(&id, &spec, now, volumes, history, growth);
            let blocked = self.blocked(&spec);
            let cooldown = i64::try_from(spec.cooldown.as_secs()).unwrap_or(i64::MAX);
            for check in checks {
                let state = self
                    .state
                    .entry((id.clone(), check.scope.clone()))
                    .or_default();
                state.active = if state.active && kind != TriggerKind::Schedule {
                    !check.release
                } else {
                    check.enter
                };
                if !state.active {
                    continue;
                }
                if state
                    .last_fired
                    .is_some_and(|last| now.saturating_sub(last) < cooldown)
                {
                    continue;
                }
                if blocked.is_none() {
                    state.last_fired = Some(now);
                }
                firings.push(Firing {
                    policy_id: id.clone(),
                    trigger: kind,
                    scope: check.scope,
                    volume: check.volume,
                    path: check.path,
                    at: now,
                    mode: spec.mode,
                    severity: spec.severity,
                    notify: spec.notify.clone(),
                    message: check.message,
                    blocked: blocked.clone(),
                });
            }
        }
        firings
    }

    fn checks(
        &mut self,
        id: &str,
        spec: &PolicySpec,
        now: i64,
        volumes: &[VolumeStats],
        history: &dyn VolumeHistory,
        growth: &dyn GrowthHistory,
    ) -> (TriggerKind, Vec<Check>) {
        match &spec.trigger {
            TriggerSpec::FreeBelow { volume, threshold } => (
                TriggerKind::FreeBelow,
                match_volumes(volume, volumes)
                    .into_iter()
                    .map(|v| {
                        let t = threshold.resolve(v.total);
                        let margin = self.margin(t, v.total);
                        Check {
                            scope: v.mount_point.clone(),
                            volume: Some(v.clone()),
                            path: None,
                            enter: v.available < t,
                            release: v.available >= t.saturating_add(margin),
                            message: format!(
                                "free space on {} is {} (threshold {})",
                                v.mount_point,
                                human(v.available),
                                human(t)
                            ),
                        }
                    })
                    .collect(),
            ),
            TriggerSpec::UsedAbove { volume, threshold } => (
                TriggerKind::UsedAbove,
                match_volumes(volume, volumes)
                    .into_iter()
                    .map(|v| {
                        let t = threshold.resolve(v.total);
                        let margin = self.margin(t, v.total);
                        Check {
                            scope: v.mount_point.clone(),
                            volume: Some(v.clone()),
                            path: None,
                            enter: v.used() > t,
                            release: v.used().saturating_add(margin) <= t,
                            message: format!(
                                "used space on {} is {} (threshold {})",
                                v.mount_point,
                                human(v.used()),
                                human(t)
                            ),
                        }
                    })
                    .collect(),
            ),
            TriggerSpec::TimeToFull { volume, within } => (
                TriggerKind::TimeToFull,
                match_volumes(volume, volumes)
                    .into_iter()
                    .map(|v| self.check_ttf(now, v, within.duration(), history))
                    .collect(),
            ),
            TriggerSpec::Growth { path, by, within } => (
                TriggerKind::Growth,
                vec![self.check_growth(now, path, by.bytes(), within.duration(), growth)],
            ),
            TriggerSpec::Schedule { .. } => {
                let due = self.schedule_due(id, now);
                (
                    TriggerKind::Schedule,
                    vec![Check {
                        scope: String::new(),
                        volume: None,
                        path: None,
                        enter: due,
                        release: !due,
                        message: "scheduled run".to_owned(),
                    }],
                )
            }
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )] // byte thresholds: f64 precision is ample and results are clamped non-negative
    fn margin(&self, threshold: u64, total: u64) -> u64 {
        let h = self.hysteresis.max(0.0);
        let a = threshold as f64 * h;
        let b = total as f64 * h / 10.0;
        a.max(b) as u64
    }

    #[allow(clippy::cast_precision_loss)] // seconds and bytes fit comfortably in f64
    fn check_ttf(
        &self,
        now: i64,
        volume: &VolumeStats,
        within: Duration,
        history: &dyn VolumeHistory,
    ) -> Check {
        let within_secs = i64::try_from(within.as_secs()).unwrap_or(i64::MAX);
        let lookback = within_secs.clamp(MIN_TTF_LOOKBACK, MAX_TTF_LOOKBACK);
        let mut points: Vec<(i64, u64)> = history
            .volume_samples(&volume.mount_point, now.saturating_sub(lookback))
            .into_iter()
            .filter(|s| s.at <= now)
            .map(|s| (s.at, s.used))
            .collect();
        if !points.iter().any(|(at, _)| *at == volume.at) {
            points.push((volume.at, volume.used()));
        }
        let fit = (points.len() >= MIN_TTF_SAMPLES)
            .then(|| linear_fit(&points))
            .flatten();
        let mut check = Check {
            scope: volume.mount_point.clone(),
            volume: Some(volume.clone()),
            path: None,
            enter: false,
            release: true,
            message: format!("{} is not filling up", volume.mount_point),
        };
        if let Some((_, slope)) = fit
            && slope > 0.0
        {
            let seconds_left = volume.available as f64 / slope;
            check.enter = seconds_left <= within_secs as f64;
            check.release = seconds_left > within_secs as f64 * (1.0 + self.hysteresis.max(0.0));
            check.message = format!(
                "{} is predicted to fill in {} (growing {}/day)",
                volume.mount_point,
                human_secs(seconds_left),
                human_f(slope * 86_400.0)
            );
        }
        check
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )] // byte thresholds: f64 precision is ample and results are clamped non-negative
    fn check_growth(
        &self,
        now: i64,
        path: &str,
        by: u64,
        within: Duration,
        growth: &dyn GrowthHistory,
    ) -> Check {
        let expanded = expand_tilde(path);
        let since = now.saturating_sub(i64::try_from(within.as_secs()).unwrap_or(i64::MAX));
        let mut samples = growth.size_samples(&expanded, since);
        samples.retain(|s| s.at <= now);
        samples.sort_by_key(|s| s.at);
        let grown = match (samples.last(), samples.iter().map(|s| s.bytes).min()) {
            (Some(last), Some(min)) => last.bytes.saturating_sub(min),
            _ => 0,
        };
        let release_at = (by as f64 * (1.0 - self.hysteresis.clamp(0.0, 1.0))) as u64;
        Check {
            scope: expanded.to_string_lossy().into_owned(),
            volume: None,
            path: Some(expanded.clone()),
            enter: grown > by,
            release: grown <= release_at,
            message: format!(
                "{} grew by {} within {}",
                expanded.display(),
                human(grown),
                human_secs(within.as_secs_f64())
            ),
        }
    }

    fn schedule_due(&mut self, id: &str, now: i64) -> bool {
        let Some(schedule) = self.policies.get(id).and_then(|c| c.schedule.clone()) else {
            return false;
        };
        let Some(previous) = self.schedule_cursor.insert(id.to_owned(), now) else {
            return false;
        };
        if now <= previous {
            return false;
        }
        let next = if self.utc {
            next_after(&schedule, &Utc, previous)
        } else {
            next_after(&schedule, &Local, previous)
        };
        next.is_some_and(|t| t <= now)
    }

    fn blocked(&self, spec: &PolicySpec) -> Option<String> {
        if spec.conditions.ac_power_only && self.probe.on_ac_power() == Some(false) {
            return Some("running on battery power".to_owned());
        }
        if let Some(required) = spec.conditions.idle_for {
            match self.probe.idle_time() {
                Some(idle) if idle < required.duration() => {
                    return Some(format!(
                        "user idle for {} (need {})",
                        human_secs(idle.as_secs_f64()),
                        required
                    ));
                }
                Some(_) => {}
                None => log::debug!("idle time unknown on this platform; treating as idle"),
            }
        }
        None
    }
}

fn next_after<Z: TimeZone>(schedule: &cron::Schedule, tz: &Z, after: i64) -> Option<i64> {
    let start: DateTime<Z> = tz.timestamp_opt(after, 0).single()?;
    schedule.after(&start).next().map(|t| t.timestamp())
}

/// Volumes matching a trigger's `volume` field: `"*"` = all; otherwise the volume containing
/// that path (longest mount point prefix).
#[must_use]
pub fn match_volumes<'a>(selector: &str, volumes: &'a [VolumeStats]) -> Vec<&'a VolumeStats> {
    if selector.trim() == "*" {
        return volumes.iter().collect();
    }
    if let Some(exact) = volumes.iter().find(|v| v.mount_point == selector) {
        return vec![exact];
    }
    containing_volume(&expand_tilde(selector), volumes)
        .into_iter()
        .collect()
}

/// The volume whose mount point is the longest prefix of `path`.
///
/// On macOS the root filesystem is a read-only system volume and user data lives on the
/// firmlinked `/System/Volumes/Data` volume (`/Users`, `/private`, ... are reached through
/// firmlinks), so a path that resolves to `/` (or to no listed volume) maps to the data volume
/// when it is listed.
#[must_use]
pub fn containing_volume<'a>(path: &Path, volumes: &'a [VolumeStats]) -> Option<&'a VolumeStats> {
    let best = volumes
        .iter()
        .filter(|v| path.starts_with(&v.mount_point))
        .max_by_key(|v| Path::new(&v.mount_point).components().count());
    if cfg!(target_os = "macos")
        && best.is_none_or(|v| v.mount_point == "/")
        && let Some(data) = volumes.iter().find(|v| v.mount_point == MACOS_DATA_VOLUME)
    {
        return Some(data);
    }
    best
}

/// macOS APFS data volume mount point.
const MACOS_DATA_VOLUME: &str = "/System/Volumes/Data";

/// Formats bytes for messages.
#[must_use]
pub fn human(bytes: u64) -> String {
    bclean_model::ByteSize(bytes).to_string()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // display only
fn human_f(bytes: f64) -> String {
    human(bytes.max(0.0) as u64)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // display only
fn human_secs(secs: f64) -> String {
    let secs = secs.max(0.0);
    if secs >= 86_400.0 {
        format!("{:.1}d", secs / 86_400.0)
    } else if secs >= 3_600.0 {
        format!("{:.1}h", secs / 3_600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{}s", secs as u64)
    }
}
