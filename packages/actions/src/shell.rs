//! Running tool commands through the platform shell, with templating, timeouts, and
//! cancellation.
//!
//! Unix uses `/bin/sh -c`; Windows uses `cmd.exe /D /S /C`. Placeholders `{path}`, `{parent}`,
//! and `{name}` are substituted with shell-quoted values in command lines and raw values in
//! working directories.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::CancelToken;

/// Exit code conventionally returned by `sh` when a command is not found (127) or not
/// executable (126), and by `cmd.exe` when a command is not recognized (9009).
const NOT_FOUND_CODES: &[i32] = &[126, 127, 9009];

/// Maximum number of stderr bytes kept.
const STDERR_TAIL: usize = 4096;

/// Quotes `arg` as a single word for the platform shell.
///
/// On Unix the value is wrapped in single quotes (embedded `'` become `'\''`). On Windows it is
/// wrapped in double quotes with embedded `"` doubled; note that `cmd.exe` still expands
/// `%VAR%` inside quotes, which cannot be escaped portably.
#[must_use]
pub fn quote(arg: &str) -> String {
    if cfg!(windows) {
        format!("\"{}\"", arg.replace('"', "\"\""))
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Substitutes `{path}`, `{parent}`, and `{name}` in `template` for `path`.
///
/// When `shell_quote` is true the substituted values are quoted with [`quote`]; use that for
/// command lines, and `false` for paths such as working directories. Other braces are left
/// untouched.
#[must_use]
// `{path}` etc. are bclean template placeholders, not `format!` arguments.
#[allow(clippy::literal_string_with_formatting_args)]
pub fn render_template(template: &str, path: &Path, shell_quote: bool) -> String {
    let full = path.to_string_lossy();
    let parent = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let render = |value: &str| {
        if shell_quote {
            quote(value)
        } else {
            value.to_owned()
        }
    };
    let mut out = String::with_capacity(template.len() + full.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let (value, len) = if tail.starts_with("{path}") {
            (Some(render(&full)), "{path}".len())
        } else if tail.starts_with("{parent}") {
            (Some(render(&parent)), "{parent}".len())
        } else if tail.starts_with("{name}") {
            (Some(render(&name)), "{name}".len())
        } else {
            (None, 1)
        };
        match value {
            Some(v) => out.push_str(&v),
            None => out.push('{'),
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

/// Result of a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutcome {
    /// Exit code (`None` if killed by a signal or timed out).
    pub code: Option<i32>,
    /// The command was killed after exceeding its timeout.
    pub timed_out: bool,
    /// The command was killed because the run was cancelled.
    pub cancelled: bool,
    /// Last bytes of standard error.
    pub stderr: String,
}

impl ShellOutcome {
    /// Whether the command exited successfully.
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.code, Some(0)) && !self.timed_out && !self.cancelled
    }

    /// Whether the exit code indicates the command (tool) was not found.
    #[must_use]
    pub fn not_found(&self) -> bool {
        self.code.is_some_and(|c| NOT_FOUND_CODES.contains(&c))
    }
}

/// Builds a [`Command`] that runs `line` through the platform shell.
#[must_use]
pub fn shell_command(line: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("cmd.exe");
        cmd.raw_arg(format!("/D /S /C \"{line}\""));
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(line);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group so a timeout kills the whole pipeline.
            cmd.process_group(0);
        }
        cmd
    }
}

/// Runs `line` in `cwd` via the platform shell, killing it after `timeout` or on cancellation.
///
/// # Errors
///
/// * The shell itself cannot be spawned.
pub fn run(
    line: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &CancelToken,
) -> io::Result<ShellOutcome> {
    let mut cmd = shell_command(line);
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr {
            let mut chunk = [0_u8; 4096];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > STDERR_TAIL * 2 {
                    buf.drain(..buf.len() - STDERR_TAIL);
                }
            }
        }
        let start = buf.len().saturating_sub(STDERR_TAIL);
        String::from_utf8_lossy(&buf[start..]).into_owned()
    });
    let (code, timed_out, cancelled) = wait(&mut child, timeout, cancel)?;
    let stderr = reader.join().unwrap_or_default();
    Ok(ShellOutcome {
        code,
        timed_out,
        cancelled,
        stderr,
    })
}

fn wait(
    child: &mut Child,
    timeout: Duration,
    cancel: &CancelToken,
) -> io::Result<(Option<i32>, bool, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), false, false));
        }
        let timed_out = Instant::now() >= deadline;
        let cancelled = cancel.is_cancelled();
        if timed_out || cancelled {
            kill(child);
            child.wait().ok();
            return Ok((None, timed_out, cancelled && !timed_out));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall; the child was spawned as leader of its own process group, so
        // `-pid` targets only that group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    child.kill().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // `{path}` etc. are bclean template placeholders, not `format!` arguments.
    #[allow(clippy::literal_string_with_formatting_args)]
    fn templating_quotes_values() {
        let path = Path::new("/tmp/it's here/target");
        let line = render_template(
            "rm -rf {path} && echo {name} {unknown} {parent}",
            path,
            true,
        );
        if cfg!(windows) {
            assert!(line.contains("\"/tmp/it's here/target\""));
        } else {
            assert_eq!(
                line,
                r"rm -rf '/tmp/it'\''s here/target' && echo 'target' {unknown} '/tmp/it'\''s here'"
            );
        }
        assert_eq!(
            render_template("{parent}", path, false),
            "/tmp/it's here".to_owned()
        );
    }

    #[cfg(unix)]
    #[test]
    fn quoted_value_round_trips_through_shell() {
        let dir = tempfile::tempdir().unwrap();
        let tricky = dir.path().join("a b'c\"$(echo pwned)`x`;d");
        std::fs::create_dir(&tricky).unwrap();
        let out = dir.path().join("out.txt");
        let line = format!(
            "printf '%s' {} > {}",
            quote(&tricky.to_string_lossy()),
            quote(&out.to_string_lossy())
        );
        let result = run(
            &line,
            dir.path(),
            Duration::from_secs(10),
            &CancelToken::new(),
        )
        .unwrap();
        assert!(result.success(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(out).unwrap(),
            tricky.to_string_lossy()
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::new();
        let slow = run("sleep 5", dir.path(), Duration::from_millis(100), &cancel).unwrap();
        assert!(slow.timed_out);
        assert!(!slow.success());
        let missing = run(
            "definitely-not-a-real-tool-bclean",
            dir.path(),
            Duration::from_secs(5),
            &cancel,
        )
        .unwrap();
        assert!(missing.not_found());
    }
}
