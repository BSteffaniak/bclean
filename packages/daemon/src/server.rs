//! Generic IPC server: accepts connections, performs the handshake, dispatches requests to a
//! [`Handler`], and forwards subscribed events.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use bclean_protocol::{
    ClientFrame, Endpoint, Event, FrameReader, FrameWriter, PROTOCOL_VERSION, RemoteError, Request,
    Response, ServerFrame, ServerHello, Topic,
};
use tokio::sync::{broadcast, watch};

/// Handles daemon requests. Implemented by the daemon engine.
pub trait Handler: Send + Sync + 'static {
    /// Handles one request.
    fn handle(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response, RemoteError>> + Send;
}

/// Topic an event belongs to.
#[must_use]
pub const fn topic_of(event: &Event) -> Topic {
    match event {
        Event::ScanProgress { .. } | Event::TreeUpdated(_) => Topic::Scans,
        Event::Volumes { .. } => Topic::Volumes,
        Event::Alert(_) => Topic::Alerts,
        Event::ActionProgress { .. } => Topic::Actions,
    }
}

/// Server state shared with connection tasks.
pub struct Server<H> {
    handler: Arc<H>,
    events: broadcast::Sender<Event>,
    shutdown: watch::Sender<bool>,
    version: String,
    build: Option<String>,
}

impl<H: Handler> Server<H> {
    /// Creates a server around `handler`.
    #[must_use]
    pub fn new(handler: Arc<H>, version: impl Into<String>) -> Self {
        let (events, _) = broadcast::channel(1024);
        let (shutdown, _) = watch::channel(false);
        Self {
            handler,
            events,
            shutdown,
            version: version.into(),
            build: None,
        }
    }

    /// Reports `build` (an executable fingerprint) in the handshake, so clients from a
    /// different build of the same version restart this daemon.
    #[must_use]
    pub fn with_build(mut self, build: Option<String>) -> Self {
        self.build = build;
        self
    }

    /// Sender used by the engine to publish events to subscribers.
    #[must_use]
    pub fn events(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }

    /// Sender that stops the server when set to `true`.
    #[must_use]
    pub fn shutdown_handle(&self) -> watch::Sender<bool> {
        self.shutdown.clone()
    }

    /// Listens on `endpoint` until shutdown is requested (by a `SHUTDOWN` request, the
    /// shutdown handle, or Ctrl-C / SIGTERM).
    ///
    /// # Errors
    ///
    /// * If the endpoint cannot be bound
    pub async fn serve(self: Arc<Self>, endpoint: &Endpoint) -> std::io::Result<()> {
        let listener = bclean_protocol::listen(endpoint)?;
        log::info!("bclean daemon listening on {endpoint}");
        let mut shutdown = self.shutdown.subscribe();
        loop {
            tokio::select! {
                accepted = bclean_protocol::accept(&listener) => match accepted {
                    Ok((reader, writer)) => {
                        let server = Arc::clone(&self);
                        tokio::spawn(async move { server.connection(reader, writer).await });
                    }
                    Err(error) => log::warn!("failed to accept ipc connection: {error}"),
                },
                _ = shutdown.changed() => break,
                () = termination_signal() => break,
            }
        }
        log::info!("bclean daemon shutting down");
        drop(listener);
        if let Endpoint::Path(path) = endpoint {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    async fn connection(self: Arc<Self>, mut reader: FrameReader, writer: FrameWriter) {
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        match reader.read::<ClientFrame>().await {
            Ok(Some(ClientFrame::Hello(hello))) => {
                log::debug!(
                    "client connected (protocol {}, {})",
                    hello.protocol_version,
                    hello.client_version
                );
                let reply = ServerFrame::Hello(ServerHello {
                    protocol_version: PROTOCOL_VERSION,
                    daemon_version: self.version.clone(),
                    pid: std::process::id(),
                    build: self.build.clone(),
                });
                if writer.lock().await.write(&reply).await.is_err() {
                    return;
                }
            }
            Ok(_) | Err(_) => return,
        }

        let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
        let mut topics: BTreeSet<Topic> = BTreeSet::new();
        while let Ok(Some(value)) = reader.read::<serde_json::Value>().await {
            let frame = match serde_json::from_value::<ClientFrame>(value.clone()) {
                Ok(frame) => frame,
                Err(e) => {
                    // A newer client sent a request this daemon doesn't know: say so instead
                    // of dropping the connection.
                    let Some(id) = value.get("id").and_then(serde_json::Value::as_u64) else {
                        break;
                    };
                    log::warn!("unsupported request from client: {e}");
                    let reply = ServerFrame::Response {
                        id,
                        result: Err(RemoteError::new(
                            "UNSUPPORTED_REQUEST",
                            format!(
                                "this bclean daemon ({}) is older than the client and does not \
                                 support the request; restart it with `bclean daemon restart`",
                                self.version
                            ),
                        )),
                    };
                    if writer.lock().await.write(&reply).await.is_err() {
                        break;
                    }
                    continue;
                }
            };
            let ClientFrame::Request { id, request } = frame else {
                break;
            };
            let result = match request {
                Request::Subscribe { topics: new } => {
                    topics.extend(new);
                    if let Some(task) = forwarder.take() {
                        task.abort();
                    }
                    forwarder = Some(self.forward_events(topics.clone(), Arc::clone(&writer)));
                    Ok(Response::Ok)
                }
                Request::Shutdown => {
                    let _ = writer
                        .lock()
                        .await
                        .write(&ServerFrame::Response {
                            id,
                            result: Ok(Response::Ok),
                        })
                        .await;
                    let _ = self.shutdown.send(true);
                    break;
                }
                other => self.handler.handle(other).await,
            };
            if writer
                .lock()
                .await
                .write(&ServerFrame::Response { id, result })
                .await
                .is_err()
            {
                break;
            }
        }
        if let Some(task) = forwarder {
            task.abort();
        }
    }

    fn forward_events(
        &self,
        topics: BTreeSet<Topic>,
        writer: Arc<tokio::sync::Mutex<FrameWriter>>,
    ) -> tokio::task::JoinHandle<()> {
        let mut events = self.events.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) if topics.contains(&topic_of(&event)) => {
                        if writer
                            .lock()
                            .await
                            .write(&ServerFrame::Event(event))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

#[cfg(unix)]
async fn termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(not(unix))]
async fn termination_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bclean_client::{Client, ClientOptions};

    struct Echo;

    impl Handler for Echo {
        fn handle(
            &self,
            request: Request,
        ) -> impl Future<Output = Result<Response, RemoteError>> + Send {
            std::future::ready(match request {
                Request::Volumes => Ok(Response::Volumes {
                    volumes: Vec::new(),
                }),
                _ => Err(RemoteError::new("UNSUPPORTED", "nope")),
            })
        }
    }

    #[tokio::test]
    async fn serves_requests_events_and_shutdown() {
        let endpoint = if cfg!(windows) {
            Endpoint::Namespaced(format!("bclean-daemon-test-{}", std::process::id()))
        } else {
            Endpoint::Path(
                std::env::temp_dir()
                    .join(format!("bclean-daemon-{}", std::process::id()))
                    .join("d.sock"),
            )
        };
        let server = Arc::new(
            Server::new(Arc::new(Echo), env!("CARGO_PKG_VERSION")).with_build(Some("b1".into())),
        );
        let events = server.events();
        let serving = tokio::spawn({
            let server = Arc::clone(&server);
            let endpoint = endpoint.clone();
            async move { server.serve(&endpoint).await }
        });

        let mut options = ClientOptions::new(endpoint);
        options.autostart = false;
        let mut client = loop {
            if let Ok(client) = Client::connect(&options).await {
                break client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert!(matches!(
            client.request(Request::Volumes).await,
            Ok(Response::Volumes { .. })
        ));
        assert!(client.request(Request::Status).await.is_err());
        assert_eq!(client.server().build.as_deref(), Some("b1"));

        // A request this daemon doesn't know (from a newer client) gets an error reply, and the
        // connection keeps working.
        let (mut reader, mut writer) = bclean_protocol::connect(&options.endpoint).await.unwrap();
        writer
            .write(&ClientFrame::Hello(bclean_protocol::ClientHello {
                protocol_version: PROTOCOL_VERSION,
                client_version: "future".into(),
                build: None,
            }))
            .await
            .unwrap();
        assert!(matches!(
            reader.read::<ServerFrame>().await.unwrap(),
            Some(ServerFrame::Hello(_))
        ));
        writer
            .write(&serde_json::json!({
                "type": "REQUEST",
                "id": 7,
                "request": { "kind": "FROM_THE_FUTURE" },
            }))
            .await
            .unwrap();
        let Some(ServerFrame::Response {
            id: 7,
            result: Err(error),
        }) = reader.read::<ServerFrame>().await.unwrap()
        else {
            panic!("expected an error reply");
        };
        assert_eq!(error.code, "UNSUPPORTED_REQUEST");
        assert!(error.message.contains("bclean daemon restart"), "{error}");
        writer
            .write(&ClientFrame::Request {
                id: 8,
                request: Request::Volumes,
            })
            .await
            .unwrap();
        assert!(matches!(
            reader.read::<ServerFrame>().await.unwrap(),
            Some(ServerFrame::Response {
                id: 8,
                result: Ok(_)
            })
        ));

        client
            .request(Request::Subscribe {
                topics: vec![Topic::Volumes],
            })
            .await
            .unwrap();
        events
            .send(Event::ScanProgress {
                root: "/".into(),
                progress: bclean_protocol::ScanProgressInfo::default(),
            })
            .unwrap();
        events
            .send(Event::Volumes {
                volumes: Vec::new(),
            })
            .unwrap();
        assert!(matches!(
            client.next_event().await.unwrap(),
            Some(Event::Volumes { .. })
        ));

        client.request(Request::Shutdown).await.unwrap();
        serving.await.unwrap().unwrap();
    }
}
