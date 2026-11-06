//! Client-side selection filters and sort orders for classified items (`ls` / `clean`).

use std::cmp::Ordering;

use bclean_model::Classification;
use bclean_protocol::ClassifiedPath;

/// Filters shared by `ls` and `clean`. The default selects everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Only ephemeral (regenerable) items.
    pub ephemeral: bool,
    /// `None`: no staleness filter. `Some(None)`: only items classified stale.
    /// `Some(Some(secs))`: only items idle at least `secs` (or forced stale).
    pub stale: Option<Option<u64>>,
    /// Only these rule ids (empty = any).
    pub rules: Vec<String>,
    /// Only these categories (empty = any).
    pub categories: Vec<String>,
    /// Ignore items smaller than this many bytes.
    pub min_size: u64,
    /// Only rules marked safe for automatic cleaning.
    pub auto_only: bool,
}

impl Selection {
    /// Whether no filter is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether `class` passes every filter.
    #[must_use]
    pub fn matches(&self, class: &Classification) -> bool {
        if self.ephemeral && class.ephemeral.is_none() {
            return false;
        }
        match self.stale {
            None => {}
            Some(None) => {
                if !class.staleness.stale {
                    return false;
                }
            }
            Some(Some(secs)) => {
                let forced = class.staleness.signals.iter().any(|s| s.forces_stale);
                let idle = class.staleness.idle_secs.is_some_and(|idle| idle >= secs);
                if !forced && !idle {
                    return false;
                }
            }
        }
        if !self.rules.is_empty()
            && !class
                .rule_id
                .as_ref()
                .is_some_and(|id| self.rules.contains(id))
        {
            return false;
        }
        if !self.categories.is_empty()
            && !class
                .category
                .as_ref()
                .is_some_and(|c| self.categories.contains(c))
        {
            return false;
        }
        if class.size < self.min_size {
            return false;
        }
        if self.auto_only && !class.auto {
            return false;
        }
        true
    }

    /// Keeps the items that pass every filter.
    #[must_use]
    pub fn apply(&self, items: Vec<ClassifiedPath>) -> Vec<ClassifiedPath> {
        items
            .into_iter()
            .filter(|item| self.matches(&item.classification))
            .collect()
    }
}

/// Listing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// Highest cleanup score first.
    #[default]
    Score,
    /// Largest first.
    Size,
    /// Longest idle first.
    Age,
    /// Path order.
    Path,
}

/// Sorts classified items in place.
pub fn sort_items(items: &mut [ClassifiedPath], order: SortOrder) {
    items.sort_by(|a, b| {
        let (ca, cb) = (&a.classification, &b.classification);
        let primary = match order {
            SortOrder::Score => cb.score.total_cmp(&ca.score),
            SortOrder::Size => cb.size.cmp(&ca.size),
            SortOrder::Age => cb
                .staleness
                .idle_secs
                .unwrap_or(0)
                .cmp(&ca.staleness.idle_secs.unwrap_or(0)),
            SortOrder::Path => Ordering::Equal,
        };
        primary.then_with(|| a.path.cmp(&b.path))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_model::{
        ActionSpec, Confidence, EphemeralKind, EphemeralSpec, RegenCost, Staleness,
    };

    fn class(size: u64, stale: bool, idle: Option<u64>, ephemeral: bool) -> Classification {
        Classification {
            rule_id: Some("r".to_owned()),
            category: Some("rust".to_owned()),
            ephemeral: ephemeral.then_some(EphemeralSpec {
                kind: EphemeralKind::BuildArtifact,
                regen_cost: RegenCost::Cheap,
            }),
            confidence: Confidence::High,
            staleness: Staleness {
                stale,
                idle_secs: idle,
                ..Staleness::default()
            },
            size,
            score: 1.0,
            action: ActionSpec::Trash,
            auto: true,
            reasons: Vec::new(),
        }
    }

    #[test]
    fn filters() {
        let all = Selection::default();
        assert!(all.is_empty());
        assert!(all.matches(&class(1, false, None, false)));
        let eph = Selection {
            ephemeral: true,
            ..Selection::default()
        };
        assert!(!eph.matches(&class(1, false, None, false)));
        let stale = Selection {
            stale: Some(None),
            ..Selection::default()
        };
        assert!(stale.matches(&class(1, true, None, true)));
        assert!(!stale.matches(&class(1, false, None, true)));
        let aged = Selection {
            stale: Some(Some(100)),
            ..Selection::default()
        };
        assert!(aged.matches(&class(1, false, Some(200), true)));
        assert!(!aged.matches(&class(1, true, Some(50), true)));
        let size = Selection {
            min_size: 10,
            categories: vec!["rust".to_owned()],
            ..Selection::default()
        };
        assert!(!size.matches(&class(5, false, None, true)));
        assert!(size.matches(&class(50, false, None, true)));
    }
}
