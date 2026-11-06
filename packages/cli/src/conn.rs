//! A connection to "the engine": the daemon over IPC, or an in-process engine with
//! `--no-daemon`. Commands are written once against [`Conn::request`].

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use bclean_client::{Client, ClientOptions};
use bclean_config::AppDirs;
use bclean_engine::{Engine, EngineOptions, Settings};
use bclean_protocol::{Endpoint, Request, Response, SnapshotRef};
use bclean_tree::Tree;

/// Package version (sent in the daemon handshake).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Process-wide context derived from the global flags.
#[derive(Debug)]
pub struct Ctx {
    /// Engine construction options (config layers).
    pub options: EngineOptions,
    /// Loaded settings (config + rules).
    pub settings: Arc<Settings>,
    /// Application directories.
    pub dirs: AppDirs,
    /// Explicit `--config` file.
    pub config_file: Option<PathBuf>,
    /// `--no-daemon`.
    pub no_daemon: bool,
}

impl Ctx {
    /// Loads configuration for the current process.
    ///
    /// # Errors
    ///
    /// * If configuration cannot be loaded or rules don't compile
    pub fn load(config_file: Option<PathBuf>, no_daemon: bool) -> anyhow::Result<Self> {
        let options = EngineOptions::from_process(config_file.clone())?;
        let settings = Settings::load(&options.load)?;
        let dirs = options.load.expander.app_dirs()?;
        Ok(Self {
            options,
            settings: Arc::new(settings),
            dirs,
            config_file,
            no_daemon,
        })
    }

    /// Daemon log file (`<data>/daemon.log`).
    #[must_use]
    pub fn log_path(&self) -> PathBuf {
        self.dirs.data.join("daemon.log")
    }

    /// Daemon endpoint.
    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        bclean_daemon::endpoint_for(&self.settings, &self.dirs.runtime)
    }

    /// Client options (autostart per `daemon.autostart`, log redirected to the data dir).
    #[must_use]
    pub fn client_options(&self) -> ClientOptions {
        let mut options = ClientOptions::new(self.endpoint());
        options.autostart = self.settings.config().daemon.autostart;
        options.daemon_log = Some(self.log_path());
        if let Some(config) = &self.config_file
            && let Ok(abs) = std::path::absolute(config)
        {
            options.daemon_args = vec![
                "--config".into(),
                abs.into_os_string(),
                "daemon".into(),
                "run".into(),
            ];
        }
        VERSION.clone_into(&mut options.client_version);
        options
    }

    /// Arguments that run the daemon in the foreground (for service units).
    #[must_use]
    pub fn daemon_args(&self) -> Vec<OsString> {
        self.client_options().daemon_args
    }

    /// Opens the in-process engine.
    ///
    /// # Errors
    ///
    /// * If the engine cannot be opened
    pub async fn engine(&self) -> anyhow::Result<Engine> {
        Ok(Engine::open(self.options.clone()).await?)
    }

    /// Connects according to `--no-daemon`.
    ///
    /// # Errors
    ///
    /// * If the engine cannot be opened or the daemon cannot be reached/started
    pub async fn connect(&self) -> anyhow::Result<Conn> {
        if self.no_daemon {
            Ok(Conn::Local(self.engine().await?))
        } else {
            let client = Client::connect(&self.client_options())
                .await
                .with_context(|| format!("connecting to the daemon at {}", self.endpoint()))?;
            Ok(Conn::Daemon(Box::new(client)))
        }
    }
}

/// An engine connection.
pub enum Conn {
    /// In-process engine.
    Local(Engine),
    /// Daemon client.
    Daemon(Box<Client>),
}

impl Conn {
    /// Whether this is the in-process engine.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Sends one request.
    ///
    /// # Errors
    ///
    /// * The engine/daemon error for the request
    pub async fn request(&mut self, request: Request) -> anyhow::Result<Response> {
        match self {
            Self::Local(engine) => bclean_daemon::handle(engine, VERSION, request)
                .await
                .map_err(|e| anyhow!(e)),
            Self::Daemon(client) => Ok(client.request(request).await?),
        }
    }

    /// Scans (or reuses) `path`. Local mode always scans fresh.
    ///
    /// # Errors
    ///
    /// * If the scan fails
    pub async fn scan(&mut self, path: &Path, force: bool) -> anyhow::Result<SnapshotRef> {
        let force = force || self.is_local();
        match self
            .request(Request::Scan {
                root: path.to_path_buf(),
                force,
                wait: true,
            })
            .await?
        {
            Response::Snapshot(snapshot) => Ok(snapshot),
            other => Err(unexpected(&other)),
        }
    }

    /// Loads the tree a snapshot refers to (from memory locally, from disk otherwise).
    ///
    /// # Errors
    ///
    /// * If the snapshot cannot be read
    pub fn load_tree(&self, snapshot: &SnapshotRef) -> anyhow::Result<Arc<Tree>> {
        if let Self::Local(engine) = self
            && let Some(tree) = engine.tree(&snapshot.root)
        {
            return Ok(tree);
        }
        Ok(Arc::new(
            bclean_index::snapshot::read(&snapshot.path, Some(&snapshot.root))
                .with_context(|| format!("reading snapshot {}", snapshot.path.display()))?
                .tree,
        ))
    }
}

/// Error for an unexpected response kind.
#[must_use]
pub fn unexpected(response: &Response) -> anyhow::Error {
    anyhow!("unexpected response from the engine: {response:?}")
}
