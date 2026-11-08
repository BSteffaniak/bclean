#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! IPC client for the bclean daemon.
//!
//! [`Client::connect`] connects to the daemon, spawning it detached when it is not running
//! (if autostart is enabled), and restarts it when its protocol or package version differs
//! from the client's.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bclean_protocol::{
    ClientFrame, ClientHello, Endpoint, Event, FrameReader, FrameWriter, PROTOCOL_VERSION,
    RemoteError, Request, Response, ServerFrame, ServerHello, TransportError,
};

/// Errors from the client.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Transport failure.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The daemon is not running and autostart is disabled.
    #[error("bclean daemon is not running at {0} (start it with `bclean daemon start`)")]
    NotRunning(Endpoint),
    /// Spawning the daemon failed.
    #[error("failed to spawn the bclean daemon: {0}")]
    Spawn(std::io::Error),
    /// The daemon did not become reachable in time.
    #[error("timed out waiting for the bclean daemon at {0}")]
    Timeout(Endpoint),
    /// The daemon closed the connection.
    #[error("the bclean daemon closed the connection")]
    Disconnected,
    /// The daemon sent an unexpected frame.
    #[error("unexpected frame from the bclean daemon: {0}")]
    Protocol(String),
    /// The daemon returned an error for a request.
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

/// How to reach (and if needed start) the daemon.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Daemon endpoint.
    pub endpoint: Endpoint,
    /// Spawn the daemon when it is not running.
    pub autostart: bool,
    /// Executable to spawn (defaults to the current executable).
    pub daemon_exe: Option<PathBuf>,
    /// Arguments that make the executable run the daemon in the foreground.
    pub daemon_args: Vec<OsString>,
    /// File receiving the spawned daemon's stdout/stderr.
    pub daemon_log: Option<PathBuf>,
    /// How long to wait for a spawned daemon to start listening.
    pub start_timeout: Duration,
    /// Version string sent in the handshake and compared with the daemon's.
    pub client_version: String,
    /// Restart the daemon when its version differs from `client_version`.
    pub restart_on_version_mismatch: bool,
    /// This client's executable fingerprint ([`bclean_protocol::executable_build_id`]). When
    /// set, a daemon reporting a different (or no) fingerprint is restarted like a version
    /// mismatch: it runs older code than this client. Leave `None` in clients that are not the
    /// daemon's own executable (plugins).
    pub build: Option<String>,
}

impl ClientOptions {
    /// Options for `endpoint` with autostart enabled and `bclean daemon run` as the command.
    #[must_use]
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            autostart: true,
            daemon_exe: None,
            daemon_args: vec!["daemon".into(), "run".into()],
            daemon_log: None,
            start_timeout: Duration::from_secs(30),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            restart_on_version_mismatch: true,
            build: None,
        }
    }
}

/// A connected client.
pub struct Client {
    reader: FrameReader,
    writer: FrameWriter,
    server: ServerHello,
    next_id: u64,
    pending_events: VecDeque<Event>,
}

impl Client {
    /// Connects to the daemon, spawning or restarting it as configured.
    ///
    /// # Errors
    ///
    /// * [`ClientError::NotRunning`] if the daemon is down and autostart is disabled
    /// * [`ClientError::Spawn`] / [`ClientError::Timeout`] if starting the daemon fails
    /// * [`ClientError::Transport`] / [`ClientError::Protocol`] on handshake failures
    pub async fn connect(options: &ClientOptions) -> Result<Self, ClientError> {
        let client = match Self::try_connect(options).await? {
            Some(client) => client,
            None if options.autostart => {
                spawn_daemon(options)?;
                Self::wait_for_daemon(options).await?
            }
            None => return Err(ClientError::NotRunning(options.endpoint.clone())),
        };

        if !needs_restart(&client.server, options) || !options.autostart {
            return Ok(client);
        }
        if outdated_client(&client.server, options) {
            log::info!(
                "keeping the bclean daemon: it runs the installed build, and this client is older"
            );
            return Ok(client);
        }
        log::info!(
            "restarting bclean daemon (daemon {} / protocol {} / build {}, client {} / protocol {} / build {})",
            client.server.daemon_version,
            client.server.protocol_version,
            client.server.build.as_deref().unwrap_or("-"),
            options.client_version,
            PROTOCOL_VERSION,
            options.build.as_deref().unwrap_or("-")
        );
        client.shutdown_daemon(options).await?;
        spawn_daemon(options)?;
        Self::wait_for_daemon(options).await
    }

    /// Connects only if a daemon is already listening.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Transport`] / [`ClientError::Protocol`] on handshake failures
    pub async fn try_connect(options: &ClientOptions) -> Result<Option<Self>, ClientError> {
        let Ok((reader, writer)) = bclean_protocol::connect(&options.endpoint).await else {
            return Ok(None);
        };
        Self::handshake(
            reader,
            writer,
            &options.client_version,
            options.build.clone(),
        )
        .await
        .map(Some)
    }

    async fn handshake(
        mut reader: FrameReader,
        mut writer: FrameWriter,
        client_version: &str,
        build: Option<String>,
    ) -> Result<Self, ClientError> {
        writer
            .write(&ClientFrame::Hello(ClientHello {
                protocol_version: PROTOCOL_VERSION,
                client_version: client_version.to_owned(),
                build,
            }))
            .await?;
        match reader.read::<ServerFrame>().await? {
            Some(ServerFrame::Hello(server)) => Ok(Self {
                reader,
                writer,
                server,
                next_id: 1,
                pending_events: VecDeque::new(),
            }),
            Some(other) => Err(ClientError::Protocol(format!("{other:?}"))),
            None => Err(ClientError::Disconnected),
        }
    }

    async fn wait_for_daemon(options: &ClientOptions) -> Result<Self, ClientError> {
        let deadline = Instant::now() + options.start_timeout;
        loop {
            if let Some(client) = Self::try_connect(options).await? {
                return Ok(client);
            }
            if Instant::now() >= deadline {
                return Err(ClientError::Timeout(options.endpoint.clone()));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn shutdown_daemon(mut self, options: &ClientOptions) -> Result<(), ClientError> {
        // The daemon may exit before replying; either outcome is fine.
        let _ = self.request(Request::Shutdown).await;
        drop(self);
        let deadline = Instant::now() + options.start_timeout;
        while Instant::now() < deadline {
            if bclean_protocol::connect(&options.endpoint).await.is_err() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(ClientError::Timeout(options.endpoint.clone()))
    }

    /// The daemon's handshake.
    #[must_use]
    pub const fn server(&self) -> &ServerHello {
        &self.server
    }

    /// Sends a request and waits for its response, buffering events that arrive meanwhile.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Remote`] if the daemon reports an error
    /// * [`ClientError::Transport`] / [`ClientError::Disconnected`] on connection failures
    pub async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        self.writer
            .write(&ClientFrame::Request { id, request })
            .await?;
        loop {
            match self.reader.read::<ServerFrame>().await? {
                Some(ServerFrame::Response { id: got, result }) if got == id => {
                    return result.map_err(ClientError::Remote);
                }
                Some(ServerFrame::Event(event)) => self.pending_events.push_back(event),
                Some(other) => return Err(ClientError::Protocol(format!("{other:?}"))),
                None => return Err(ClientError::Disconnected),
            }
        }
    }

    /// Waits for the next pushed event (after subscribing with [`Request::Subscribe`]).
    ///
    /// # Errors
    ///
    /// * [`ClientError::Transport`] on connection failures
    /// * [`ClientError::Protocol`] if a stray response arrives
    pub async fn next_event(&mut self) -> Result<Option<Event>, ClientError> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        match self.reader.read::<ServerFrame>().await? {
            Some(ServerFrame::Event(event)) => Ok(Some(event)),
            Some(other) => Err(ClientError::Protocol(format!("{other:?}"))),
            None => Ok(None),
        }
    }
}

/// Whether a daemon that answered with `server` runs different code than this client: another
/// protocol, package version, or (when the client sets one) executable build.
fn needs_restart(server: &ServerHello, options: &ClientOptions) -> bool {
    server.protocol_version != PROTOCOL_VERSION
        || (options.restart_on_version_mismatch
            && (server.daemon_version != options.client_version
                || (options.build.is_some() && server.build != options.build)))
}

/// Whether only the build differs, because this client predates a rebuild that the daemon
/// already runs: the client (a long-lived TUI, say) must not replace the daemon with the same
/// executable it is already running.
fn outdated_client(server: &ServerHello, options: &ClientOptions) -> bool {
    server.protocol_version == PROTOCOL_VERSION
        && server.daemon_version == options.client_version
        && server.build.is_some()
        && server.build != options.build
        && server.build == bclean_protocol::installed_build_id()
}

/// Spawns the daemon fully detached from the current terminal/session.
///
/// # Errors
///
/// * If the executable cannot be resolved or spawned, or the log file cannot be opened
pub fn spawn_daemon(options: &ClientOptions) -> Result<(), ClientError> {
    let exe = match &options.daemon_exe {
        Some(exe) => exe.clone(),
        None => std::env::current_exe().map_err(ClientError::Spawn)?,
    };
    let mut command = Command::new(exe);
    command.args(&options.daemon_args).stdin(Stdio::null());
    if let Some(log) = &options.daemon_log {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent).map_err(ClientError::Spawn)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(ClientError::Spawn)?;
        let err = file.try_clone().map_err(ClientError::Spawn)?;
        command.stdout(file).stderr(err);
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    detach(&mut command);
    // The child is intentionally not waited on: it outlives this process.
    command.spawn().map(drop).map_err(ClientError::Spawn)
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // New process group so terminal signals (Ctrl-C) aimed at the client don't reach it.
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
const fn detach(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_protocol::DaemonStatus;

    #[test]
    fn restarts_daemons_running_a_different_build() {
        let hello = |version: &str, build: Option<&str>| ServerHello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: version.to_owned(),
            pid: 1,
            build: build.map(str::to_owned),
        };
        let mut options = ClientOptions::new(endpoint("restart"));
        "1.0".clone_into(&mut options.client_version);
        // Clients without a fingerprint (plugins) only compare versions.
        assert!(!needs_restart(&hello("1.0", Some("a")), &options));
        assert!(needs_restart(&hello("0.9", None), &options));
        // The CLI also restarts a daemon built from other code, or too old to report a build.
        options.build = Some("a".to_owned());
        assert!(!needs_restart(&hello("1.0", Some("a")), &options));
        assert!(needs_restart(&hello("1.0", Some("b")), &options));
        assert!(needs_restart(&hello("1.0", None), &options));
        options.restart_on_version_mismatch = false;
        assert!(!needs_restart(&hello("1.0", Some("b")), &options));
    }

    fn endpoint(tag: &str) -> Endpoint {
        if cfg!(windows) {
            Endpoint::Namespaced(format!("bclean-client-test-{tag}-{}", std::process::id()))
        } else {
            Endpoint::Path(
                std::env::temp_dir()
                    .join(format!("bclean-client-{tag}-{}", std::process::id()))
                    .join("d.sock"),
            )
        }
    }

    fn fake_daemon(endpoint: &Endpoint, version: &'static str) {
        let listener = bclean_protocol::listen(endpoint).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut reader, mut writer) = bclean_protocol::accept(&listener).await.unwrap();
                tokio::spawn(async move {
                    while let Ok(Some(frame)) = reader.read::<ClientFrame>().await {
                        let reply = match frame {
                            ClientFrame::Hello(_) => ServerFrame::Hello(ServerHello {
                                protocol_version: PROTOCOL_VERSION,
                                daemon_version: version.to_owned(),
                                pid: 1,
                                build: None,
                            }),
                            ClientFrame::Request { id, .. } => {
                                writer
                                    .write(&ServerFrame::Event(Event::Volumes {
                                        volumes: Vec::new(),
                                    }))
                                    .await
                                    .unwrap();
                                ServerFrame::Response {
                                    id,
                                    result: Ok(Response::Status(DaemonStatus {
                                        pid: 1,
                                        version: version.to_owned(),
                                        started_at: 0,
                                        roots: Vec::new(),
                                        policies: 0,
                                        active_alerts: 0,
                                        watchers: Vec::new(),
                                    })),
                                }
                            }
                        };
                        writer.write(&reply).await.unwrap();
                    }
                });
            }
        });
    }

    #[tokio::test]
    async fn connects_requests_and_buffers_events() {
        let endpoint = endpoint("ok");
        fake_daemon(&endpoint, env!("CARGO_PKG_VERSION"));
        let mut options = ClientOptions::new(endpoint);
        options.autostart = false;
        let mut client = Client::connect(&options).await.unwrap();
        assert_eq!(client.server().pid, 1);
        let response = client.request(Request::Status).await.unwrap();
        assert!(matches!(response, Response::Status(_)));
        assert!(matches!(
            client.next_event().await.unwrap(),
            Some(Event::Volumes { .. })
        ));
    }

    #[tokio::test]
    async fn reports_not_running_without_autostart() {
        let mut options = ClientOptions::new(endpoint("missing"));
        options.autostart = false;
        assert!(matches!(
            Client::connect(&options).await,
            Err(ClientError::NotRunning(_))
        ));
    }
}
