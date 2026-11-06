#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Alert sinks for bclean: desktop notifications, command hooks, webhooks, and bmux.
//!
//! [`AlertRouter`] is built from the configured `[alerts.*]` sinks and delivers
//! [`AlertEvent`]s to a list of sink ids, honouring each sink's `enabled`, `min_severity`, and
//! per-(sink, event key) `debounce`.
//!
//! * `DESKTOP` uses `notify-rust` (Notification Center on macOS, freedesktop D-Bus via `zbus` on
//!   Linux, `WinRT` toasts on Windows). Delivery runs on a helper thread with a timeout.
//! * `COMMAND` runs a shell command (`/bin/sh -c` or `cmd.exe /D /S /C`) with the alert JSON on
//!   stdin, killed after the timeout.
//! * `WEBHOOK` sends the alert JSON with `ureq` (method, headers, global timeout). HTTPS uses the
//!   platform TLS stack on macOS/Windows; on Linux it requires the `rustls` feature.
//! * `BMUX` performs no I/O: it records the alert in an in-memory bounded [`BmuxBuffer`] that the
//!   daemon exposes to the bmux plugin over IPC.
//!
//! Time comes from a [`Clock`] and external I/O from a [`Transport`], so delivery is fully
//! testable with [`FakeClock`] and a fake transport.

mod bmux;
mod clock;
mod transport;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bclean_model::{AlertEvent, AlertSink, AlertSpec};
use serde::{Deserialize, Serialize};

pub use bmux::{ActiveAlert, BmuxBuffer};
pub use clock::{Clock, FakeClock, SystemClock};
pub use transport::{DeliveryError, SystemTransport, Transport};

/// Default timeout for command and webhook sinks and desktop notifications.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default capacity of the bmux active-alert buffer.
pub const DEFAULT_BMUX_CAPACITY: usize = 64;

/// JSON payload sent to command and webhook sinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertPayload {
    /// Id of the sink delivering the alert.
    pub sink: String,
    /// The alert itself (fields flattened into the payload).
    #[serde(flatten)]
    pub event: AlertEvent,
}

/// Why an alert was not delivered to a sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Suppression {
    /// The sink id is not configured.
    UnknownSink,
    /// The sink is disabled.
    Disabled,
    /// The event's severity is below the sink's `min_severity`.
    BelowSeverity,
    /// The same event key was delivered to this sink within its debounce window.
    Debounced,
}

/// Delivery result for one sink.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DeliveryOutcome {
    /// Delivered.
    Delivered,
    /// Not attempted.
    Suppressed(Suppression),
    /// Attempted and failed.
    Failed(String),
}

/// Per-sink results of one [`AlertRouter::dispatch`] call, keyed by sink id.
pub type DispatchReport = BTreeMap<String, DeliveryOutcome>;

/// Routes alert events to configured sinks.
pub struct AlertRouter {
    sinks: BTreeMap<String, AlertSpec>,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn Transport>,
    bmux: BmuxBuffer,
    timeout: Duration,
    last_sent: Mutex<BTreeMap<(String, String), i64>>,
}

impl std::fmt::Debug for AlertRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlertRouter")
            .field("sinks", &self.sinks)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl AlertRouter {
    /// Creates a router with the system clock and real transports.
    #[must_use]
    pub fn new(sinks: BTreeMap<String, AlertSpec>) -> Self {
        Self {
            sinks,
            clock: Arc::new(SystemClock),
            transport: Arc::new(SystemTransport),
            bmux: BmuxBuffer::new(DEFAULT_BMUX_CAPACITY),
            timeout: DEFAULT_TIMEOUT,
            last_sent: Mutex::new(BTreeMap::new()),
        }
    }

    /// Replaces the clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replaces the transport used by desktop, command, and webhook sinks.
    #[must_use]
    pub fn with_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = transport;
        self
    }

    /// Replaces the bmux buffer (for example to share one across routers after a reload).
    #[must_use]
    pub fn with_bmux_buffer(mut self, buffer: BmuxBuffer) -> Self {
        self.bmux = buffer;
        self
    }

    /// Sets the delivery timeout for each sink.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The bmux active-alert buffer (shared handle).
    #[must_use]
    pub const fn bmux(&self) -> &BmuxBuffer {
        &self.bmux
    }

    /// Configured sinks.
    #[must_use]
    pub const fn sinks(&self) -> &BTreeMap<String, AlertSpec> {
        &self.sinks
    }

    /// Delivers `event` to each of `sink_ids`, sequentially. Each sink is bounded by the
    /// router timeout, so a call never blocks forever.
    pub fn dispatch<S: AsRef<str>>(&self, event: &AlertEvent, sink_ids: &[S]) -> DispatchReport {
        let mut report = DispatchReport::new();
        for id in sink_ids {
            let id = id.as_ref();
            if report.contains_key(id) {
                continue;
            }
            let outcome = self.deliver_one(id, event);
            report.insert(id.to_owned(), outcome);
        }
        report
    }

    /// Delivers `event` to every configured sink.
    pub fn dispatch_all(&self, event: &AlertEvent) -> DispatchReport {
        let ids: Vec<String> = self.sinks.keys().cloned().collect();
        self.dispatch(event, &ids)
    }

    fn deliver_one(&self, id: &str, event: &AlertEvent) -> DeliveryOutcome {
        let Some(spec) = self.sinks.get(id) else {
            log::warn!("alert sink `{id}` is not configured");
            return DeliveryOutcome::Suppressed(Suppression::UnknownSink);
        };
        if !spec.enabled {
            return DeliveryOutcome::Suppressed(Suppression::Disabled);
        }
        if event.severity < spec.min_severity {
            return DeliveryOutcome::Suppressed(Suppression::BelowSeverity);
        }
        let now = self.clock.now();
        let debounce = i64::try_from(spec.debounce.as_secs()).unwrap_or(i64::MAX);
        let slot = (id.to_owned(), event.key.clone());
        {
            let last = self
                .last_sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(&at) = last.get(&slot)
                && now.saturating_sub(at) < debounce
            {
                return DeliveryOutcome::Suppressed(Suppression::Debounced);
            }
        }
        let result = self.send(id, &spec.sink, event, now);
        match result {
            Ok(()) => {
                let mut last = self
                    .last_sent
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                last.insert(slot, now);
                // Bound memory: forget entries far older than any debounce window.
                if last.len() > 4096 {
                    last.retain(|_, at| now.saturating_sub(*at) < 86_400 * 7);
                }
                DeliveryOutcome::Delivered
            }
            Err(err) => {
                log::warn!("alert sink `{id}` failed: {err}");
                DeliveryOutcome::Failed(err.to_string())
            }
        }
    }

    fn send(
        &self,
        id: &str,
        sink: &AlertSink,
        event: &AlertEvent,
        now: i64,
    ) -> Result<(), DeliveryError> {
        let payload = || {
            serde_json::to_vec(&AlertPayload {
                sink: id.to_owned(),
                event: event.clone(),
            })
            .map_err(|e| DeliveryError::Other(e.to_string()))
        };
        match sink {
            AlertSink::Bmux => {
                self.bmux.push(id, event.clone(), now);
                Ok(())
            }
            AlertSink::Desktop => self.transport.desktop(event, self.timeout),
            AlertSink::Command { run } => self.transport.command(run, &payload()?, self.timeout),
            AlertSink::Webhook {
                url,
                method,
                headers,
            } => self.transport.webhook(
                url,
                method.as_deref().unwrap_or("POST"),
                headers,
                &payload()?,
                self.timeout,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use bclean_model::{DurationSpec, Severity};

    use super::*;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        fail: bool,
    }

    impl Transport for Recorder {
        fn desktop(&self, event: &AlertEvent, _timeout: Duration) -> Result<(), DeliveryError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("desktop:{}", event.title));
            if self.fail {
                Err(DeliveryError::Other("boom".into()))
            } else {
                Ok(())
            }
        }

        fn command(
            &self,
            run: &str,
            payload: &[u8],
            _timeout: Duration,
        ) -> Result<(), DeliveryError> {
            let payload: AlertPayload = serde_json::from_slice(payload).unwrap();
            self.calls.lock().unwrap().push(format!(
                "command:{run}:{}:{}",
                payload.sink, payload.event.key
            ));
            Ok(())
        }

        fn webhook(
            &self,
            url: &str,
            method: &str,
            headers: &BTreeMap<String, String>,
            _body: &[u8],
            _timeout: Duration,
        ) -> Result<(), DeliveryError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("webhook:{method}:{url}:{}", headers.len()));
            Ok(())
        }
    }

    fn spec(sink: AlertSink, min: Severity, debounce_secs: u64) -> AlertSpec {
        AlertSpec {
            enabled: true,
            min_severity: min,
            debounce: DurationSpec(Duration::from_secs(debounce_secs)),
            sink,
        }
    }

    fn event(key: &str, severity: Severity) -> AlertEvent {
        AlertEvent {
            key: key.to_owned(),
            policy_id: Some("low_disk".to_owned()),
            severity,
            title: format!("title {key}"),
            body: "body".to_owned(),
            volume: None,
            at: 0,
        }
    }

    fn router(fail: bool) -> (AlertRouter, Arc<Recorder>, Arc<FakeClock>) {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "desktop".to_owned(),
            spec(AlertSink::Desktop, Severity::Warning, 60),
        );
        sinks.insert(
            "hook".to_owned(),
            spec(
                AlertSink::Command {
                    run: "cat".to_owned(),
                },
                Severity::Info,
                0,
            ),
        );
        sinks.insert(
            "web".to_owned(),
            spec(
                AlertSink::Webhook {
                    url: "https://example.invalid/x".to_owned(),
                    method: None,
                    headers: BTreeMap::from([("X-A".to_owned(), "1".to_owned())]),
                },
                Severity::Info,
                0,
            ),
        );
        sinks.insert("bmux".to_owned(), spec(AlertSink::Bmux, Severity::Info, 0));
        let mut off = spec(AlertSink::Desktop, Severity::Info, 0);
        off.enabled = false;
        sinks.insert("off".to_owned(), off);
        let recorder = Arc::new(Recorder {
            fail,
            ..Recorder::default()
        });
        let clock = Arc::new(FakeClock::new(1_000));
        let router = AlertRouter::new(sinks)
            .with_clock(clock.clone())
            .with_transport(recorder.clone());
        (router, recorder, clock)
    }

    #[test]
    fn routes_and_filters() {
        let (router, recorder, _) = router(false);
        let report = router.dispatch(
            &event("k", Severity::Info),
            &["desktop", "hook", "web", "bmux", "off", "missing"],
        );
        assert_eq!(
            report["desktop"],
            DeliveryOutcome::Suppressed(Suppression::BelowSeverity)
        );
        assert_eq!(report["hook"], DeliveryOutcome::Delivered);
        assert_eq!(report["web"], DeliveryOutcome::Delivered);
        assert_eq!(report["bmux"], DeliveryOutcome::Delivered);
        assert_eq!(
            report["off"],
            DeliveryOutcome::Suppressed(Suppression::Disabled)
        );
        assert_eq!(
            report["missing"],
            DeliveryOutcome::Suppressed(Suppression::UnknownSink)
        );
        let calls = recorder.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "command:cat:hook:k".to_owned(),
                "webhook:POST:https://example.invalid/x:1".to_owned()
            ]
        );
        assert_eq!(router.bmux().active().len(), 1);
    }

    #[test]
    fn debounces_per_sink_and_key() {
        let (router, recorder, clock) = router(false);
        let critical = event("vol:/", Severity::Critical);
        assert_eq!(
            router.dispatch(&critical, &["desktop"])["desktop"],
            DeliveryOutcome::Delivered
        );
        clock.advance(30);
        assert_eq!(
            router.dispatch(&critical, &["desktop"])["desktop"],
            DeliveryOutcome::Suppressed(Suppression::Debounced)
        );
        // A different key is not debounced.
        assert_eq!(
            router.dispatch(&event("vol:/data", Severity::Critical), &["desktop"])["desktop"],
            DeliveryOutcome::Delivered
        );
        clock.advance(31);
        assert_eq!(
            router.dispatch(&critical, &["desktop"])["desktop"],
            DeliveryOutcome::Delivered
        );
        assert_eq!(recorder.calls.lock().unwrap().len(), 3);
    }

    #[test]
    fn failures_are_not_debounced() {
        let (router, recorder, _) = router(true);
        let critical = event("k", Severity::Critical);
        assert!(matches!(
            router.dispatch(&critical, &["desktop"])["desktop"],
            DeliveryOutcome::Failed(_)
        ));
        assert!(matches!(
            router.dispatch(&critical, &["desktop"])["desktop"],
            DeliveryOutcome::Failed(_)
        ));
        assert_eq!(recorder.calls.lock().unwrap().len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn real_command_sink_receives_json() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payload.json");
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "hook".to_owned(),
            spec(
                AlertSink::Command {
                    run: format!("cat > '{}'", out.display()),
                },
                Severity::Info,
                0,
            ),
        );
        let router = AlertRouter::new(sinks);
        let report = router.dispatch(&event("k", Severity::Warning), &["hook"]);
        assert_eq!(report["hook"], DeliveryOutcome::Delivered);
        let payload: AlertPayload = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(payload.sink, "hook");
        assert_eq!(payload.event.key, "k");
    }

    #[cfg(unix)]
    #[test]
    fn real_command_sink_times_out_and_reports_failure() {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "slow".to_owned(),
            spec(
                AlertSink::Command {
                    run: "sleep 10".to_owned(),
                },
                Severity::Info,
                0,
            ),
        );
        sinks.insert(
            "bad".to_owned(),
            spec(
                AlertSink::Command {
                    run: "exit 4".to_owned(),
                },
                Severity::Info,
                0,
            ),
        );
        let router = AlertRouter::new(sinks).with_timeout(Duration::from_millis(200));
        let start = std::time::Instant::now();
        let report = router.dispatch(&event("k", Severity::Warning), &["slow", "bad"]);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(report["slow"], DeliveryOutcome::Failed(ref m) if m.contains("timed out"))
        );
        assert!(matches!(report["bad"], DeliveryOutcome::Failed(_)));
    }

    #[test]
    fn webhook_failure_is_bounded() {
        let mut sinks = BTreeMap::new();
        sinks.insert(
            "web".to_owned(),
            spec(
                AlertSink::Webhook {
                    // Port 9 on localhost: connection refused (or times out quickly).
                    url: "http://127.0.0.1:9/hook".to_owned(),
                    method: Some("PUT".to_owned()),
                    headers: BTreeMap::new(),
                },
                Severity::Info,
                0,
            ),
        );
        let router = AlertRouter::new(sinks).with_timeout(Duration::from_secs(2));
        let start = std::time::Instant::now();
        let report = router.dispatch(&event("k", Severity::Warning), &["web"]);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(matches!(report["web"], DeliveryOutcome::Failed(_)));
    }
}
