//! Login-service registration: launchd (macOS), systemd `--user` (Linux), Task Scheduler
//! (Windows). Unit file rendering is pure so it can be tested and previewed.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

/// Service label / unit / task name.
pub const SERVICE_LABEL: &str = "dev.bclean.daemon";

/// Errors installing or removing the service.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Filesystem error writing or removing the unit.
    #[error("service file i/o error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Cause.
        source: std::io::Error,
    },
    /// The service manager command failed.
    #[error("`{command}` failed: {message}")]
    Command {
        /// Command that ran.
        command: String,
        /// Output or error.
        message: String,
    },
    /// The user's home or config directory could not be determined.
    #[error("could not determine the user's home directory")]
    NoHome,
    /// Not supported on this platform.
    #[error("service installation is not supported on this platform")]
    Unsupported,
}

/// What to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSpec {
    /// Absolute path of the bclean executable.
    pub exe: PathBuf,
    /// Arguments that run the daemon in the foreground.
    pub args: Vec<String>,
    /// Log file for stdout/stderr (launchd only; systemd uses the journal).
    pub log: PathBuf,
}

/// Renders a launchd agent plist.
#[must_use]
pub fn render_launchd(spec: &ServiceSpec) -> String {
    let mut args = format!(
        "        <string>{}</string>\n",
        xml_escape(&spec.exe.to_string_lossy())
    );
    for arg in &spec.args {
        let _ = writeln!(args, "        <string>{}</string>", xml_escape(arg));
    }
    let log = xml_escape(&spec.log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>Nice</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    )
}

/// Renders a systemd user unit.
#[must_use]
pub fn render_systemd(spec: &ServiceSpec) -> String {
    let mut exec = systemd_quote(&spec.exe.to_string_lossy());
    for arg in &spec.args {
        exec.push(' ');
        exec.push_str(&systemd_quote(arg));
    }
    format!(
        "[Unit]
Description=bclean disk usage monitor and cleaner
Documentation=https://github.com/BSteffaniak/bclean

[Service]
Type=simple
ExecStart={exec}
Restart=on-failure
RestartSec=10
Nice=10
IOSchedulingClass=idle

[Install]
WantedBy=default.target
"
    )
}

/// Renders the Task Scheduler `/TR` command line.
#[must_use]
pub fn render_task_command(spec: &ServiceSpec) -> String {
    let mut command = format!("\"{}\"", spec.exe.display());
    for arg in &spec.args {
        command.push(' ');
        if arg.contains(' ') {
            let _ = write!(command, "\"{arg}\"");
        } else {
            command.push_str(arg);
        }
    }
    command
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn systemd_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@".contains(c))
    {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn home() -> Result<PathBuf, ServiceError> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or(ServiceError::NoHome)
}

/// Location of the unit file on this platform (`None` on Windows, which has no file).
///
/// # Errors
///
/// * [`ServiceError::NoHome`] if the home directory is unknown
pub fn unit_path() -> Result<Option<PathBuf>, ServiceError> {
    let home = home()?;
    Ok(if cfg!(target_os = "macos") {
        Some(
            home.join("Library/LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist")),
        )
    } else if cfg!(windows) {
        None
    } else {
        let config =
            std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        Some(config.join("systemd/user/bclean.service"))
    })
}

#[cfg(unix)]
fn write_file(path: &std::path::Path, contents: &str) -> Result<(), ServiceError> {
    let io = |source| ServiceError::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    std::fs::write(path, contents).map_err(io)
}

fn run(program: &str, args: &[&str]) -> Result<String, ServiceError> {
    let command = format!("{program} {}", args.join(" "));
    let output =
        Command::new(program)
            .args(args)
            .output()
            .map_err(|error| ServiceError::Command {
                command: command.clone(),
                message: error.to_string(),
            })?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(ServiceError::Command {
            command,
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

#[cfg(target_os = "macos")]
fn uid() -> String {
    run("id", &["-u"]).map_or_else(|_| "0".to_owned(), |out| out.trim().to_owned())
}

/// Installs and starts the login service.
///
/// # Errors
///
/// * [`ServiceError::Io`] if the unit file cannot be written
/// * [`ServiceError::Command`] if the service manager rejects it
/// * [`ServiceError::Unsupported`] on unsupported platforms
pub fn install(spec: &ServiceSpec) -> Result<(), ServiceError> {
    #[cfg(target_os = "macos")]
    {
        let path = unit_path()?.ok_or(ServiceError::Unsupported)?;
        let domain = format!("gui/{}", uid());
        // Re-installing: unload any previous definition first (ignore "not loaded").
        let _ = run(
            "launchctl",
            &["bootout", &format!("{domain}/{SERVICE_LABEL}")],
        );
        write_file(&path, &render_launchd(spec))?;
        run(
            "launchctl",
            &["bootstrap", &domain, &path.to_string_lossy()],
        )?;
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = unit_path()?.ok_or(ServiceError::Unsupported)?;
        write_file(&path, &render_systemd(spec))?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run(
            "systemctl",
            &["--user", "enable", "--now", "bclean.service"],
        )?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let command = render_task_command(spec);
        run(
            "schtasks",
            &[
                "/Create",
                "/F",
                "/SC",
                "ONLOGON",
                "/RL",
                "LIMITED",
                "/TN",
                SERVICE_LABEL,
                "/TR",
                &command,
            ],
        )?;
        let _ = run("schtasks", &["/Run", "/TN", SERVICE_LABEL]);
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = spec;
        Err(ServiceError::Unsupported)
    }
}

/// Stops and removes the login service. Succeeds if it was not installed.
///
/// # Errors
///
/// * [`ServiceError::Io`] if the unit file cannot be removed
/// * [`ServiceError::Command`] if the service manager fails unexpectedly
/// * [`ServiceError::Unsupported`] on unsupported platforms
pub fn uninstall() -> Result<(), ServiceError> {
    #[cfg(target_os = "macos")]
    {
        let path = unit_path()?.ok_or(ServiceError::Unsupported)?;
        let _ = run(
            "launchctl",
            &["bootout", &format!("gui/{}/{SERVICE_LABEL}", uid())],
        );
        remove_if_exists(&path)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let path = unit_path()?.ok_or(ServiceError::Unsupported)?;
        let _ = run(
            "systemctl",
            &["--user", "disable", "--now", "bclean.service"],
        );
        remove_if_exists(&path)?;
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = run("schtasks", &["/End", "/TN", SERVICE_LABEL]);
        match run("schtasks", &["/Delete", "/F", "/TN", SERVICE_LABEL]) {
            Ok(_) => Ok(()),
            Err(_) if !is_installed()? => Ok(()),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(ServiceError::Unsupported)
    }
}

#[cfg(unix)]
fn remove_if_exists(path: &std::path::Path) -> Result<(), ServiceError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ServiceError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Whether the login service is installed.
///
/// # Errors
///
/// * [`ServiceError::NoHome`] if the home directory is unknown
pub fn is_installed() -> Result<bool, ServiceError> {
    if cfg!(windows) {
        Ok(run("schtasks", &["/Query", "/TN", SERVICE_LABEL]).is_ok())
    } else {
        Ok(unit_path()?.is_some_and(|path| path.exists()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/opt/my tools/bclean"),
            args: vec!["daemon".to_owned(), "run".to_owned()],
            log: PathBuf::from("/tmp/bclean & co.log"),
        }
    }

    #[test]
    fn launchd_plist_escapes_and_lists_args() {
        let plist = render_launchd(&spec());
        assert!(plist.contains("<string>/opt/my tools/bclean</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("bclean &amp; co.log"));
        assert!(plist.contains(SERVICE_LABEL));
    }

    #[test]
    fn systemd_unit_quotes_paths_with_spaces() {
        let unit = render_systemd(&spec());
        assert!(unit.contains("ExecStart=\"/opt/my tools/bclean\" daemon run"));
    }

    #[test]
    fn task_command_quotes_exe() {
        assert_eq!(
            render_task_command(&spec()),
            "\"/opt/my tools/bclean\" daemon run"
        );
    }
}
