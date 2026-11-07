//! Talking to the bclean daemon: endpoint resolution, connection options, and one-shot queries.

use std::path::PathBuf;
use std::time::Duration;

use bclean_client::{Client, ClientError, ClientOptions};
use bclean_config::{Expander, LoadOptions};
use bclean_model::AlertEvent;
use bclean_protocol::{Endpoint, PROTOCOL_VERSION, Request, Response};

use crate::settings::Settings;
use crate::status::{Connection, Reclaimable, Snapshot};

/// Resolves the daemon endpoint: the plugin `socket` setting, else bclean's own
/// `daemon.socket`, else the platform default from [`bclean_config::AppDirs`].
///
/// # Errors
///
/// * If the override cannot be expanded (`~`/`$VAR` with no value)
/// * If the home directory needed for the default location is unknown
pub fn resolve_endpoint(settings: &Settings) -> Result<Endpoint, String> {
    let expander = Expander::from_process();
    let configured = settings
        .socket_override()
        .map(str::to_owned)
        .or_else(|| configured_socket(&expander));
    if let Some(socket) = configured {
        let socket = expander.expand(&socket).map_err(|e| e.to_string())?;
        return Ok(Endpoint::from_config(&socket));
    }
    let dirs = expander.app_dirs().map_err(|e| e.to_string())?;
    Ok(Endpoint::from_config(&dirs.socket_path().to_string_lossy()))
}

/// bclean's global `daemon.socket`, if configured. The catalog and project layers are skipped:
/// they cannot set the socket meaningfully and the bmux server's working directory is arbitrary.
fn configured_socket(expander: &Expander) -> Option<String> {
    let cwd = expander
        .home()
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let mut options = LoadOptions::new(cwd, expander.clone());
    options.catalog = false;
    options.project = false;
    bclean_config::load(&options)
        .ok()?
        .config
        .daemon
        .socket
        .filter(|s| !s.trim().is_empty())
}

/// Client options for the plugin: never restart a daemon because of a version difference, and
/// autostart (when enabled) runs `<daemon_exe> daemon run` rather than the bmux executable.
#[must_use]
pub fn client_options(endpoint: Endpoint, settings: &Settings, autostart: bool) -> ClientOptions {
    let mut options = ClientOptions::new(endpoint);
    options.autostart = autostart;
    options.daemon_exe = Some(PathBuf::from(&settings.daemon_exe));
    options.restart_on_version_mismatch = false;
    options.start_timeout = Duration::from_secs(10);
    options
}

/// Connects and verifies the protocol version.
///
/// # Errors
///
/// * Any [`ClientError`] from connecting (including [`ClientError::NotRunning`])
/// * [`ClientError::Protocol`] if the daemon speaks another protocol version
pub async fn connect(options: &ClientOptions) -> Result<Client, ClientError> {
    let client = Client::connect(options).await?;
    let server = client.server();
    if server.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::Protocol(format!(
            "daemon protocol {} differs from plugin protocol {PROTOCOL_VERSION}",
            server.protocol_version
        )));
    }
    Ok(client)
}

fn unexpected(response: &Response) -> ClientError {
    ClientError::Protocol(format!("unexpected response: {response:?}"))
}

/// Active alerts.
///
/// # Errors
///
/// * Any request failure, or an unexpected response kind
pub async fn alerts(client: &mut Client) -> Result<Vec<AlertEvent>, ClientError> {
    match client.request(Request::Alerts).await? {
        Response::Alerts { alerts } => Ok(alerts),
        other => Err(unexpected(&other)),
    }
}

/// Fetches status, volumes, and alerts into a connected snapshot.
///
/// # Errors
///
/// * Any request failure, or an unexpected response kind
pub async fn fetch(client: &mut Client) -> Result<Snapshot, ClientError> {
    let daemon = match client.request(Request::Status).await? {
        Response::Status(status) => status,
        other => return Err(unexpected(&other)),
    };
    let volumes = match client.request(Request::Volumes).await? {
        Response::Volumes { volumes } => volumes,
        other => return Err(unexpected(&other)),
    };
    let alerts = alerts(client).await?;
    // Older daemons don't know the request; the segment simply omits the amount.
    let reclaimable = match client
        .request(Request::Dashboard { roots: Vec::new() })
        .await
    {
        Ok(Response::Dashboard(summary)) => Some(Reclaimable {
            total: summary.reclaimable.total,
            safe: summary.reclaimable.safe,
        }),
        _ => None,
    };
    Ok(Snapshot {
        connection: Connection::Connected,
        daemon: Some(daemon),
        volumes,
        alerts,
        reclaimable,
        updated_at: Some(now()),
    })
}

/// Acknowledges every active alert; returns how many were acknowledged.
///
/// # Errors
///
/// * Any request failure, or an unexpected response kind
pub async fn ack_all(client: &mut Client) -> Result<usize, ClientError> {
    let active = alerts(client).await?;
    for alert in &active {
        match client
            .request(Request::AckAlert {
                key: alert.key.clone(),
            })
            .await?
        {
            Response::Ok => {}
            other => return Err(unexpected(&other)),
        }
    }
    Ok(active.len())
}

/// Runs `future` on a fresh current-thread runtime (for synchronous plugin command handlers).
///
/// # Errors
///
/// * If the runtime cannot be created
pub fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|runtime| runtime.block_on(future))
        .map_err(|e| format!("failed to start async runtime: {e}"))
}

/// One-shot snapshot for commands that run without the background monitor.
#[must_use]
pub fn one_shot_snapshot(options: &ClientOptions) -> Snapshot {
    let result = block_on(async {
        let mut client = connect(options).await?;
        fetch(&mut client).await
    });
    match result {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(error)) => offline(&error),
        Err(error) => Snapshot {
            connection: Connection::Offline(error),
            ..Snapshot::default()
        },
    }
}

/// An offline snapshot describing `error`.
#[must_use]
pub fn offline(error: &ClientError) -> Snapshot {
    Snapshot {
        connection: Connection::Offline(describe(error)),
        ..Snapshot::default()
    }
}

/// Short human description of a client error.
#[must_use]
pub fn describe(error: &ClientError) -> String {
    match error {
        ClientError::NotRunning(_) => "daemon not running".to_owned(),
        other => other.to_string(),
    }
}

/// Current Unix time in seconds.
#[must_use]
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_override_wins() {
        let settings = Settings {
            socket: Some("/tmp/custom/bclean.sock".to_owned()),
            ..Settings::default()
        };
        assert_eq!(
            resolve_endpoint(&settings).unwrap(),
            Endpoint::Path(PathBuf::from("/tmp/custom/bclean.sock"))
        );
        let named = Settings {
            socket: Some("bclean-test".to_owned()),
            ..Settings::default()
        };
        assert_eq!(
            resolve_endpoint(&named).unwrap(),
            Endpoint::Namespaced("bclean-test".to_owned())
        );
    }

    #[test]
    fn options_never_restart_and_use_bclean_exe() {
        let options = client_options(
            Endpoint::Namespaced("x".to_owned()),
            &Settings::default(),
            false,
        );
        assert!(!options.autostart);
        assert!(!options.restart_on_version_mismatch);
        assert_eq!(options.daemon_exe, Some(PathBuf::from("bclean")));
        assert_eq!(options.daemon_args, ["daemon", "run"]);
    }

    #[test]
    fn one_shot_reports_offline_daemon() {
        let endpoint = if cfg!(windows) {
            Endpoint::Namespaced(format!("bclean-plugin-missing-{}", std::process::id()))
        } else {
            Endpoint::Path(
                std::env::temp_dir()
                    .join(format!("bclean-plugin-missing-{}", std::process::id()))
                    .join("d.sock"),
            )
        };
        let snapshot = one_shot_snapshot(&client_options(endpoint, &Settings::default(), false));
        assert_eq!(
            snapshot.connection,
            Connection::Offline("daemon not running".to_owned())
        );
    }
}
