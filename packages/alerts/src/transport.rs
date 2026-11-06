//! External delivery mechanisms (desktop, command, webhook).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bclean_model::AlertEvent;

/// Delivery failure.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// The sink did not finish within its timeout.
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    /// A command sink exited unsuccessfully.
    #[error("command exited with {code:?}: {stderr}")]
    CommandFailed {
        /// Exit code, if any.
        code: Option<i32>,
        /// Tail of standard error.
        stderr: String,
    },
    /// An I/O error (spawning, pipes).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// HTTP failure (connection, TLS, or non-2xx status).
    #[error("http error: {0}")]
    Http(String),
    /// Desktop notification failure.
    #[error("desktop notification failed: {0}")]
    Desktop(String),
    /// Anything else.
    #[error("{0}")]
    Other(String),
}

/// Performs the I/O for desktop, command, and webhook sinks.
///
/// Implementations must return within roughly `timeout`.
pub trait Transport: Send + Sync {
    /// Shows a desktop notification.
    ///
    /// # Errors
    ///
    /// * The notification could not be shown or timed out.
    fn desktop(&self, event: &AlertEvent, timeout: Duration) -> Result<(), DeliveryError>;

    /// Runs `run` via the platform shell with `payload` on stdin.
    ///
    /// # Errors
    ///
    /// * The command cannot be spawned, exits unsuccessfully, or times out.
    fn command(&self, run: &str, payload: &[u8], timeout: Duration) -> Result<(), DeliveryError>;

    /// Sends `body` (JSON) to `url`.
    ///
    /// # Errors
    ///
    /// * The request fails, times out, or returns a non-2xx status.
    fn webhook(
        &self,
        url: &str,
        method: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), DeliveryError>;
}

/// Real transport.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTransport;

impl Transport for SystemTransport {
    fn desktop(&self, event: &AlertEvent, timeout: Duration) -> Result<(), DeliveryError> {
        let title = event.title.clone();
        let body = event.body.clone();
        #[cfg(all(unix, not(target_os = "macos")))]
        let urgency = match event.severity {
            bclean_model::Severity::Info => notify_rust::Urgency::Low,
            bclean_model::Severity::Warning => notify_rust::Urgency::Normal,
            bclean_model::Severity::Critical => notify_rust::Urgency::Critical,
        };
        run_with_timeout(timeout, move || {
            let mut notification = notify_rust::Notification::new();
            notification.appname("bclean").summary(&title).body(&body);
            #[cfg(all(unix, not(target_os = "macos")))]
            notification.urgency(urgency);
            notification
                .show()
                .map(|_| ())
                .map_err(|e| DeliveryError::Desktop(e.to_string()))
        })
    }

    fn command(&self, run: &str, payload: &[u8], timeout: Duration) -> Result<(), DeliveryError> {
        let mut child = shell_command(run)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        // Feed stdin and drain stderr on helper threads so a child that ignores its input or
        // writes a lot of output cannot deadlock us.
        if let Some(mut stdin) = child.stdin.take() {
            let payload = payload.to_vec();
            std::thread::spawn(move || {
                stdin.write_all(&payload).ok();
            });
        }
        let stderr_reader = child.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                stderr.by_ref().take(64 * 1024).read_to_end(&mut buf).ok();
                let start = buf.len().saturating_sub(2048);
                String::from_utf8_lossy(&buf[start..]).into_owned()
            })
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                kill_tree(&mut child);
                child.wait().ok();
                return Err(DeliveryError::Timeout(timeout));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if status.success() {
            return Ok(());
        }
        let stderr = stderr_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        Err(DeliveryError::CommandFailed {
            code: status.code(),
            stderr: stderr.trim().to_owned(),
        })
    }

    fn webhook(
        &self,
        url: &str,
        method: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
        timeout: Duration,
    ) -> Result<(), DeliveryError> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(true)
            .build()
            .into();
        let mut request = ureq::http::Request::builder()
            .method(method.to_ascii_uppercase().as_str())
            .uri(url)
            .header("content-type", "application/json")
            .header("user-agent", concat!("bclean/", env!("CARGO_PKG_VERSION")));
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let request = request
            .body(body.to_vec())
            .map_err(|e| DeliveryError::Http(e.to_string()))?;
        agent
            .run(request)
            .map(|_| ())
            .map_err(|e| DeliveryError::Http(e.to_string()))
    }
}

/// Runs `f` on a helper thread, giving up after `timeout` (the thread is left to finish on its
/// own).
fn run_with_timeout(
    timeout: Duration,
    f: impl FnOnce() -> Result<(), DeliveryError> + Send + 'static,
) -> Result<(), DeliveryError> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("bclean-alert".to_owned())
        .spawn(move || {
            tx.send(f()).ok();
        })?;
    rx.recv_timeout(timeout)
        .unwrap_or(Err(DeliveryError::Timeout(timeout)))
}

fn shell_command(line: &str) -> Command {
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
            cmd.process_group(0);
        }
        cmd
    }
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: plain syscall; the child leads its own process group, so `-pid` only targets
        // processes spawned for this alert.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    child.kill().ok();
}
