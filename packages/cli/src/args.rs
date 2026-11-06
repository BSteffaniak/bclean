//! Command-line argument definitions.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Fast, action-oriented disk usage analyzer, cleaner, and monitor.
#[derive(Debug, Parser)]
#[command(name = "bclean", version, about, propagate_version = true)]
pub struct Cli {
    /// Additional configuration file layered on top of global and project config.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Don't use (or start) the daemon; do everything in this process.
    #[arg(long, global = true)]
    pub no_daemon: bool,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Subcommand (defaults to the interactive browser).
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to browse when no subcommand is given (defaults to the current directory).
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

/// Output format for listing commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Format {
    /// Human-readable table.
    #[default]
    Table,
    /// JSON.
    Json,
}

/// Sort order for listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum SortBy {
    /// Highest cleanup score first.
    #[default]
    Score,
    /// Largest first.
    Size,
    /// Longest idle first.
    Age,
    /// Path order.
    Path,
}

/// Filters shared by `ls` and `clean`.
#[derive(Debug, Clone, Args, Default)]
pub struct Selection {
    /// Only ephemeral (regenerable) items.
    #[arg(long, short = 'e')]
    pub ephemeral: bool,

    /// Only stale items; optionally override the idle threshold (e.g. `--stale=60d`).
    #[arg(long, short = 's', num_args = 0..=1, default_missing_value = "", value_name = "AGE")]
    pub stale: Option<String>,

    /// Only items matched by these rule ids.
    #[arg(long = "rule", value_name = "ID")]
    pub rules: Vec<String>,

    /// Only items in these categories (rust, node, xcode, ...).
    #[arg(long = "category", value_name = "NAME")]
    pub categories: Vec<String>,

    /// Ignore items smaller than this (e.g. `100M`).
    #[arg(long, value_name = "SIZE")]
    pub min_size: Option<String>,

    /// Only rules marked safe for automatic cleaning.
    #[arg(long)]
    pub auto_only: bool,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive browser (default).
    Browse {
        /// Root to browse.
        path: Option<PathBuf>,
    },
    /// Scan a directory and print a summary (or the full tree as JSON).
    Scan {
        /// Root to scan.
        path: Option<PathBuf>,
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
        /// Number of largest entries to show.
        #[arg(long, short = 'n', default_value_t = 20)]
        top: usize,
        /// Rescan even if the daemon has a fresh snapshot.
        #[arg(long)]
        force: bool,
    },
    /// List classified (cleanable) items.
    Ls {
        /// Root to inspect.
        path: Option<PathBuf>,
        #[command(flatten)]
        selection: Selection,
        /// Sort order.
        #[arg(long, value_enum, default_value_t)]
        sort: SortBy,
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
        /// Maximum rows.
        #[arg(long, short = 'n')]
        limit: Option<usize>,
    },
    /// Clean classified items (dry run unless `--yes`/confirmed).
    Clean {
        /// Root to clean, or explicit paths.
        paths: Vec<PathBuf>,
        #[command(flatten)]
        selection: Selection,
        /// Only show what would be done.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Don't ask for confirmation.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Output format for the plan/report.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
    /// Explain how a path is classified (rule, signals, score, action).
    Explain {
        /// Path to explain.
        path: PathBuf,
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
    /// Disk usage per volume, daemon state, and active alerts.
    Status {
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
    /// What grew recently (requires the daemon's history).
    History {
        /// Root to inspect.
        path: Option<PathBuf>,
        /// Window (e.g. `7d`).
        #[arg(long, default_value = "7d")]
        since: String,
        /// Maximum rows.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
    /// Undo a journaled action (latest reversible one when no id is given).
    Undo {
        /// Journal entry id.
        id: Option<String>,
    },
    /// Show the action journal.
    Journal {
        /// Maximum entries.
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
    /// Inspect rules.
    Rules {
        #[command(subcommand)]
        command: RulesCommand,
    },
    /// Inspect configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Install or remove the login service that keeps the daemon running.
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Run the monitoring engine in the foreground (no daemon).
    Watch,
}

/// `bclean rules` subcommands.
#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// List effective rules.
    List {
        /// Output format.
        #[arg(long, value_enum, default_value_t)]
        format: Format,
    },
    /// Show one rule's effective definition.
    Show {
        /// Rule id.
        id: String,
    },
    /// Test which rule (if any) matches a path.
    Test {
        /// Path to test.
        path: PathBuf,
    },
}

/// `bclean config` subcommands.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the effective (merged) configuration as TOML.
    Show,
    /// Print configuration file locations.
    Paths,
}

/// `bclean daemon` subcommands.
#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground (used by autostart and service managers).
    Run,
    /// Start the daemon in the background if it isn't running.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Restart the daemon.
    Restart,
    /// Show daemon status.
    Status,
    /// Print the daemon log file path (or its tail with `--tail`).
    Logs {
        /// Print the last N lines.
        #[arg(long)]
        tail: Option<usize>,
    },
}

/// `bclean service` subcommands.
#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Install and start the login service.
    Install,
    /// Stop and remove the login service.
    Uninstall,
    /// Show whether the service is installed.
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_default_browse_and_stale_flag() {
        let cli = Cli::parse_from(["bclean", "/tmp"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.path, Some(PathBuf::from("/tmp")));

        let cli = Cli::parse_from(["bclean", "ls", "--stale", "-e"]);
        let Some(Command::Ls { selection, .. }) = cli.command else {
            panic!("expected ls");
        };
        assert_eq!(selection.stale.as_deref(), Some(""));
        assert!(selection.ephemeral);

        let cli = Cli::parse_from(["bclean", "clean", "--stale=60d", "--rule", "cargo_target"]);
        let Some(Command::Clean { selection, .. }) = cli.command else {
            panic!("expected clean");
        };
        assert_eq!(selection.stale.as_deref(), Some("60d"));
        assert_eq!(selection.rules, vec!["cargo_target"]);
    }
}
