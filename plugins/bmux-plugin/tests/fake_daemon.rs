//! End-to-end monitor/ack behavior against an in-process fake bclean daemon.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bclean_bmux_plugin::daemon;
use bclean_bmux_plugin::monitor::Monitor;
use bclean_bmux_plugin::settings::Settings;
use bclean_bmux_plugin::status::{self, Connection, Style};
use bclean_bmux_plugin::tab_bar::{OWNER, Publisher, TabBar, TabBarError};
use bclean_model::{AlertEvent, Severity, VolumeStats};
use bclean_protocol::{
    ClientFrame, DaemonStatus, Endpoint, Event, PROTOCOL_VERSION, Request, Response, ServerFrame,
    ServerHello,
};
use bmux_tab_bar_plugin_api::status_items_list::Severity as BarSeverity;

fn endpoint(tag: &str) -> Endpoint {
    if cfg!(windows) {
        Endpoint::Namespaced(format!("bclean-bmux-{tag}-{}", std::process::id()))
    } else {
        Endpoint::Path(
            std::env::temp_dir()
                .join(format!("bclean-bmux-{tag}-{}", std::process::id()))
                .join("d.sock"),
        )
    }
}

fn volume(free: u64) -> VolumeStats {
    VolumeStats {
        mount_point: "/".to_owned(),
        fs_type: "apfs".to_owned(),
        total: 100,
        free,
        available: free,
        purgeable: None,
        at: 0,
    }
}

fn alert(key: &str) -> AlertEvent {
    AlertEvent {
        key: key.to_owned(),
        policy_id: Some("p".to_owned()),
        severity: Severity::Critical,
        title: format!("{key} almost full"),
        body: String::new(),
        volume: None,
        at: 0,
    }
}

/// Fake daemon state: active alerts and acknowledged keys.
#[derive(Default)]
struct State {
    alerts: Vec<AlertEvent>,
    acked: Vec<String>,
}

/// Runs a fake daemon on its own thread. After a client subscribes it pushes one volume sample
/// and one new alert.
fn spawn_daemon(endpoint: Endpoint, state: Arc<Mutex<State>>) {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = bclean_protocol::listen(&endpoint).unwrap();
            ready_tx.send(()).unwrap();
            loop {
                let (mut reader, mut writer) = bclean_protocol::accept(&listener).await.unwrap();
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    while let Ok(Some(frame)) = reader.read::<ClientFrame>().await {
                        let (id, request) = match frame {
                            ClientFrame::Hello(_) => {
                                let hello = ServerFrame::Hello(ServerHello {
                                    protocol_version: PROTOCOL_VERSION,
                                    daemon_version: "test".to_owned(),
                                    pid: 42,
                                });
                                writer.write(&hello).await.unwrap();
                                continue;
                            }
                            ClientFrame::Request { id, request } => (id, request),
                        };
                        let mut push = Vec::new();
                        let response = {
                            let mut state = state.lock().unwrap();
                            match request {
                                Request::Status => Response::Status(DaemonStatus {
                                    pid: 42,
                                    version: "test".to_owned(),
                                    started_at: 0,
                                    roots: Vec::new(),
                                    policies: 1,
                                    active_alerts: state.alerts.len(),
                                    watchers: Vec::new(),
                                }),
                                Request::Volumes => Response::Volumes {
                                    volumes: vec![volume(50)],
                                },
                                Request::Alerts => Response::Alerts {
                                    alerts: state.alerts.clone(),
                                },
                                Request::AckAlert { key } => {
                                    state.alerts.retain(|a| a.key != key);
                                    state.acked.push(key);
                                    Response::Ok
                                }
                                Request::Subscribe { .. } => {
                                    let new = alert("fresh");
                                    state.alerts.push(new.clone());
                                    push.push(Event::Volumes {
                                        volumes: vec![volume(8)],
                                    });
                                    push.push(Event::Alert(new));
                                    Response::Ok
                                }
                                other => panic!("unexpected request {other:?}"),
                            }
                        };
                        let reply = ServerFrame::Response {
                            id,
                            result: Ok(response),
                        };
                        writer.write(&reply).await.unwrap();
                        for event in push {
                            writer.write(&ServerFrame::Event(event)).await.unwrap();
                        }
                    }
                });
            }
        });
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn monitor_tracks_daemon_and_ack_clears_alerts() {
    let endpoint = endpoint("e2e");
    let state = Arc::new(Mutex::new(State {
        alerts: vec![alert("old")],
        acked: Vec::new(),
    }));
    spawn_daemon(endpoint.clone(), Arc::clone(&state));

    let settings = Settings::default();
    let options = daemon::client_options(endpoint, &settings, false);
    let mut monitor = Monitor::start(settings.clone(), options.clone()).unwrap();
    let shared = Arc::clone(monitor.shared());

    // Initial fetch, then the pushed volume sample and new alert.
    wait_until("pushed alert", || shared.snapshot().alerts.len() == 2);
    wait_until("pushed volume", || {
        shared
            .snapshot()
            .volumes
            .first()
            .is_some_and(|v| v.free == 8)
    });
    let snapshot = shared.snapshot();
    assert_eq!(snapshot.connection, Connection::Connected);
    assert_eq!(snapshot.daemon.as_ref().unwrap().pid, 42);
    assert_eq!(
        status::render_segment(&snapshot, &settings, Style::Plain),
        "● 92% /  ⚠ 2"
    );
    let pending = shared.take_pending();
    assert_eq!(pending.len(), 1, "only the pushed alert is new");
    assert_eq!(pending[0].key, "fresh");

    // Acknowledge everything through a separate connection, like the `ack-alerts` command.
    let count = daemon::block_on(async {
        let mut client = daemon::connect(&options).await.unwrap();
        daemon::ack_all(&mut client).await.unwrap()
    })
    .unwrap();
    assert_eq!(count, 2);
    assert_eq!(state.lock().unwrap().acked, ["old", "fresh"]);
    shared.clear_alerts();
    assert!(shared.snapshot().alerts.is_empty());

    monitor.stop();
}

/// Tab-bar stand-in recording what the monitor publishes.
#[derive(Clone, Default)]
struct Bar {
    item: Arc<Mutex<Option<(String, BarSeverity)>>>,
    toasts: Arc<Mutex<Vec<(String, BarSeverity)>>>,
}

impl TabBar for Bar {
    fn set_status_item(
        &mut self,
        _id: &str,
        text: &str,
        severity: BarSeverity,
    ) -> Result<(), TabBarError> {
        *self.item.lock().unwrap() = Some((text.to_owned(), severity));
        Ok(())
    }

    fn clear_status_item(&mut self, _id: &str) -> Result<(), TabBarError> {
        *self.item.lock().unwrap() = None;
        Ok(())
    }

    fn clear_status_items(&mut self) -> Result<(), TabBarError> {
        self.clear_status_item("")
    }

    fn status_item_owners(&mut self, _id: &str) -> Result<Vec<String>, TabBarError> {
        Ok(vec![OWNER.to_owned()])
    }

    fn notify(
        &mut self,
        title: &str,
        _body: Option<&str>,
        severity: BarSeverity,
    ) -> Result<(), TabBarError> {
        self.toasts
            .lock()
            .unwrap()
            .push((title.to_owned(), severity));
        Ok(())
    }
}

#[test]
fn monitor_publishes_status_item_and_toasts() {
    let endpoint = endpoint("bar");
    let state = Arc::new(Mutex::new(State {
        alerts: vec![alert("old")],
        acked: Vec::new(),
    }));
    spawn_daemon(endpoint.clone(), Arc::clone(&state));

    let settings = Settings::default();
    let options = daemon::client_options(endpoint, &settings, false);
    let bar = Bar::default();
    let publisher = Publisher::new(Box::new(bar.clone()), settings.clone());
    let mut monitor = Monitor::start_with_tab_bar(publisher, settings, options).unwrap();

    let item = || bar.item.lock().unwrap().clone();
    wait_until("published segment", || {
        item().is_some_and(|(text, _)| text == "● 92% /  ⚠ 2")
    });
    assert_eq!(item().unwrap().1, BarSeverity::Critical);
    // Only the alert pushed after subscribing is new; "old" was already active.
    wait_until("toast", || !bar.toasts.lock().unwrap().is_empty());
    assert_eq!(
        *bar.toasts.lock().unwrap(),
        [("fresh almost full".to_owned(), BarSeverity::Critical)]
    );

    monitor.shared().clear_alerts();
    assert_eq!(item().unwrap().0, "● 92% /");

    monitor.stop();
    monitor.shared().clear_tab_bar();
    assert_eq!(item(), None);
}
