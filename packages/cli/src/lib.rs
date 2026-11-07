#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for bclean.
//!
//! By default commands go through the daemon (started on demand); `--no-daemon` runs the
//! engine in-process. Exit codes: `0` success, `1` error, `2` usage error.

pub mod args;
pub mod commands;
pub mod conn;
pub mod daemon_cmd;
pub mod output;
pub mod tui_backend;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser as _;

use crate::args::{Cli, Command, ConfigCommand, DaemonCommand, RulesCommand, ServiceCommand};
use crate::conn::Ctx;

/// Runs the bclean command-line interface with the process arguments.
#[must_use]
pub fn run() -> ExitCode {
    run_from(std::env::args_os())
}

/// Runs the CLI with explicit arguments (the first is the program name).
#[must_use]
pub fn run_from<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let is_daemon_run = matches!(
        cli.command,
        Some(Command::Daemon {
            command: DaemonCommand::Run
        })
    );
    init_logging(cli.verbose, is_daemon_run);
    match execute(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("bclean: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: u8, daemon: bool) {
    let level = match (verbose, daemon) {
        (0, false) => log::LevelFilter::Warn,
        (0, true) | (1, _) => log::LevelFilter::Info,
        (2, _) => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    let mut builder = pretty_env_logger::formatted_timed_builder();
    builder.filter_level(level);
    if let Ok(filters) = std::env::var("RUST_LOG") {
        builder.parse_filters(&filters);
    }
    let _ = builder.try_init();
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("bclean")
        .build()?)
}

/// Executes a parsed command line. `Ok(false)` means "completed, but exit with status 1".
///
/// # Errors
///
/// * Any command failure
#[allow(clippy::too_many_lines)] // one flat dispatch table reads best
pub fn execute(cli: Cli) -> anyhow::Result<bool> {
    let ctx = Ctx::load(cli.config.clone(), cli.no_daemon, cli.one_file_system)?;
    if matches!(
        cli.command,
        Some(Command::Daemon {
            command: DaemonCommand::Run
        })
    ) && ctx.settings.config().daemon.background_priority
    {
        // Before the runtime starts its threads, which inherit the priority.
        if let Err(e) = bclean_daemon::priority::lower_process_priority() {
            log::warn!("could not lower the daemon's priority: {e}");
        }
    }
    let rt = runtime()?;
    let Some(command) = cli.command else {
        return browse(&ctx, &rt, cli.path.as_deref());
    };
    match command {
        Command::Browse { path } => browse(&ctx, &rt, path.as_deref().or(cli.path.as_deref())),
        Command::Scan {
            path,
            format,
            top,
            force,
        } => rt
            .block_on(commands::scan(&ctx, path.as_deref(), format, top, force))
            .map(|()| true),
        Command::Ls {
            path,
            selection,
            sort,
            format,
            limit,
        } => rt
            .block_on(commands::ls(
                &ctx,
                path.as_deref(),
                &selection,
                sort,
                format,
                limit,
            ))
            .map(|()| true),
        Command::Clean {
            paths,
            selection,
            dry_run,
            yes,
            format,
        } => rt.block_on(commands::clean(
            &ctx, &paths, &selection, dry_run, yes, format,
        )),
        Command::Explain { path, format } => rt
            .block_on(commands::explain(&ctx, &path, format))
            .map(|()| true),
        Command::Status { format } => rt.block_on(commands::status(&ctx, format)).map(|()| true),
        Command::History {
            path,
            since,
            limit,
            format,
        } => rt
            .block_on(commands::history(
                &ctx,
                path.as_deref(),
                &since,
                limit,
                format,
            ))
            .map(|()| true),
        Command::Undo { id } => rt
            .block_on(commands::undo(&ctx, id.as_deref()))
            .map(|()| true),
        Command::Journal { limit, format } => rt
            .block_on(commands::journal(&ctx, limit, format))
            .map(|()| true),
        Command::Rules { command } => match command {
            RulesCommand::List { format } => commands::rules_list(&ctx, format),
            RulesCommand::Show { id } => commands::rules_show(&ctx, &id),
            RulesCommand::Test { path } => commands::rules_test(&ctx, &path),
        }
        .map(|()| true),
        Command::Config { command } => match command {
            ConfigCommand::Show => commands::config_show(&ctx),
            ConfigCommand::Paths => commands::config_paths(&ctx),
        }
        .map(|()| true),
        Command::Daemon { command } => match command {
            DaemonCommand::Run => rt.block_on(daemon_cmd::run(&ctx)).map(|()| true),
            DaemonCommand::Start => rt.block_on(daemon_cmd::start(&ctx)).map(|()| true),
            DaemonCommand::Stop => rt.block_on(daemon_cmd::stop(&ctx)).map(|_| true),
            DaemonCommand::Restart => rt
                .block_on(async {
                    daemon_cmd::stop(&ctx).await?;
                    daemon_cmd::start(&ctx).await
                })
                .map(|()| true),
            DaemonCommand::Status => rt.block_on(daemon_cmd::status(&ctx)),
            DaemonCommand::Logs { tail } => daemon_cmd::logs(&ctx, tail).map(|()| true),
        },
        Command::Service { command } => match command {
            ServiceCommand::Install => daemon_cmd::service_install(&ctx).map(|()| true),
            ServiceCommand::Uninstall => daemon_cmd::service_uninstall().map(|()| true),
            ServiceCommand::Status => daemon_cmd::service_status(),
        },
        Command::Watch => rt.block_on(daemon_cmd::watch(&ctx)).map(|()| true),
    }
}

fn tui_options(ctx: &Ctx, root: &Path) -> bclean_tui::TuiOptions {
    let tui = &ctx.settings.config().tui;
    bclean_tui::TuiOptions {
        title: format!("bclean — {}", root.display()),
        apparent: ctx.settings.config().scan.size_mode == bclean_model::SizeModeSpec::Apparent,
        show_hidden: tui.show_hidden,
        ..bclean_tui::TuiOptions::default()
    }
}

/// Where the UI opens: plain `bclean` starts on the dashboard with the browser rooted at the
/// index root containing the current directory (already indexed, so instant, and holding the
/// paths the dashboard drills into), opened at the current directory. `bclean <path>` browses
/// `path`. `tui.start_view` overrides which view comes first.
fn start_location(
    ctx: &Ctx,
    path: Option<&Path>,
) -> anyhow::Result<(PathBuf, bool, Option<PathBuf>)> {
    let target = commands::abs(path)?;
    let dashboard = match ctx.settings.config().tui.start_view {
        bclean_model::TuiStartView::Auto => path.is_none(),
        bclean_model::TuiStartView::Dashboard => true,
        bclean_model::TuiStartView::Browser => false,
    };
    if path.is_some() {
        return Ok((target, dashboard, None));
    }
    let mut roots = ctx.settings.index_roots();
    roots
        .extend(bclean_engine::volume_roots(&ctx.settings, ctx.one_filesystem).unwrap_or_default());
    let root = roots
        .into_iter()
        .filter(|root| target.starts_with(root))
        .max_by_key(|root| root.components().count());
    Ok(match root {
        Some(root) if root != target => (root, dashboard, Some(target)),
        _ => (target, dashboard, None),
    })
}

/// Launches the interactive browser (daemon backend by default, local with `--no-daemon`).
fn browse(ctx: &Ctx, rt: &tokio::runtime::Runtime, path: Option<&Path>) -> anyhow::Result<bool> {
    let (root, start_on_dashboard, initial_path) = start_location(ctx, path)?;
    let options = bclean_tui::TuiOptions {
        start_on_dashboard,
        initial_path,
        one_filesystem: ctx.one_filesystem,
        ..tui_options(ctx, &root)
    };
    let exit = if ctx.no_daemon {
        let engine = rt.block_on(ctx.engine())?;
        let backend = tui_backend::LocalBackend::new(engine, rt.handle().clone(), root)
            .with_one_filesystem(ctx.one_filesystem);
        bclean_tui::run(backend, options)?
    } else {
        // Make sure the daemon is up (autostart) before entering the terminal UI.
        rt.block_on(bclean_client::Client::connect(&ctx.client_options()))?;
        let backend = tui_backend::DaemonBackend::new(
            ctx.client_options(),
            std::sync::Arc::clone(&ctx.settings),
            ctx.dirs.data.clone(),
            rt.handle().clone(),
            root,
        )
        .with_one_filesystem(ctx.one_filesystem);
        bclean_tui::run(backend, options)?
    };
    if !exit.marked.is_empty() {
        println!("{} path(s) were still marked:", exit.marked.len());
        for p in &exit.marked {
            println!("  {}", p.display());
        }
    }
    Ok(true)
}
