#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg_attr(feature = "static-bundled", allow(dead_code))]

//! bmux plugin surfacing bclean disk-pressure status and alerts.
//!
//! This is a bmux *native* plugin (a `cdylib` loaded by the bmux server). On activation it
//! starts a background [`monitor::Monitor`] thread that keeps a live view of the bclean daemon
//! (status, volumes, alerts, plus a `Subscribe { Volumes, Alerts }` event stream), reconnecting
//! with backoff while the daemon is down. The monitor mirrors that view to bmux's tab bar (a
//! status item plus toasts for new alerts) when bmux attributes those calls to this plugin; see
//! [`tab_bar`]. Commands read the same view:
//!
//! * `status`: detailed report (CLI) or a one-line status message (keybinding)
//! * `segment`: the compact status segment (e.g. `◕ 87% /  ⚠ 2`), for prompts and bars
//! * `ack-alerts`: acknowledge every active alert
//! * `open`: open a pane running the `bclean` TUI through the `bmux.tabs` service

pub mod daemon;
pub mod monitor;
pub mod settings;
pub mod status;
pub mod tab_bar;

use bclean_model::AlertEvent;
use bclean_protocol::Endpoint;
use bmux_plugin::ServiceCaller as _;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    COMMAND_OUTCOME_STATUS_MESSAGE_KEY, EXIT_USAGE, NativeLifecycleContext,
    record_command_outcome_metadata,
};
use bmux_tabs_plugin_api::tabs_commands::{
    self, PaneAck, PaneDirection, PaneMutationError,
    client::{CreateFloatingPaneRequest, LaunchPaneRequest},
};

use crate::monitor::Monitor;
use crate::settings::{OpenLayout, Settings};
use crate::status::{Connection, Snapshot, Style};
use crate::tab_bar::{Publisher, ServiceTabBar};

/// The plugin manifest (also used by `export_plugin!`).
pub const PLUGIN_MANIFEST: &str = include_str!("../plugin.toml");

/// Capability required to call `bmux.tabs` mutating commands.
const TABS_WRITE: &str = "bmux.tabs.write";
/// Plugin id of the `bmux.tabs` service provider.
const BMUX_TABS: &str = "bmux.tabs";

/// The plugin instance. bmux keeps one per provider process.
#[derive(Default)]
pub struct BcleanPlugin {
    monitor: Option<Monitor>,
}

impl BcleanPlugin {
    /// The monitor's live snapshot once it has reported in this process, otherwise a one-shot
    /// query (for example when the command runs in a process without the monitor).
    fn snapshot(&self, settings: &Settings, endpoint: &Endpoint) -> Snapshot {
        if let Some(monitor) = &self.monitor {
            let snapshot = monitor.shared().snapshot();
            if snapshot.connection != Connection::Connecting {
                return snapshot;
            }
        }
        daemon::one_shot_snapshot(&daemon::client_options(
            endpoint.clone(),
            settings,
            settings.autostart,
        ))
    }

    fn take_pending(&self) -> Vec<AlertEvent> {
        self.monitor
            .as_ref()
            .map(|m| m.shared().take_pending())
            .unwrap_or_default()
    }

    fn run_status(&self, context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        let (settings, endpoint) = invocation_config(context)?;
        let snapshot = self.snapshot(&settings, &endpoint);
        let pending = self.take_pending();
        let mut details = status::render_details(&snapshot, &settings, &endpoint.to_string());
        if !pending.is_empty() {
            details.push_str("new since last check:\n");
            for alert in &pending {
                details.push_str("  ");
                details.push_str(&status::alert_line(alert));
                details.push('\n');
            }
        }
        let summary = match pending.as_slice() {
            [] => status::render_summary(&snapshot, &settings),
            [only] => format!("bclean: {}", status::alert_line(only)),
            [first, rest @ ..] => format!(
                "bclean: {} (+{} more new)",
                status::alert_line(first),
                rest.len()
            ),
        };
        report(context, details.trim_end(), &summary);
        Ok(EXIT_OK)
    }

    fn run_segment(&self, context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        let (settings, endpoint) = invocation_config(context)?;
        let mut style = Style::Plain;
        for argument in &context.arguments {
            match argument.as_str() {
                "--ansi" | "--color" | "true" => style = Style::Ansi,
                "false" => {}
                other => {
                    return Err(PluginCommandError::new(
                        EXIT_USAGE,
                        format!("segment does not accept argument '{other}'"),
                    ));
                }
            }
        }
        let snapshot = self.snapshot(&settings, &endpoint);
        let segment = status::render_segment(&snapshot, &settings, style);
        let plain = status::render_segment(&snapshot, &settings, Style::Plain);
        report(context, &segment, &plain);
        Ok(EXIT_OK)
    }

    fn run_ack(&self, context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
        let (settings, endpoint) = invocation_config(context)?;
        // Acknowledging never starts a daemon: with no daemon there is nothing to acknowledge.
        let options = daemon::client_options(endpoint, &settings, false);
        let count = daemon::block_on(async {
            let mut client = daemon::connect(&options).await?;
            daemon::ack_all(&mut client).await
        })?
        .map_err(|e| {
            PluginCommandError::unavailable(format!("bclean: {}", daemon::describe(&e)))
        })?;
        if let Some(monitor) = &self.monitor {
            monitor.shared().clear_alerts();
        }
        let message = match count {
            0 => "bclean: no active alerts".to_owned(),
            1 => "bclean: acknowledged 1 alert".to_owned(),
            n => format!("bclean: acknowledged {n} alerts"),
        };
        report(context, &message, &message);
        Ok(EXIT_OK)
    }

    fn activate_monitor(&mut self, context: &NativeLifecycleContext) {
        self.monitor = None;
        let settings = match Settings::from_value(context.settings.clone()) {
            Ok(settings) => settings,
            Err(error) => {
                // Never fail activation (that would abort bmux startup); stay passive instead.
                eprintln!("bclean: invalid plugin settings, monitor disabled: {error}");
                return;
            }
        };
        if !settings.monitor {
            return;
        }
        let endpoint = match daemon::resolve_endpoint(&settings) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                eprintln!("bclean: cannot resolve daemon socket, monitor disabled: {error}");
                return;
            }
        };
        let options = daemon::client_options(endpoint, &settings, settings.autostart);
        let started = if settings.tab_bar {
            // The monitor thread calls the tab bar through this activation's context.
            let publisher = Publisher::new(
                Box::new(ServiceTabBar::new(context.clone())),
                settings.clone(),
            );
            Monitor::start_with_tab_bar(publisher, settings, options)
        } else {
            Monitor::start(settings, options)
        };
        match started {
            Ok(monitor) => self.monitor = Some(monitor),
            Err(error) => eprintln!("bclean: failed to start monitor: {error}"),
        }
    }
}

impl RustPlugin for BcleanPlugin {
    type Contract = bmux_plugin_sdk::NoPluginContract;

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        let result = bmux_plugin_sdk::route_command!(context, {
            "status" => self.run_status(&context),
            "segment" => self.run_segment(&context),
            "ack-alerts" => self.run_ack(&context),
            "open" => run_open(&context),
        });
        // A dynamically loaded plugin's error message does not reach the host's outcome
        // capture (it lives in this library's copy of the SDK), so print it ourselves on the CLI.
        if let Err(error) = &result
            && context.invocation_source.is_cli()
        {
            eprintln!("{}", error.message);
        }
        result
    }

    fn activate(&mut self, context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        self.activate_monitor(&context);
        Ok(EXIT_OK)
    }

    fn deactivate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        if let Some(mut monitor) = self.monitor.take() {
            monitor.stop();
            // The tab bar keeps items until their owner clears them.
            monitor.shared().clear_tab_bar();
        }
        Ok(EXIT_OK)
    }
}

// The macro body declares its own `MANIFEST` static, so pass the literal here.
bmux_plugin_sdk::export_plugin!(BcleanPlugin, include_str!("../plugin.toml"));

/// Parses this invocation's settings and resolves the daemon endpoint.
fn invocation_config(
    context: &NativeCommandContext,
) -> Result<(Settings, Endpoint), PluginCommandError> {
    let settings = Settings::from_value(context.settings.clone()).map_err(|e| {
        PluginCommandError::new(
            EXIT_USAGE,
            format!("invalid [plugins.settings.\"bclean\"]: {e}"),
        )
    })?;
    let endpoint = daemon::resolve_endpoint(&settings).map_err(PluginCommandError::failed)?;
    Ok((settings, endpoint))
}

/// Prints `cli_text` when invoked from the CLI and records `summary` as the bmux status message
/// (attach keybinding invocations must not write to stdout).
fn report(context: &NativeCommandContext, cli_text: &str, summary: &str) {
    if context.invocation_source.is_cli() {
        println!("{cli_text}");
    }
    record_command_outcome_metadata(
        COMMAND_OUTCOME_STATUS_MESSAGE_KEY,
        serde_json::Value::String(summary.to_owned()),
    );
}

/// Opens a pane running the bclean TUI through the `bmux.tabs` typed service.
fn run_open(context: &NativeCommandContext) -> Result<i32, PluginCommandError> {
    let (settings, _endpoint) = invocation_config(context)?;
    // A dynamically loaded plugin links its own copy of `bmux_plugin`, whose service-location
    // map the host never fills, so typed calls would try to load the bundled `bmux.tabs`
    // provider from disk and fail. Route it through the host like bmux does for CLI processes;
    // this never overrides a provider that is local to this process (static bundling).
    bmux_plugin::global_service_locations().mark_remote(BMUX_TABS);
    let interface_id = tabs_commands::INTERFACE_ID;
    let interface = interface_id.as_str();
    let program = settings.open_program.clone();
    let args = settings.open_args.clone();
    let result = match settings.open_layout {
        OpenLayout::SplitVertical | OpenLayout::SplitHorizontal => {
            let direction = if settings.open_layout == OpenLayout::SplitVertical {
                PaneDirection::Vertical
            } else {
                PaneDirection::Horizontal
            };
            context.call_service::<LaunchPaneRequest, Result<PaneAck, PaneMutationError>>(
                TABS_WRITE,
                ServiceKind::Command,
                interface,
                tabs_commands::OP_LAUNCH_PANE.as_str(),
                &LaunchPaneRequest {
                    session: None,
                    target: None,
                    direction,
                    name: Some("bclean".to_owned()),
                    program,
                    args,
                },
            )
        }
        OpenLayout::Floating => context
            .call_service::<CreateFloatingPaneRequest, Result<PaneAck, PaneMutationError>>(
                TABS_WRITE,
                ServiceKind::Command,
                interface,
                tabs_commands::OP_CREATE_FLOATING_PANE.as_str(),
                &CreateFloatingPaneRequest {
                    session: None,
                    target: None,
                    x: None,
                    y: None,
                    w: None,
                    h: None,
                    z: None,
                    layer: None,
                    scope: None,
                    program: Some(program),
                    args,
                },
            ),
    };
    match result {
        Ok(Ok(_ack)) => {
            let message = format!("bclean: opened {}", settings.open_program);
            report(context, &message, &message);
            Ok(EXIT_OK)
        }
        Ok(Err(error)) => Err(PluginCommandError::failed(format!(
            "bclean: could not open pane: {error:?}"
        ))),
        Err(error) => Err(PluginCommandError::unavailable(format!(
            "bclean: bmux.tabs service unavailable: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_validates() {
        let manifest = bmux_plugin::PluginManifest::from_toml_str(PLUGIN_MANIFEST).unwrap();
        assert_eq!(manifest.id, "bclean");
        let names: Vec<&str> = manifest.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["status", "segment", "ack-alerts", "open"]);
        let declaration = manifest.to_declaration().unwrap();
        assert_eq!(declaration.id.as_str(), "bclean");
    }
}
