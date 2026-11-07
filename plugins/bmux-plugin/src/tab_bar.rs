//! Publishing the disk-pressure segment and new alerts to bmux's tab bar.
//!
//! The tab bar (`bmux.tab_bar`) exposes typed services that let plugins contribute a right-side
//! status item and raise attach toasts. Items are namespaced by the *caller plugin id* that the
//! host attaches to each service request, so an item is only correct if bmux attributes the call
//! to `bclean`.
//!
//! [`Publisher`] holds the policy (what to show, dedupe, notify-once, cleanup) and talks to the
//! tab bar through the small [`TabBar`] trait, which [`ServiceTabBar`] implements over any bmux
//! [`ServiceCaller`] and tests replace with a recorder.
//!
//! The publisher never ships a misattributed item. Its first successful `set-status-item` is
//! followed by a `list-status-items` check that the item is owned by `bclean`. If the host
//! relayed the call under another identity (bmux before `b3dc1afa` attributed relayed calls to
//! `bmux.core`), the item is removed again and publishing is disabled for this activation. A
//! transport failure (for example, an older bmux with no route from a dynamically loaded plugin
//! to the bundled tab-bar provider) also disables it. Either way the `segment`/`status` commands keep working.

use std::collections::BTreeMap;

use bclean_model::{AlertEvent, Severity as AlertSeverity};
use bmux_plugin::{ServiceCaller, ServiceCallerDispatchClient, block_on_typed_dispatch};
use bmux_tab_bar_plugin_api::{
    notifications, status_items, status_items_list::Severity, status_items_state,
};

use crate::settings::Settings;
use crate::status::{self, Connection, Pressure, Snapshot, Style};

/// The plugin id bmux must attribute our tab-bar calls to.
pub const OWNER: &str = "bclean";
/// Id of the single status item this plugin owns.
pub const STATUS_ITEM_ID: &str = "disk";
/// Render priority of the status item (higher renders first, drops last).
pub const STATUS_ITEM_PRIORITY: i32 = 10;

/// Failure of one tab-bar call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabBarError {
    /// The call never reached the tab bar (no route, missing capability, provider down).
    Unavailable(String),
    /// The tab bar rejected the request (validation or limits).
    Rejected(String),
}

impl std::fmt::Display for TabBarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(f, "unavailable: {reason}"),
            Self::Rejected(reason) => write!(f, "rejected: {reason}"),
        }
    }
}

/// The tab-bar operations the publisher needs.
pub trait TabBar: Send {
    /// Creates or replaces this plugin's status item `id`.
    ///
    /// # Errors
    ///
    /// * [`TabBarError::Unavailable`] if the call cannot reach the tab bar
    /// * [`TabBarError::Rejected`] if the tab bar refuses the item
    fn set_status_item(
        &mut self,
        id: &str,
        text: &str,
        severity: Severity,
    ) -> Result<(), TabBarError>;

    /// Removes the caller's status item `id` (as attributed by the host).
    ///
    /// # Errors
    ///
    /// * [`TabBarError::Unavailable`] if the call cannot reach the tab bar
    fn clear_status_item(&mut self, id: &str) -> Result<(), TabBarError>;

    /// Removes every status item attributed to the caller.
    ///
    /// # Errors
    ///
    /// * [`TabBarError::Unavailable`] if the call cannot reach the tab bar
    fn clear_status_items(&mut self) -> Result<(), TabBarError>;

    /// Owners of the status items currently registered under `id`.
    ///
    /// # Errors
    ///
    /// * [`TabBarError::Unavailable`] if the call cannot reach the tab bar
    fn status_item_owners(&mut self, id: &str) -> Result<Vec<String>, TabBarError>;

    /// Raises a toast.
    ///
    /// # Errors
    ///
    /// * [`TabBarError::Unavailable`] if the call cannot reach the tab bar
    /// * [`TabBarError::Rejected`] if the tab bar refuses the toast
    fn notify(
        &mut self,
        title: &str,
        body: Option<&str>,
        severity: Severity,
    ) -> Result<(), TabBarError>;
}

/// [`TabBar`] over the generated `bmux_tab_bar_plugin_api` clients and a bmux service caller
/// (the plugin's lifecycle context).
pub struct ServiceTabBar<C> {
    caller: C,
}

impl<C> ServiceTabBar<C> {
    /// Wraps `caller`.
    #[must_use]
    pub const fn new(caller: C) -> Self {
        Self { caller }
    }
}

fn unavailable(error: impl std::fmt::Display) -> TabBarError {
    TabBarError::Unavailable(error.to_string())
}

impl<C: ServiceCaller + Send + Sync> TabBar for ServiceTabBar<C> {
    fn set_status_item(
        &mut self,
        id: &str,
        text: &str,
        severity: Severity,
    ) -> Result<(), TabBarError> {
        let mut client = ServiceCallerDispatchClient::new(&self.caller);
        block_on_typed_dispatch(status_items::client::set_status_item(
            &mut client,
            id.to_owned(),
            text.to_owned(),
            severity,
            STATUS_ITEM_PRIORITY,
            None,
        ))
        .map_err(unavailable)?
        .map_err(|e| TabBarError::Rejected(format!("{e:?}")))
    }

    fn clear_status_item(&mut self, id: &str) -> Result<(), TabBarError> {
        let mut client = ServiceCallerDispatchClient::new(&self.caller);
        block_on_typed_dispatch(status_items::client::clear_status_item(
            &mut client,
            id.to_owned(),
        ))
        .map(|_removed| ())
        .map_err(unavailable)
    }

    fn clear_status_items(&mut self) -> Result<(), TabBarError> {
        let mut client = ServiceCallerDispatchClient::new(&self.caller);
        block_on_typed_dispatch(status_items::client::clear_status_items(&mut client))
            .map(|_count| ())
            .map_err(unavailable)
    }

    fn status_item_owners(&mut self, id: &str) -> Result<Vec<String>, TabBarError> {
        let mut client = ServiceCallerDispatchClient::new(&self.caller);
        let items = block_on_typed_dispatch(status_items_state::client::list_status_items(
            &mut client,
            None,
        ))
        .map_err(unavailable)?;
        Ok(items
            .into_iter()
            .filter(|item| item.id == id)
            .map(|item| item.owner)
            .collect())
    }

    fn notify(
        &mut self,
        title: &str,
        body: Option<&str>,
        severity: Severity,
    ) -> Result<(), TabBarError> {
        let mut client = ServiceCallerDispatchClient::new(&self.caller);
        block_on_typed_dispatch(notifications::client::notify(
            &mut client,
            title.to_owned(),
            body.map(str::to_owned),
            severity,
            None,
        ))
        .map_err(unavailable)?
        .map(|_id| ())
        .map_err(|e| TabBarError::Rejected(format!("{e:?}")))
    }
}

/// The status item the publisher wants shown, or `None` to show nothing.
#[must_use]
pub fn desired_item(snapshot: &Snapshot, settings: &Settings) -> Option<(String, Severity)> {
    match snapshot.connection {
        // Nothing is known yet; keep whatever is (not) shown until the first result.
        Connection::Connecting => None,
        Connection::Offline(_) => settings.offline_indicator.then(|| {
            (
                status::render_segment(snapshot, settings, Style::Plain),
                Severity::Normal,
            )
        }),
        Connection::Connected => {
            let text = status::render_segment(snapshot, settings, Style::Plain);
            (!text.is_empty()).then(|| (text, status_severity(snapshot, settings)))
        }
    }
}

/// Severity of the status item: the worse of the most pressured volume (by the configured
/// thresholds) and the active alerts.
#[must_use]
pub fn status_severity(snapshot: &Snapshot, settings: &Settings) -> Severity {
    let volume = status::most_pressured(&snapshot.volumes, &settings.volumes)
        .map_or(Pressure::Ok, |v| {
            Pressure::classify(v.used_fraction(), settings.thresholds())
        });
    let alert = snapshot.alerts.iter().map(|a| a.severity).max();
    match (volume, alert) {
        (Pressure::Critical, _) | (_, Some(AlertSeverity::Critical)) => Severity::Critical,
        (Pressure::Warning, _) | (_, Some(AlertSeverity::Warning)) => Severity::Warning,
        (Pressure::Ok, Some(AlertSeverity::Info)) => Severity::Info,
        (Pressure::Ok, None) => Severity::Normal,
    }
}

/// Toast severity for a bclean alert severity.
#[must_use]
pub const fn toast_severity(severity: AlertSeverity) -> Severity {
    match severity {
        AlertSeverity::Info => Severity::Info,
        AlertSeverity::Warning => Severity::Warning,
        AlertSeverity::Critical => Severity::Critical,
    }
}

/// Whether the tab bar is (still) usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// No call has succeeded yet.
    Unverified,
    /// A status item was published and confirmed to be owned by `bclean`.
    Verified,
    /// Publishing is disabled for this activation; the reason is logged once.
    Disabled(String),
}

/// Publishes the monitor's view to the tab bar: one deduplicated status item plus one toast per
/// newly raised (or escalated) alert.
pub struct Publisher {
    tab_bar: Box<dyn TabBar>,
    settings: Settings,
    route: Route,
    /// What is currently shown (`None` = nothing).
    shown: Option<(String, Severity)>,
    /// Alerts already toasted, by key, with the severity they were toasted at.
    notified: BTreeMap<String, AlertSeverity>,
}

impl Publisher {
    /// A publisher using `tab_bar` under `settings`.
    #[must_use]
    pub fn new(tab_bar: Box<dyn TabBar>, settings: Settings) -> Self {
        Self {
            tab_bar,
            settings,
            route: Route::Unverified,
            shown: None,
            notified: BTreeMap::new(),
        }
    }

    /// Current route state.
    #[must_use]
    pub const fn route(&self) -> &Route {
        &self.route
    }

    fn disable(&mut self, reason: String) {
        eprintln!("bclean: tab-bar status and toasts disabled: {reason}");
        self.route = Route::Disabled(reason);
        self.shown = None;
    }

    /// Brings the status item in line with `snapshot`. Only changes (text or severity) are sent.
    pub fn publish(&mut self, snapshot: &Snapshot) {
        // Forget toasted alerts that are no longer active so a re-raised alert toasts again.
        self.notified
            .retain(|key, _| snapshot.alerts.iter().any(|a| &a.key == key));
        if matches!(self.route, Route::Disabled(_)) {
            return;
        }
        if snapshot.connection == Connection::Connecting {
            return;
        }
        let desired = desired_item(snapshot, &self.settings);
        if desired == self.shown {
            return;
        }
        match desired {
            Some((text, severity)) => self.show(text, severity),
            None => match self.tab_bar.clear_status_item(STATUS_ITEM_ID) {
                Ok(()) => self.shown = None,
                Err(error) => self.disable(error.to_string()),
            },
        }
    }

    fn show(&mut self, text: String, severity: Severity) {
        match self
            .tab_bar
            .set_status_item(STATUS_ITEM_ID, &text, severity)
        {
            Ok(()) => {}
            Err(TabBarError::Rejected(reason)) => {
                // Leave `shown` unchanged so the next change retries.
                eprintln!("bclean: tab bar rejected status item: {reason}");
                return;
            }
            Err(error @ TabBarError::Unavailable(_)) => {
                self.disable(error.to_string());
                return;
            }
        }
        if self.route == Route::Unverified && !self.verify_owner() {
            return;
        }
        self.shown = Some((text, severity));
    }

    /// Confirms the item just set is attributed to `bclean`; otherwise removes it and disables.
    fn verify_owner(&mut self) -> bool {
        let owners = match self.tab_bar.status_item_owners(STATUS_ITEM_ID) {
            Ok(owners) => owners,
            Err(error) => {
                let _ = self.tab_bar.clear_status_item(STATUS_ITEM_ID);
                self.disable(format!("cannot verify status item owner: {error}"));
                return false;
            }
        };
        if owners.iter().any(|owner| owner == OWNER) {
            self.route = Route::Verified;
            return true;
        }
        // The host attributed the call to someone else: remove that item under the same
        // identity and stop rather than keep publishing in another plugin's namespace.
        let _ = self.tab_bar.clear_status_item(STATUS_ITEM_ID);
        self.disable(format!(
            "bmux attributed the status item to {owners:?} instead of '{OWNER}'"
        ));
        false
    }

    /// Raises a toast for `alert` unless it was already toasted at the same or a higher
    /// severity, notifications are off, or the route is not verified.
    pub fn alert_raised(&mut self, alert: &AlertEvent) {
        if !self.settings.notify || self.route != Route::Verified {
            return;
        }
        if self
            .notified
            .get(&alert.key)
            .is_some_and(|&previous| previous >= alert.severity)
        {
            return;
        }
        let body = Some(alert.body.as_str()).filter(|b| !b.trim().is_empty());
        match self
            .tab_bar
            .notify(&alert.title, body, toast_severity(alert.severity))
        {
            Ok(()) => {
                self.notified.insert(alert.key.clone(), alert.severity);
            }
            Err(TabBarError::Rejected(reason)) => {
                eprintln!("bclean: tab bar rejected toast: {reason}");
            }
            Err(error @ TabBarError::Unavailable(_)) => self.disable(error.to_string()),
        }
    }

    /// Removes everything this plugin shows (on deactivate). A no-op if nothing was ever
    /// published under `bclean`.
    pub fn clear(&mut self) {
        if self.route == Route::Verified {
            let _ = self.tab_bar.clear_status_items();
        }
        self.shown = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use bclean_model::VolumeStats;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Set(String, Severity),
        Clear,
        ClearAll,
        List,
        Notify(String, Option<String>, Severity),
    }

    /// Records calls; the owner reported by `list` and failures are configurable.
    #[derive(Clone)]
    struct Recorder {
        calls: Arc<Mutex<Vec<Call>>>,
        owner: String,
        fail: Option<TabBarError>,
    }

    impl Recorder {
        fn new(owner: &str) -> Self {
            Self {
                calls: Arc::default(),
                owner: owner.to_owned(),
                fail: None,
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: Call) -> Result<(), TabBarError> {
            self.calls.lock().unwrap().push(call);
            self.fail.clone().map_or(Ok(()), Err)
        }
    }

    impl TabBar for Recorder {
        fn set_status_item(
            &mut self,
            _id: &str,
            text: &str,
            severity: Severity,
        ) -> Result<(), TabBarError> {
            self.record(Call::Set(text.to_owned(), severity))
        }

        fn clear_status_item(&mut self, _id: &str) -> Result<(), TabBarError> {
            self.record(Call::Clear)
        }

        fn clear_status_items(&mut self) -> Result<(), TabBarError> {
            self.record(Call::ClearAll)
        }

        fn status_item_owners(&mut self, _id: &str) -> Result<Vec<String>, TabBarError> {
            self.record(Call::List)?;
            Ok(vec![self.owner.clone()])
        }

        fn notify(
            &mut self,
            title: &str,
            body: Option<&str>,
            severity: Severity,
        ) -> Result<(), TabBarError> {
            self.record(Call::Notify(
                title.to_owned(),
                body.map(str::to_owned),
                severity,
            ))
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

    fn alert(key: &str, severity: AlertSeverity) -> AlertEvent {
        AlertEvent {
            key: key.to_owned(),
            policy_id: None,
            severity,
            title: format!("{key} filling up"),
            body: "free some space".to_owned(),
            volume: None,
            at: 0,
        }
    }

    fn connected(free: u64, alerts: Vec<AlertEvent>) -> Snapshot {
        Snapshot {
            connection: Connection::Connected,
            daemon: None,
            volumes: vec![volume(free)],
            alerts,
            reclaimable: None,
            updated_at: Some(0),
        }
    }

    fn offline() -> Snapshot {
        Snapshot {
            connection: Connection::Offline("daemon not running".to_owned()),
            ..Snapshot::default()
        }
    }

    fn new_publisher(recorder: &Recorder, settings: Settings) -> Publisher {
        Publisher::new(Box::new(recorder.clone()), settings)
    }

    #[test]
    fn severity_follows_thresholds_and_alerts() {
        let settings = Settings::default();
        assert_eq!(
            status_severity(&connected(60, Vec::new()), &settings),
            Severity::Normal
        );
        assert_eq!(
            status_severity(&connected(15, Vec::new()), &settings),
            Severity::Warning
        );
        assert_eq!(
            status_severity(&connected(5, Vec::new()), &settings),
            Severity::Critical
        );
        assert_eq!(
            status_severity(
                &connected(60, vec![alert("a", AlertSeverity::Info)]),
                &settings
            ),
            Severity::Info
        );
        assert_eq!(
            status_severity(
                &connected(15, vec![alert("a", AlertSeverity::Critical)]),
                &settings
            ),
            Severity::Critical
        );
        let strict = Settings {
            warning_percent: 30.0,
            critical_percent: 50.0,
            ..Settings::default()
        };
        assert_eq!(
            status_severity(&connected(60, Vec::new()), &strict),
            Severity::Warning
        );
        assert_eq!(toast_severity(AlertSeverity::Info), Severity::Info);
        assert_eq!(toast_severity(AlertSeverity::Warning), Severity::Warning);
        assert_eq!(toast_severity(AlertSeverity::Critical), Severity::Critical);
    }

    #[test]
    fn publishes_on_change_only_and_verifies_owner_once() {
        let recorder = Recorder::new(OWNER);
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.publish(&Snapshot::default()); // still connecting: nothing
        publisher.publish(&connected(13, Vec::new()));
        publisher.publish(&connected(13, Vec::new())); // unchanged
        let mut newer = connected(13, Vec::new());
        newer.updated_at = Some(99); // same text and severity
        publisher.publish(&newer);
        publisher.publish(&connected(13, vec![alert("a", AlertSeverity::Warning)]));
        publisher.publish(&connected(5, vec![alert("a", AlertSeverity::Warning)]));
        assert_eq!(
            recorder.calls(),
            [
                Call::Set("◕ 87% /".to_owned(), Severity::Warning),
                Call::List,
                Call::Set("◕ 87% /  ⚠ 1".to_owned(), Severity::Warning),
                Call::Set("● 95% /  ⚠ 1".to_owned(), Severity::Critical),
            ]
        );
        assert_eq!(publisher.route(), &Route::Verified);
    }

    #[test]
    fn offline_shows_dim_indicator_or_clears() {
        let recorder = Recorder::new(OWNER);
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.publish(&connected(60, Vec::new()));
        publisher.publish(&offline());
        publisher.publish(&offline());
        assert_eq!(
            recorder.calls()[2..],
            [Call::Set("◌ bclean".to_owned(), Severity::Normal)]
        );

        let recorder = Recorder::new(OWNER);
        let settings = Settings {
            offline_indicator: false,
            ..Settings::default()
        };
        let mut publisher = new_publisher(&recorder, settings);
        publisher.publish(&connected(60, Vec::new()));
        publisher.publish(&offline());
        publisher.publish(&offline());
        assert_eq!(recorder.calls()[2..], [Call::Clear]);
    }

    #[test]
    fn hidden_ok_volume_clears_item() {
        let recorder = Recorder::new(OWNER);
        let settings = Settings {
            show_when_ok: false,
            ..Settings::default()
        };
        let mut publisher = new_publisher(&recorder, settings);
        publisher.publish(&connected(15, Vec::new()));
        publisher.publish(&connected(60, Vec::new()));
        assert_eq!(recorder.calls()[2..], [Call::Clear]);
    }

    #[test]
    fn notifies_once_per_alert_key_and_on_escalation() {
        let recorder = Recorder::new(OWNER);
        let mut publisher = new_publisher(&recorder, Settings::default());
        let warning = alert("a", AlertSeverity::Warning);
        publisher.publish(&connected(60, vec![warning.clone()]));
        publisher.alert_raised(&warning);
        publisher.alert_raised(&warning);
        let critical = alert("a", AlertSeverity::Critical);
        publisher.alert_raised(&critical);
        publisher.alert_raised(&warning); // downgrade: no toast
        let toasts: Vec<Call> = recorder
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Notify(..)))
            .collect();
        assert_eq!(
            toasts,
            [
                Call::Notify(
                    "a filling up".to_owned(),
                    Some("free some space".to_owned()),
                    Severity::Warning
                ),
                Call::Notify(
                    "a filling up".to_owned(),
                    Some("free some space".to_owned()),
                    Severity::Critical
                ),
            ]
        );

        // Once the alert is gone (acknowledged), raising it again toasts again.
        publisher.publish(&connected(60, Vec::new()));
        publisher.alert_raised(&warning);
        assert!(matches!(
            recorder.calls().last(),
            Some(Call::Notify(_, _, Severity::Warning))
        ));
    }

    #[test]
    fn notify_respects_setting_and_empty_body() {
        let recorder = Recorder::new(OWNER);
        let settings = Settings {
            notify: false,
            ..Settings::default()
        };
        let mut quiet = new_publisher(&recorder, settings);
        quiet.publish(&connected(60, Vec::new()));
        quiet.alert_raised(&alert("a", AlertSeverity::Critical));
        assert!(
            !recorder
                .calls()
                .iter()
                .any(|c| matches!(c, Call::Notify(..)))
        );

        let recorder = Recorder::new(OWNER);
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.publish(&connected(60, Vec::new()));
        let mut bare = alert("b", AlertSeverity::Info);
        bare.body = "  ".to_owned();
        publisher.alert_raised(&bare);
        assert_eq!(
            recorder.calls().last(),
            Some(&Call::Notify(
                "b filling up".to_owned(),
                None,
                Severity::Info
            ))
        );
    }

    #[test]
    fn misattributed_item_is_removed_and_publishing_stops() {
        let recorder = Recorder::new("bmux.core");
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.publish(&connected(13, Vec::new()));
        publisher.alert_raised(&alert("a", AlertSeverity::Critical));
        publisher.publish(&connected(5, Vec::new()));
        publisher.clear();
        assert_eq!(
            recorder.calls(),
            [
                Call::Set("◕ 87% /".to_owned(), Severity::Warning),
                Call::List,
                Call::Clear,
            ]
        );
        assert!(matches!(publisher.route(), Route::Disabled(_)));
    }

    #[test]
    fn unavailable_tab_bar_disables_after_one_attempt() {
        let mut recorder = Recorder::new(OWNER);
        recorder.fail = Some(TabBarError::Unavailable("no route".to_owned()));
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.publish(&connected(13, Vec::new()));
        publisher.publish(&connected(5, Vec::new()));
        publisher.alert_raised(&alert("a", AlertSeverity::Critical));
        publisher.clear();
        assert_eq!(
            recorder.calls(),
            [Call::Set("◕ 87% /".to_owned(), Severity::Warning)]
        );
        assert_eq!(
            publisher.route(),
            &Route::Disabled("unavailable: no route".to_owned())
        );
    }

    #[test]
    fn rejected_item_is_retried_on_next_change() {
        let mut recorder = Recorder::new(OWNER);
        recorder.fail = Some(TabBarError::Rejected("limit".to_owned()));
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.publish(&connected(13, Vec::new()));
        publisher.publish(&connected(13, Vec::new()));
        assert_eq!(recorder.calls().len(), 2, "not marked as shown");
        assert_eq!(publisher.route(), &Route::Unverified);
    }

    #[test]
    fn clear_on_deactivate_only_after_publishing() {
        let recorder = Recorder::new(OWNER);
        let mut publisher = new_publisher(&recorder, Settings::default());
        publisher.clear();
        assert_eq!(recorder.calls(), Vec::<Call>::new());
        publisher.publish(&connected(60, Vec::new()));
        publisher.clear();
        assert_eq!(recorder.calls().last(), Some(&Call::ClearAll));
    }
}
