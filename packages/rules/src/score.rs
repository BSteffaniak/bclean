//! Cleanup scoring.
//!
//! ```text
//! score = size_bytes × staleness_factor × confidence_weight ÷ regen_weight
//! ```
//!
//! * `size_bytes`: reclaimable bytes (allocated by default, apparent if `scan.size_mode`
//!   says so)
//! * `staleness_factor`: `idle / threshold` clamped to `[0.05, 4.0]`, so an item idle for
//!   exactly its threshold scores 1.0, a fresh item still gets a small non-zero weight (it can
//!   be cleaned under pressure), and very old items saturate at 4×. A forcing signal (for
//!   example a merged worktree) raises the factor to at least 2.0. Unknown idle time = 0.5.
//! * `confidence_weight`: [`Confidence::weight`] (`LOW` 0.25, `MEDIUM` 0.6, `HIGH` 1.0)
//! * `regen_weight`: [`RegenCost::weight`](bclean_model::RegenCost::weight) (`FREE` 0.5,
//!   `CHEAP` 1, `NETWORK` 2, `EXPENSIVE` 4), or [`DURABLE_REGEN_WEIGHT`] for durable
//!   (non-regenerable) items so they rank below comparable ephemeral ones
//!
//! The score is in "weighted bytes": higher is a better cleanup candidate.

use bclean_model::{Confidence, EphemeralSpec, Staleness};

/// Divisor used for durable items, which cannot be regenerated.
pub const DURABLE_REGEN_WEIGHT: f64 = 8.0;
/// Lower clamp of the staleness factor.
pub const MIN_STALENESS_FACTOR: f64 = 0.05;
/// Upper clamp of the staleness factor.
pub const MAX_STALENESS_FACTOR: f64 = 4.0;
/// Factor used when no activity information exists.
pub const UNKNOWN_STALENESS_FACTOR: f64 = 0.5;
/// Minimum factor when a signal forces staleness.
pub const FORCED_STALENESS_FACTOR: f64 = 2.0;

/// The staleness factor of the score formula.
#[must_use]
#[allow(clippy::cast_precision_loss)] // ratios of seconds; precision loss is irrelevant
pub fn staleness_factor(staleness: &Staleness) -> f64 {
    let forced = staleness.signals.iter().any(|s| s.forces_stale);
    let base = match staleness.idle_secs {
        Some(idle) if staleness.threshold_secs > 0 => (idle as f64
            / staleness.threshold_secs as f64)
            .clamp(MIN_STALENESS_FACTOR, MAX_STALENESS_FACTOR),
        Some(_) => MAX_STALENESS_FACTOR,
        None if staleness.stale => 1.0,
        None => UNKNOWN_STALENESS_FACTOR,
    };
    if forced {
        base.max(FORCED_STALENESS_FACTOR)
    } else {
        base
    }
}

/// The regeneration-cost divisor of the score formula.
#[must_use]
pub fn regen_weight(ephemeral: Option<EphemeralSpec>) -> f64 {
    ephemeral.map_or(DURABLE_REGEN_WEIGHT, |e| e.regen_cost.weight())
}

/// Computes the cleanup score (see the module docs).
#[must_use]
#[allow(clippy::cast_precision_loss)] // byte counts as f64 weights
pub fn score(
    size: u64,
    staleness: &Staleness,
    confidence: Confidence,
    ephemeral: Option<EphemeralSpec>,
) -> f64 {
    size as f64 * staleness_factor(staleness) * confidence.weight() / regen_weight(ephemeral)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_model::{EphemeralKind, RegenCost, SignalKind, SignalReading};

    fn staleness(idle: Option<u64>, threshold: u64, forced: bool) -> Staleness {
        Staleness {
            stale: forced || idle.is_some_and(|i| i >= threshold),
            last_activity: None,
            idle_secs: idle,
            threshold_secs: threshold,
            signals: vec![SignalReading {
                kind: SignalKind::GitState,
                last_activity: None,
                forces_stale: forced,
                detail: String::new(),
            }],
            project_scoped: false,
        }
    }

    #[test]
    fn formula() {
        let cheap = Some(EphemeralSpec {
            kind: EphemeralKind::BuildArtifact,
            regen_cost: RegenCost::Cheap,
        });
        assert!(
            (score(
                1000,
                &staleness(Some(30), 30, false),
                Confidence::High,
                cheap
            ) - 1000.0)
                .abs()
                < 1e-9
        );
        assert!(
            (score(
                1000,
                &staleness(Some(600), 30, false),
                Confidence::High,
                cheap
            ) - 4000.0)
                .abs()
                < 1e-9
        );
        assert!(
            (score(1000, &staleness(Some(0), 30, false), Confidence::Low, cheap) - 12.5).abs()
                < 1e-9
        );
        assert!(
            (score(1000, &staleness(Some(0), 30, true), Confidence::High, cheap) - 2000.0).abs()
                < 1e-9
        );
        assert!(
            (score(800, &staleness(None, 30, false), Confidence::High, None) - 50.0).abs() < 1e-9
        );
    }
}
