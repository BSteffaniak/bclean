//! Background monitor: a dedicated thread with its own Tokio runtime that keeps a live
//! [`Snapshot`] of the daemon, reconnecting with exponential backoff.
//!
//! Command handlers (running in the same provider process) read the shared state. When given a
//! [`Publisher`], the monitor also pushes the status segment and new-alert toasts to bmux's tab
//! bar as the state changes. Those calls run on this thread, outside any host invocation, through
//! the service caller captured at activation; [`crate::tab_bar`] explains how the publisher
//! verifies that bmux attributes them to `bclean` and disables itself otherwise.

use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use bclean_client::{ClientError, ClientOptions};
use bclean_model::AlertEvent;
use bclean_protocol::{Event, Request, Topic};
use tokio::sync::{mpsc, watch};

use crate::daemon;
use crate::settings::Settings;
use crate::status::{Connection, Snapshot};
use crate::tab_bar::Publisher;

/// Maximum number of queued, not-yet-shown alert notifications.
const MAX_PENDING: usize = 32;

/// State shared between the monitor thread and command handlers.
#[derive(Default)]
pub struct Shared {
    snapshot: Mutex<Snapshot>,
    pending: Mutex<Vec<AlertEvent>>,
    tab_bar: Mutex<Option<Publisher>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("snapshot", &self.snapshot)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Shared state that mirrors every change to the tab bar through `publisher`.
    #[must_use]
    pub fn with_tab_bar(publisher: Publisher) -> Self {
        Self {
            tab_bar: Mutex::new(Some(publisher)),
            ..Self::default()
        }
    }

    /// A copy of the current snapshot.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Takes the alerts raised since the last call.
    pub fn take_pending(&self) -> Vec<AlertEvent> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Applies `f` to the snapshot and publishes the result to the tab bar (if any).
    fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        let snapshot = {
            let mut snapshot = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut snapshot);
            snapshot.clone()
        };
        self.with_publisher(|publisher| publisher.publish(&snapshot));
    }

    fn with_publisher(&self, f: impl FnOnce(&mut Publisher)) {
        let mut tab_bar = self.tab_bar.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(publisher) = tab_bar.as_mut() {
            f(publisher);
        }
    }

    /// Removes everything published to the tab bar (on deactivate).
    pub fn clear_tab_bar(&self) {
        self.with_publisher(Publisher::clear);
    }

    /// Replaces the snapshot with a freshly fetched one.
    pub fn replace(&self, fresh: Snapshot) {
        self.update(|s| *s = fresh);
    }

    /// Marks the daemon unreachable, keeping the last known data out of the segment.
    pub fn set_offline(&self, reason: String) {
        self.update(|s| {
            *s = Snapshot {
                connection: Connection::Offline(reason),
                ..Snapshot::default()
            };
        });
    }

    /// Clears active alerts locally (after acknowledging them on the daemon).
    pub fn clear_alerts(&self) {
        self.update(|s| s.alerts.clear());
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Applies a pushed event. Returns whether a notification was queued.
    pub fn apply(&self, event: Event, notify: bool) -> bool {
        let alert = match event {
            Event::Volumes { volumes } => {
                self.update(|s| {
                    s.merge_volumes(volumes);
                    s.updated_at = Some(daemon::now());
                });
                return false;
            }
            Event::Alert(alert) => alert,
            Event::ScanProgress { .. } | Event::TreeUpdated(_) | Event::ActionProgress { .. } => {
                return false;
            }
        };
        let mut fresh = false;
        self.update(|s| {
            fresh = s.raise_alert(alert.clone());
            s.updated_at = Some(daemon::now());
        });
        if fresh {
            self.with_publisher(|publisher| publisher.alert_raised(&alert));
        }
        if !(fresh && notify) {
            return false;
        }
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.retain(|a| a.key != alert.key);
        pending.push(alert);
        let overflow = pending.len().saturating_sub(MAX_PENDING);
        pending.drain(..overflow);
        drop(pending);
        true
    }
}

/// Exponential backoff between reconnect attempts.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    current: Duration,
}

impl Backoff {
    /// Backoff starting at `min` and doubling up to `max`.
    #[must_use]
    pub fn new(min: Duration, max: Duration) -> Self {
        let min = min.min(max);
        Self {
            min,
            max,
            current: min,
        }
    }

    /// The next delay.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(self.max);
        delay
    }

    /// Resets after a successful connection.
    pub const fn reset(&mut self) {
        self.current = self.min;
    }
}

/// A running monitor. Dropping it stops the thread.
pub struct Monitor {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl Monitor {
    /// Starts the monitor thread.
    ///
    /// # Errors
    ///
    /// * If the thread cannot be spawned
    pub fn start(settings: Settings, options: ClientOptions) -> std::io::Result<Self> {
        Self::start_with(Shared::default(), settings, options)
    }

    /// Starts the monitor thread, mirroring state changes to the tab bar through `publisher`.
    ///
    /// # Errors
    ///
    /// * If the thread cannot be spawned
    pub fn start_with_tab_bar(
        publisher: Publisher,
        settings: Settings,
        options: ClientOptions,
    ) -> std::io::Result<Self> {
        Self::start_with(Shared::with_tab_bar(publisher), settings, options)
    }

    fn start_with(
        shared: Shared,
        settings: Settings,
        options: ClientOptions,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(shared);
        let (stop, stop_rx) = watch::channel(false);
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("bclean-bmux-monitor".to_owned())
            .spawn(move || run(&thread_shared, &settings, &options, stop_rx))?;
        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// The shared state.
    #[must_use]
    pub const fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// Signals the thread to stop and waits for it.
    pub fn stop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(shared: &Shared, settings: &Settings, options: &ClientOptions, stop: watch::Receiver<bool>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            shared.set_offline(format!("failed to start async runtime: {error}"));
            return;
        }
    };
    runtime.block_on(supervise(shared, settings, options, stop));
}

/// Resolves when stop is requested (or the monitor handle is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            return;
        }
    }
}

async fn supervise(
    shared: &Shared,
    settings: &Settings,
    options: &ClientOptions,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new(
        Duration::from_secs(settings.retry_min_secs),
        Duration::from_secs(settings.retry_max_secs),
    );
    // Refreshes never autostart: only the initial connection honors the setting.
    let mut refresh_options = options.clone();
    refresh_options.autostart = false;
    loop {
        let outcome = tokio::select! {
            () = stopped(&mut stop) => return,
            outcome = session(shared, settings, options, &refresh_options) => outcome,
        };
        let (error, was_connected) = outcome;
        shared.set_offline(daemon::describe(&error));
        if was_connected {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        tokio::select! {
            () = stopped(&mut stop) => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// One connected session. Returns the error that ended it and whether it got connected.
async fn session(
    shared: &Shared,
    settings: &Settings,
    options: &ClientOptions,
    refresh_options: &ClientOptions,
) -> (ClientError, bool) {
    let mut client = match daemon::connect(options).await {
        Ok(client) => client,
        Err(error) => return (error, false),
    };
    match daemon::fetch(&mut client).await {
        Ok(snapshot) => shared.replace(snapshot),
        Err(error) => return (error, false),
    }
    if let Err(error) = client
        .request(Request::Subscribe {
            topics: vec![Topic::Volumes, Topic::Alerts],
        })
        .await
    {
        return (error, true);
    }

    // The frame reader is not cancel-safe, so events are read by a dedicated task and handed
    // over through a channel; the select loop below only cancels channel receives and ticks.
    let (tx, mut rx) = mpsc::channel::<Result<Event, ClientError>>(64);
    let reader = tokio::spawn(async move {
        loop {
            let item = match client.next_event().await {
                Ok(Some(event)) => Ok(event),
                Ok(None) => Err(ClientError::Disconnected),
                Err(error) => Err(error),
            };
            let end = item.is_err();
            if tx.send(item).await.is_err() || end {
                return;
            }
        }
    });
    let _abort = AbortOnDrop(reader);

    let mut refresh = tokio::time::interval(settings.refresh_interval());
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    refresh.tick().await; // the first tick completes immediately; we just fetched
    loop {
        tokio::select! {
            item = rx.recv() => match item {
                Some(Ok(event)) => {
                    shared.apply(event, settings.notify);
                }
                Some(Err(error)) => return (error, true),
                None => return (ClientError::Disconnected, true),
            },
            _ = refresh.tick() => {
                // A full refresh on a separate connection picks up acknowledgements made by
                // other clients and volumes that disappeared.
                if let Ok(mut client) = daemon::connect(refresh_options).await
                    && let Ok(mut snapshot) = daemon::fetch(&mut client).await
                {
                    let current = shared.snapshot();
                    snapshot.connection = current.connection;
                    shared.replace(snapshot);
                }
            }
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_model::{Severity, VolumeStats};

    fn alert(key: &str) -> AlertEvent {
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
    fn backoff_doubles_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(2), Duration::from_secs(10));
        let delays: Vec<u64> = (0..5).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, [2, 4, 8, 10, 10]);
        backoff.reset();
        assert_eq!(backoff.next_delay().as_secs(), 2);
    }

    #[test]
    fn applies_events_and_queues_notifications() {
        let shared = Shared::default();
        shared.replace(Snapshot {
            connection: Connection::Connected,
            ..Snapshot::default()
        });
        assert!(!shared.apply(
            Event::Volumes {
                volumes: vec![VolumeStats {
                    mount_point: "/".to_owned(),
                    fs_type: "apfs".to_owned(),
                    total: 10,
                    free: 1,
                    available: 1,
                    purgeable: None,
                    at: 0,
                }],
            },
            true
        ));
        assert!(shared.apply(Event::Alert(alert("a")), true));
        assert!(!shared.apply(Event::Alert(alert("a")), true), "duplicate");
        assert!(!shared.apply(Event::Alert(alert("b")), false), "notify off");
        let snapshot = shared.snapshot();
        assert_eq!(snapshot.volumes.len(), 1);
        assert_eq!(snapshot.alerts.len(), 2);
        assert_eq!(shared.take_pending().len(), 1);
        assert_eq!(shared.take_pending(), Vec::<AlertEvent>::new());
        shared.clear_alerts();
        assert_eq!(shared.snapshot().alerts, Vec::<AlertEvent>::new());
    }

    #[test]
    fn monitor_reports_offline_and_stops_promptly() {
        let endpoint = if cfg!(windows) {
            bclean_protocol::Endpoint::Namespaced(format!(
                "bclean-plugin-monitor-{}",
                std::process::id()
            ))
        } else {
            bclean_protocol::Endpoint::Path(
                std::env::temp_dir()
                    .join(format!("bclean-plugin-monitor-{}", std::process::id()))
                    .join("d.sock"),
            )
        };
        let settings = Settings::default();
        let options = daemon::client_options(endpoint, &settings, false);
        let mut monitor = Monitor::start(settings, options).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while monitor.shared().snapshot().connection == Connection::Connecting {
            assert!(
                std::time::Instant::now() < deadline,
                "monitor never reported"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            monitor.shared().snapshot().connection,
            Connection::Offline("daemon not running".to_owned())
        );
        let started = std::time::Instant::now();
        monitor.stop();
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
