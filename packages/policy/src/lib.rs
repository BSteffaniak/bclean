#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Disk-pressure triggers and auto-clean planning for bclean.
//!
//! [`PolicyEngine::evaluate`] checks every enabled policy's trigger against current volume
//! stats and history sources and returns [`Firing`]s (honouring hysteresis, cooldowns, and
//! environmental conditions). [`PolicyEngine::plan`] turns a firing plus classified candidates
//! into a [`bclean_actions::Plan`] (shadow or enforced) and an [`bclean_model::AlertEvent`].

mod conditions;
mod engine;
mod history;
mod planner;

pub use conditions::{FakeProbe, PlatformProbe, SystemProbe, parse_hid_idle, parse_pmset};
pub use engine::{
    Firing, PolicyEngine, PolicyError, TriggerKind, containing_volume, human, match_volumes,
};
pub use history::{
    GrowthHistory, MemoryHistory, NoHistory, SizeSample, VolumeHistory, VolumeSample, linear_fit,
};
pub use planner::{PolicyPlan, firing_event, result_event, select};

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use bclean_actions::{Candidate, EntryKind};
    use bclean_model::{
        ActionSpec, BudgetSpec, ByteSize, Classification, ConditionSpec, Confidence, DurationSpec,
        EphemeralKind, EphemeralSpec, PolicyMode, PolicySpec, RegenCost, SelectOrder, SelectSpec,
        Severity, Staleness, TargetSpec, Threshold, TriggerSpec, VolumeStats,
    };

    use super::*;

    const GIB: u64 = 1 << 30;

    fn volume(mount: &str, total: u64, available: u64, at: i64) -> VolumeStats {
        VolumeStats {
            mount_point: mount.to_owned(),
            fs_type: "apfs".to_owned(),
            total,
            free: available,
            available,
            purgeable: None,
            at,
        }
    }

    fn policy(trigger: TriggerSpec) -> PolicySpec {
        PolicySpec {
            enabled: true,
            description: None,
            trigger,
            target: None,
            select: SelectSpec::default(),
            mode: PolicyMode::Enforce,
            notify: vec!["desktop".to_owned()],
            severity: Severity::Warning,
            budget: BudgetSpec::default(),
            conditions: ConditionSpec::default(),
            cooldown: DurationSpec(Duration::from_secs(3_600)),
        }
    }

    fn engine(policies: Vec<(&str, PolicySpec)>) -> PolicyEngine {
        PolicyEngine::new(
            policies
                .into_iter()
                .map(|(id, p)| (id.to_owned(), p))
                .collect::<BTreeMap<_, _>>(),
        )
        .unwrap()
        .with_probe(FakeProbe {
            ac_power: Some(true),
            idle: Some(Duration::from_secs(3_600)),
        })
        .with_utc_schedules(true)
    }

    fn free_below(volume: &str, pct: f64) -> PolicySpec {
        policy(TriggerSpec::FreeBelow {
            volume: volume.to_owned(),
            threshold: Threshold::Percent(pct),
        })
    }

    #[test]
    fn free_below_with_hysteresis_and_cooldown() {
        let mut e = engine(vec![("low", free_below("/", 10.0))]);
        let total = 100 * GIB;
        let eval = |e: &mut PolicyEngine, now, avail| {
            e.evaluate(
                now,
                &[volume("/", total, avail, now)],
                &NoHistory,
                &NoHistory,
            )
        };
        assert_eq!(eval(&mut e, 0, 20 * GIB).len(), 0);
        let fired = eval(&mut e, 10, 9 * GIB);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].trigger, TriggerKind::FreeBelow);
        assert_eq!(fired[0].scope, "/");
        // Still low, but within cooldown.
        assert_eq!(eval(&mut e, 20, 9 * GIB).len(), 0);
        // Hovering just above the threshold does not release (margin = 0.5 GiB here)...
        assert_eq!(eval(&mut e, 30, 10 * GIB + GIB / 4).len(), 0);
        // ...so after the cooldown it fires again while still active.
        assert_eq!(eval(&mut e, 3_700, 10 * GIB + GIB / 4).len(), 1);
        // Clearly above threshold + margin: released.
        assert_eq!(eval(&mut e, 3_710, 12 * GIB).len(), 0);
        // Re-entering after release, cooldown since last firing (3700) still applies.
        assert_eq!(eval(&mut e, 3_720, 9 * GIB).len(), 0);
        assert_eq!(eval(&mut e, 7_400, 9 * GIB).len(), 1);
    }

    #[test]
    fn hysteresis_prevents_flapping_reentry() {
        let mut p = free_below("/", 10.0);
        p.cooldown = DurationSpec(Duration::ZERO);
        let mut e = engine(vec![("low", p)]);
        let total = 100 * GIB;
        let mut fires = 0;
        // Oscillate around the 10 GiB threshold inside the hysteresis band.
        for (i, avail) in [9.9, 10.1, 9.9, 10.1, 9.9].iter().enumerate() {
            let now = i64::try_from(i).unwrap() * 10;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )] // test values are small
            let avail = (avail * GIB as f64) as u64;
            fires += e
                .evaluate(
                    now,
                    &[volume("/", total, avail, now)],
                    &NoHistory,
                    &NoHistory,
                )
                .len();
        }
        // Zero cooldown: it fires on every evaluation while active, and never "releases"
        // inside the band (each evaluation is a continuation, not a new crossing).
        assert_eq!(fires, 5);
        let mut p = free_below("/", 10.0);
        p.cooldown = DurationSpec(Duration::from_secs(1_000));
        let mut e = engine(vec![("low", p)]);
        let mut fires = 0;
        for (i, avail) in [9.9, 10.1, 9.9, 10.1, 9.9].iter().enumerate() {
            let now = i64::try_from(i).unwrap() * 10;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )] // test values are small
            let avail = (avail * GIB as f64) as u64;
            fires += e
                .evaluate(
                    now,
                    &[volume("/", total, avail, now)],
                    &NoHistory,
                    &NoHistory,
                )
                .len();
        }
        assert_eq!(fires, 1);
    }

    #[test]
    fn used_above_and_volume_matching() {
        let mut e = engine(vec![(
            "full",
            policy(TriggerSpec::UsedAbove {
                volume: "/data/projects/x".to_owned(),
                threshold: Threshold::Percent(90.0),
            }),
        )]);
        let vols = [
            volume("/", 100 * GIB, 5 * GIB, 0),
            volume("/data", 100 * GIB, 5 * GIB, 0),
        ];
        let fired = e.evaluate(0, &vols, &NoHistory, &NoHistory);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].scope, "/data");

        let mut all = engine(vec![("all", free_below("*", 10.0))]);
        assert_eq!(all.evaluate(0, &vols, &NoHistory, &NoHistory).len(), 2);
        assert_eq!(match_volumes("/nope", &[]).len(), 0);
    }

    #[test]
    fn disabled_policies_never_fire() {
        let mut p = free_below("/", 10.0);
        p.enabled = false;
        let mut e = engine(vec![("low", p)]);
        assert_eq!(
            e.evaluate(0, &[volume("/", 100, 1, 0)], &NoHistory, &NoHistory)
                .len(),
            0
        );
    }

    #[test]
    fn time_to_full_linear_fit() {
        let mut e = engine(vec![(
            "ttf",
            policy(TriggerSpec::TimeToFull {
                volume: "/".to_owned(),
                within: DurationSpec(Duration::from_hours(24)),
            }),
        )]);
        let total = 100 * GIB;
        let mut history = MemoryHistory::default();
        // Growing 1 GiB per hour.
        for h in 0..10_i64 {
            history.record_volume("/", h * 3_600, 50 * GIB + u64::try_from(h).unwrap() * GIB);
        }
        let now = 10 * 3_600;
        // 40 GiB left at 1 GiB/h = 40h > 24h: no firing.
        let v = volume("/", total, 40 * GIB, now);
        assert_eq!(e.evaluate(now, &[v], &history, &NoHistory).len(), 0);
        // 10 GiB left = 10h < 24h: fires.
        let mut steep = MemoryHistory::default();
        for h in 0..10_i64 {
            steep.record_volume("/", h * 3_600, 80 * GIB + u64::try_from(h).unwrap() * GIB);
        }
        let v = volume("/", total, 10 * GIB, now);
        let fired = e.evaluate(now, std::slice::from_ref(&v), &steep, &NoHistory);
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert!(fired[0].message.contains("predicted to fill"));
        // Shrinking usage never fires.
        let mut shrinking = MemoryHistory::default();
        for h in 0..10_i64 {
            shrinking.record_volume("/", h * 3_600, 95 * GIB - u64::try_from(h).unwrap() * GIB);
        }
        let mut e2 = engine(vec![(
            "ttf",
            policy(TriggerSpec::TimeToFull {
                volume: "/".to_owned(),
                within: DurationSpec(Duration::from_hours(24)),
            }),
        )]);
        assert_eq!(e2.evaluate(now, &[v], &shrinking, &NoHistory).len(), 0);
    }

    #[test]
    fn growth_trigger() {
        let dir = PathBuf::from("/data/logs");
        let mut e = engine(vec![(
            "growth",
            policy(TriggerSpec::Growth {
                path: dir.to_string_lossy().into_owned(),
                by: ByteSize(10 * GIB),
                within: DurationSpec(Duration::from_secs(3_600)),
            }),
        )]);
        let mut history = MemoryHistory::default();
        history.record_dir(&dir, 0, 100 * GIB); // outside window at now=5000
        history.record_dir(&dir, 2_000, 5 * GIB);
        history.record_dir(&dir, 4_000, 12 * GIB);
        assert_eq!(e.evaluate(5_000, &[], &NoHistory, &history).len(), 0);
        history.record_dir(&dir, 4_900, 16 * GIB);
        let fired = e.evaluate(5_000, &[], &NoHistory, &history);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].path.as_deref(), Some(dir.as_path()));
    }

    #[test]
    fn schedule_trigger() {
        let mut p = policy(TriggerSpec::Schedule {
            cron: "0 3 * * *".to_owned(),
        });
        p.cooldown = DurationSpec(Duration::ZERO);
        let mut e = engine(vec![("nightly", p)]);
        let day = 86_400;
        // First evaluation only initializes.
        assert_eq!(
            e.evaluate(day + 2 * 3_600, &[], &NoHistory, &NoHistory)
                .len(),
            0
        );
        assert_eq!(
            e.evaluate(day + 2 * 3_600 + 1_800, &[], &NoHistory, &NoHistory)
                .len(),
            0
        );
        // Crossing 03:00 UTC fires once.
        assert_eq!(
            e.evaluate(day + 3 * 3_600 + 10, &[], &NoHistory, &NoHistory)
                .len(),
            1
        );
        assert_eq!(
            e.evaluate(day + 3 * 3_600 + 60, &[], &NoHistory, &NoHistory)
                .len(),
            0
        );
        assert!(
            PolicyEngine::new(BTreeMap::from([(
                "bad".to_owned(),
                policy(TriggerSpec::Schedule {
                    cron: "not cron".to_owned()
                })
            )]))
            .is_err()
        );
    }

    #[test]
    fn conditions_block_without_consuming_cooldown() {
        let mut p = free_below("/", 10.0);
        p.conditions = ConditionSpec {
            ac_power_only: true,
            idle_for: Some(DurationSpec(Duration::from_secs(600))),
        };
        let policies = BTreeMap::from([("low".to_owned(), p)]);
        let mut e = PolicyEngine::new(policies.clone())
            .unwrap()
            .with_probe(FakeProbe {
                ac_power: Some(false),
                idle: Some(Duration::from_secs(3_600)),
            });
        let v = [volume("/", 100 * GIB, GIB, 0)];
        let fired = e.evaluate(0, &v, &NoHistory, &NoHistory);
        assert_eq!(fired.len(), 1);
        assert!(fired[0].blocked.as_deref().unwrap().contains("battery"));
        let planned = e.plan(&fired[0], &[], None);
        assert!(planned.plan.is_none());
        // Blocked firings repeat (no cooldown consumed).
        assert_eq!(e.evaluate(10, &v, &NoHistory, &NoHistory).len(), 1);

        let mut busy_user = PolicyEngine::new(policies).unwrap().with_probe(FakeProbe {
            ac_power: Some(true),
            idle: Some(Duration::from_secs(5)),
        });
        let fired = busy_user.evaluate(0, &v, &NoHistory, &NoHistory);
        assert!(fired[0].blocked.as_deref().unwrap().contains("idle"));
    }

    fn candidate(path: &str, size: u64, score: f64, idle: Option<u64>) -> Candidate {
        Candidate::new(
            path,
            Some(EntryKind::Dir),
            Classification {
                rule_id: Some("cargo_target".to_owned()),
                category: Some("rust".to_owned()),
                ephemeral: Some(EphemeralSpec {
                    kind: EphemeralKind::BuildArtifact,
                    regen_cost: RegenCost::Cheap,
                }),
                confidence: Confidence::High,
                staleness: Staleness {
                    stale: idle.is_some_and(|i| i > 100),
                    idle_secs: idle,
                    ..Staleness::default()
                },
                size,
                score,
                action: ActionSpec::Delete,
                auto: true,
                reasons: vec![],
            },
        )
    }

    fn paths(selected: &[&Candidate]) -> Vec<String> {
        selected
            .iter()
            .map(|c| c.path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn selection_ordering_and_filters() {
        let cands = vec![
            candidate("/p/a", 5 * GIB, 1.0, Some(50)),
            candidate("/p/b", 2 * GIB, 9.0, Some(500)),
            candidate("/p/c", 8 * GIB, 3.0, None),
            candidate("/q/d", 3 * GIB, 5.0, Some(1_000)),
        ];
        let budget = BudgetSpec::default();
        let mut spec = SelectSpec::default();
        assert_eq!(
            paths(&select(&spec, &budget, &cands, None, None)),
            ["/p/b", "/q/d", "/p/c", "/p/a"]
        );
        spec.order = SelectOrder::Size;
        assert_eq!(
            paths(&select(&spec, &budget, &cands, None, None)),
            ["/p/c", "/p/a", "/q/d", "/p/b"]
        );
        spec.order = SelectOrder::Oldest;
        assert_eq!(
            paths(&select(&spec, &budget, &cands, None, None)),
            ["/q/d", "/p/b", "/p/a", "/p/c"]
        );
        spec.stale = Some(true);
        spec.roots = vec!["/p".to_owned()];
        assert_eq!(paths(&select(&spec, &budget, &cands, None, None)), ["/p/b"]);
        spec.stale = None;
        spec.roots.clear();
        spec.min_size = ByteSize(4 * GIB);
        assert_eq!(
            paths(&select(&spec, &budget, &cands, None, None)),
            ["/p/a", "/p/c"]
        );
        spec.min_size = ByteSize(0);
        spec.categories = vec!["node".to_owned()];
        assert_eq!(select(&spec, &budget, &cands, None, None).len(), 0);
        spec.categories.clear();
        spec.rules = vec!["cargo_target".to_owned()];
        assert_eq!(select(&spec, &budget, &cands, None, None).len(), 4);

        let mut not_auto = cands;
        not_auto[0].classification.auto = false;
        not_auto[1].classification.ephemeral = None;
        not_auto[2].classification.action = ActionSpec::Report;
        let spec = SelectSpec {
            min_size: ByteSize(0),
            ..SelectSpec::default()
        };
        assert_eq!(
            paths(&select(&spec, &budget, &not_auto, None, None)),
            ["/q/d"]
        );
    }

    #[test]
    fn target_and_budget_stop_selection() {
        let cands = vec![
            candidate("/p/a", 5 * GIB, 4.0, None),
            candidate("/p/b", 4 * GIB, 3.0, None),
            candidate("/p/c", 3 * GIB, 2.0, None),
            candidate("/p/d", GIB, 1.0, None),
        ];
        let spec = SelectSpec::default();
        // Need 8 GiB: a (5) + b (4) = 9 >= 8, stop.
        assert_eq!(
            paths(&select(
                &spec,
                &BudgetSpec::default(),
                &cands,
                None,
                Some(8 * GIB)
            )),
            ["/p/a", "/p/b"]
        );
        assert_eq!(
            select(&spec, &BudgetSpec::default(), &cands, None, Some(0)).len(),
            0
        );
        // Byte budget of 7 GiB: a (5), skip b (would be 9), skip c (8), d (6).
        let budget = BudgetSpec {
            max_bytes: Some(ByteSize(7 * GIB)),
            max_items: None,
        };
        assert_eq!(
            paths(&select(&spec, &budget, &cands, None, None)),
            ["/p/a", "/p/d"]
        );
        let budget = BudgetSpec {
            max_bytes: None,
            max_items: Some(3),
        };
        assert_eq!(select(&spec, &budget, &cands, None, None).len(), 3);
    }

    #[test]
    fn plan_modes_and_target() {
        let total = 100 * GIB;
        let mut enforce = free_below("*", 10.0);
        enforce.target = Some(TargetSpec {
            free_above: Threshold::Percent(15.0),
        });
        let mut shadow = enforce.clone();
        shadow.mode = PolicyMode::Shadow;
        let mut notify = enforce.clone();
        notify.mode = PolicyMode::NotifyOnly;
        let mut e = engine(vec![
            ("enforce", enforce),
            ("notify", notify),
            ("shadow", shadow),
        ]);
        let vols = [
            volume("/", total, 8 * GIB, 0),
            volume("/data", total, 50 * GIB, 0),
        ];
        let fired = e.evaluate(0, &vols, &NoHistory, &NoHistory);
        assert_eq!(fired.len(), 3);
        let cands = vec![
            candidate("/p/a", 5 * GIB, 4.0, None),
            candidate("/data/x", 50 * GIB, 100.0, None),
            candidate("/p/b", 4 * GIB, 3.0, None),
            candidate("/p/c", 3 * GIB, 2.0, None),
        ];
        let by_id = |id: &str| fired.iter().find(|f| f.policy_id == id).unwrap();

        // Need 15 - 8 = 7 GiB from "/": a (5) + b (4). /data/x is on another volume.
        let planned = e.plan(by_id("enforce"), &cands, None);
        let plan = planned.plan.unwrap();
        assert!(!plan.shadow);
        assert_eq!(plan.policy_id.as_deref(), Some("enforce"));
        let planned_paths: Vec<_> = plan.items.iter().map(|i| i.path.clone()).collect();
        assert_eq!(
            planned_paths,
            [PathBuf::from("/p/a"), PathBuf::from("/p/b")]
        );
        assert_eq!(planned.event.key, "enforce:/");
        assert!(planned.event.body.contains("cleaning 2 item(s)"));
        assert_eq!(planned.event.title, "Low disk space on /");

        let planned = e.plan(by_id("shadow"), &cands, None);
        assert!(planned.plan.unwrap().shadow);
        assert!(planned.event.body.contains("would clean"));

        let planned = e.plan(by_id("notify"), &cands, None);
        assert!(planned.plan.is_none());
        assert_eq!(planned.event.severity, Severity::Warning);

        // Target already met on a roomier volume reading.
        let roomy = volume("/", total, 20 * GIB, 0);
        let planned = e.plan(by_id("enforce"), &cands, Some(&roomy));
        assert_eq!(planned.plan.unwrap().len(), 0);
        assert!(planned.event.body.contains("target already met"));
        // Serializable for IPC.
        serde_json::to_string(&fired[0]).unwrap();
    }

    #[test]
    fn result_events_summarize_reports() {
        let firing = Firing {
            policy_id: "low".to_owned(),
            trigger: TriggerKind::FreeBelow,
            scope: "/".to_owned(),
            volume: None,
            path: None,
            at: 5,
            mode: PolicyMode::Enforce,
            severity: Severity::Critical,
            notify: vec![],
            message: "m".to_owned(),
            blocked: None,
        };
        let report = bclean_actions::ExecutionReport {
            dry_run: false,
            estimated_freed: 3 * GIB,
            measured_freed: Some(i64::try_from(2 * GIB).unwrap()),
            ..bclean_actions::ExecutionReport::default()
        };
        let event = result_event(&firing, &report);
        assert_eq!(event.key, "low:/:result");
        assert!(event.body.contains("3.0 GiB estimated"));
        assert!(event.body.contains("2.0 GiB measured"));
        assert_eq!(event.severity, Severity::Info);
    }
}
