//! In-memory "active alerts" buffer surfaced to the bmux plugin by the daemon.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use bclean_model::AlertEvent;
use serde::{Deserialize, Serialize};

/// An alert held in the bmux buffer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAlert {
    /// Buffer-unique id (monotonic), used to acknowledge.
    pub id: u64,
    /// Sink id that recorded it.
    pub sink: String,
    /// Most recent event for this key.
    pub event: AlertEvent,
    /// When the alert was first recorded (Unix seconds).
    pub first_at: i64,
    /// When it was last recorded (Unix seconds).
    pub last_at: i64,
    /// Number of times it was recorded while unacknowledged.
    pub count: u32,
    /// Whether a user acknowledged it.
    pub acknowledged: bool,
}

#[derive(Debug)]
struct State {
    next_id: u64,
    capacity: usize,
    alerts: VecDeque<ActiveAlert>,
}

/// Bounded, shareable buffer of alerts for bmux. Cloning shares the same buffer.
///
/// Repeated alerts with the same event key coalesce into one unacknowledged entry. When full,
/// the oldest acknowledged alert is evicted first, then the oldest alert overall.
#[derive(Debug, Clone)]
pub struct BmuxBuffer(Arc<Mutex<State>>);

impl BmuxBuffer {
    /// Creates a buffer holding at most `capacity` alerts (minimum 1).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(State {
            next_id: 1,
            capacity: capacity.max(1),
            alerts: VecDeque::new(),
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records an alert and returns its id.
    #[allow(clippy::must_use_candidate)] // recording is the point; the id is optional
    pub fn push(&self, sink: &str, event: AlertEvent, now: i64) -> u64 {
        let mut state = self.lock();
        if let Some(existing) = state
            .alerts
            .iter_mut()
            .find(|a| !a.acknowledged && a.event.key == event.key)
        {
            existing.event = event;
            existing.last_at = now;
            existing.count = existing.count.saturating_add(1);
            sink.clone_into(&mut existing.sink);
            return existing.id;
        }
        let id = state.next_id;
        state.next_id += 1;
        state.alerts.push_back(ActiveAlert {
            id,
            sink: sink.to_owned(),
            event,
            first_at: now,
            last_at: now,
            count: 1,
            acknowledged: false,
        });
        while state.alerts.len() > state.capacity {
            let victim = state
                .alerts
                .iter()
                .position(|a| a.acknowledged)
                .unwrap_or(0);
            state.alerts.remove(victim);
        }
        drop(state);
        id
    }

    /// Unacknowledged alerts, oldest first.
    #[must_use]
    pub fn active(&self) -> Vec<ActiveAlert> {
        self.lock()
            .alerts
            .iter()
            .filter(|a| !a.acknowledged)
            .cloned()
            .collect()
    }

    /// All buffered alerts (acknowledged included), oldest first.
    #[must_use]
    pub fn all(&self) -> Vec<ActiveAlert> {
        self.lock().alerts.iter().cloned().collect()
    }

    /// Acknowledges the alert with `id`. Returns whether it existed and was unacknowledged.
    #[allow(clippy::must_use_candidate)] // acknowledging is the point; the flag is optional
    pub fn acknowledge(&self, id: u64) -> bool {
        self.acknowledge_where(|a| a.id == id) > 0
    }

    /// Acknowledges every unacknowledged alert with event key `key`; returns how many.
    #[allow(clippy::must_use_candidate)] // acknowledging is the point; the count is optional
    pub fn acknowledge_key(&self, key: &str) -> usize {
        self.acknowledge_where(|a| a.event.key == key)
    }

    /// Acknowledges everything; returns how many were newly acknowledged.
    #[allow(clippy::must_use_candidate)] // acknowledging is the point; the count is optional
    pub fn acknowledge_all(&self) -> usize {
        self.acknowledge_where(|_| true)
    }

    fn acknowledge_where(&self, pred: impl Fn(&ActiveAlert) -> bool) -> usize {
        let mut n = 0;
        for alert in self
            .lock()
            .alerts
            .iter_mut()
            .filter(|a| !a.acknowledged && pred(a))
        {
            alert.acknowledged = true;
            n += 1;
        }
        n
    }

    /// Drops acknowledged alerts.
    pub fn clear_acknowledged(&self) {
        self.lock().alerts.retain(|a| !a.acknowledged);
    }

    /// Number of unacknowledged alerts.
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.lock()
            .alerts
            .iter()
            .filter(|a| !a.acknowledged)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use bclean_model::Severity;

    use super::*;

    fn event(key: &str) -> AlertEvent {
        AlertEvent {
            key: key.to_owned(),
            policy_id: None,
            severity: Severity::Warning,
            title: key.to_owned(),
            body: String::new(),
            volume: None,
            at: 0,
        }
    }

    #[test]
    fn coalesces_acknowledges_and_bounds() {
        let buffer = BmuxBuffer::new(2);
        let a = buffer.push("bmux", event("a"), 1);
        assert_eq!(buffer.push("bmux", event("a"), 2), a);
        assert_eq!(buffer.active()[0].count, 2);
        assert_eq!(buffer.active()[0].last_at, 2);
        assert!(buffer.acknowledge(a));
        assert!(!buffer.acknowledge(a));
        // A new alert for an acknowledged key is a fresh entry.
        let a2 = buffer.push("bmux", event("a"), 3);
        assert_ne!(a2, a);
        // Full: the acknowledged entry is evicted first.
        buffer.push("bmux", event("b"), 4);
        let all = buffer.all();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|x| !x.acknowledged));
        // Then the oldest.
        buffer.push("bmux", event("c"), 5);
        let keys: Vec<_> = buffer.active().into_iter().map(|x| x.event.key).collect();
        assert_eq!(keys, ["b", "c"]);
        assert_eq!(buffer.acknowledge_key("b"), 1);
        assert_eq!(buffer.active_count(), 1);
        assert_eq!(buffer.acknowledge_all(), 1);
        buffer.clear_acknowledged();
        assert_eq!(buffer.all(), Vec::new());
        // Clones share state.
        let clone = buffer.clone();
        clone.push("bmux", event("z"), 6);
        assert_eq!(buffer.active_count(), 1);
    }
}
