//! `bclean daemon ...`, `bclean service ...`, and `bclean watch`.

use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use bclean_client::Client;
use bclean_daemon::service::{self, ServiceSpec};
use bclean_protocol::{Event, Request, Response};

use crate::conn::{Ctx, VERSION, unexpected};
use crate::output::size;

/// `bclean daemon run`: serves until shut down.
///
/// # Errors
///
/// * If another daemon is already listening or the endpoint cannot be bound
pub async fn run(ctx: &Ctx) -> anyhow::Result<()> {
    let mut options = ctx.client_options();
    options.autostart = false;
    if let Ok(Some(client)) = Client::try_connect(&options).await {
        bail!(
            "a bclean daemon (pid {}) is already listening at {}",
            client.server().pid,
            ctx.endpoint()
        );
    }
    let engine = ctx.engine().await?;
    log::info!(
        "bclean daemon {VERSION} starting (pid {}, data {})",
        std::process::id(),
        engine.data_dir().display()
    );
    bclean_daemon::run_daemon(engine, ctx.endpoint(), VERSION)
        .await
        .context("running the daemon")?;
    log::info!("bclean daemon stopped");
    Ok(())
}

/// `bclean daemon start`.
///
/// # Errors
///
/// * If the daemon cannot be started
pub async fn start(ctx: &Ctx) -> anyhow::Result<()> {
    let mut options = ctx.daemon_options();
    options.autostart = true;
    let client = Client::connect(&options).await?;
    println!(
        "bclean daemon running (pid {}, version {}) at {}",
        client.server().pid,
        client.server().daemon_version,
        ctx.endpoint()
    );
    Ok(())
}

/// `bclean daemon stop`. Returns whether a daemon was running.
///
/// # Errors
///
/// * If the daemon does not exit in time
pub async fn stop(ctx: &Ctx) -> anyhow::Result<bool> {
    let mut options = ctx.client_options();
    options.autostart = false;
    let Some(mut client) = Client::try_connect(&options).await? else {
        println!("bclean daemon is not running");
        return Ok(false);
    };
    let pid = client.server().pid;
    let _ = client.request(Request::Shutdown).await;
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if Client::try_connect(&options).await.ok().flatten().is_none() {
            println!("bclean daemon (pid {pid}) stopped");
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("bclean daemon (pid {pid}) did not stop within 10s")
}

/// `bclean daemon status`. Returns whether the daemon is running.
///
/// # Errors
///
/// * On protocol errors
pub async fn status(ctx: &Ctx) -> anyhow::Result<bool> {
    let mut options = ctx.client_options();
    options.autostart = false;
    let Some(mut client) = Client::try_connect(&options).await? else {
        println!("bclean daemon is not running ({})", ctx.endpoint());
        return Ok(false);
    };
    match client.request(Request::Status).await? {
        Response::Status(status) => {
            println!(
                "bclean daemon running: pid {}, version {}, {} root(s), endpoint {}",
                status.pid,
                status.version,
                status.roots.len(),
                ctx.endpoint()
            );
            for root in &status.roots {
                println!(
                    "  {}  {}  gen {}{}",
                    root.root.display(),
                    size(root.total_allocated),
                    root.generation,
                    if root.scanning { "  (scanning)" } else { "" }
                );
            }
            Ok(true)
        }
        other => Err(unexpected(&other)),
    }
}

/// `bclean daemon logs`.
///
/// # Errors
///
/// * If the log cannot be read
pub fn logs(ctx: &Ctx, tail: Option<usize>) -> anyhow::Result<()> {
    let path = ctx.log_path();
    let Some(n) = tail else {
        println!("{}", path.display());
        return Ok(());
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    for line in &lines[lines.len().saturating_sub(n)..] {
        println!("{line}");
    }
    Ok(())
}

fn spec(ctx: &Ctx) -> anyhow::Result<ServiceSpec> {
    Ok(ServiceSpec {
        exe: std::env::current_exe()?,
        args: ctx
            .daemon_args()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        log: ctx.log_path(),
    })
}

/// `bclean service install`.
///
/// # Errors
///
/// * If the service manager fails
pub fn service_install(ctx: &Ctx) -> anyhow::Result<()> {
    let spec = spec(ctx)?;
    service::install(&spec)?;
    println!(
        "installed login service {} ({})",
        service::SERVICE_LABEL,
        service::unit_path()?
            .map_or_else(|| "task scheduler".to_owned(), |p| p.display().to_string())
    );
    Ok(())
}

/// `bclean service uninstall`.
///
/// # Errors
///
/// * If the service manager fails
pub fn service_uninstall() -> anyhow::Result<()> {
    service::uninstall()?;
    println!("removed login service {}", service::SERVICE_LABEL);
    Ok(())
}

/// `bclean service status`. Returns whether the service is installed.
///
/// # Errors
///
/// * If the service state cannot be queried
pub fn service_status() -> anyhow::Result<bool> {
    let installed = service::is_installed()?;
    println!(
        "login service {}: {}{}",
        service::SERVICE_LABEL,
        if installed {
            "installed"
        } else {
            "not installed"
        },
        service::unit_path()?
            .map(|p| format!(" ({})", p.display()))
            .unwrap_or_default()
    );
    Ok(installed)
}

/// `bclean watch`: runs the monitoring engine in the foreground, printing events, until
/// Ctrl-C.
///
/// # Errors
///
/// * If the engine cannot be opened
pub async fn watch(ctx: &Ctx) -> anyhow::Result<()> {
    let engine = ctx.engine().await?;
    let mut events = engine.subscribe();
    let printer = tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            match event {
                Event::TreeUpdated(s) => {
                    println!(
                        "tree updated: {} (generation {})",
                        s.root.display(),
                        s.generation
                    );
                }
                Event::Volumes { volumes } => {
                    for v in volumes {
                        println!(
                            "volume {}: {} free of {}",
                            v.mount_point,
                            size(v.available),
                            size(v.total)
                        );
                    }
                }
                Event::Alert(a) => println!("ALERT [{:?}] {}: {}", a.severity, a.title, a.body),
                Event::ActionProgress { value } => println!("action: {value}"),
                Event::ScanProgress { .. } => {}
            }
        }
    });
    let (tx, rx) = tokio::sync::watch::channel(false);
    let monitor = tokio::spawn(bclean_daemon::run_monitor(engine, rx));
    println!("watching (Ctrl-C to stop)...");
    tokio::signal::ctrl_c().await?;
    let _ = tx.send(true);
    monitor.await?;
    printer.abort();
    Ok(())
}
