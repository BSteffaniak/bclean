//! Local socket endpoints and newline-delimited JSON framing.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
use interprocess::local_socket::{ListenerOptions, Name};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, ReadBuf,
};

/// Maximum accepted frame size (bytes, excluding the newline).
pub const MAX_FRAME_BYTES: usize = 64 << 20;

/// Errors from the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Underlying socket I/O failed.
    #[error("ipc i/o error: {0}")]
    Io(#[from] io::Error),
    /// A frame could not be encoded or decoded.
    #[error("ipc frame codec error: {0}")]
    Codec(#[from] serde_json::Error),
    /// A frame exceeded [`MAX_FRAME_BYTES`].
    #[error("ipc frame of {0} bytes exceeds the maximum")]
    FrameTooLarge(usize),
}

/// Where the daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Filesystem socket path (Unix domain socket).
    Path(PathBuf),
    /// Namespaced name (Windows named pipe `\\.\pipe\<name>`, Linux abstract namespace).
    Namespaced(String),
}

impl Endpoint {
    /// Default endpoint for the current user.
    ///
    /// On Unix this is `<runtime_dir>/bclean.sock`; on Windows the per-user named pipe
    /// `\\.\pipe\bclean-<user>` (non-alphanumeric user characters replaced by `_`). This must
    /// match `bclean_config::paths::AppDirs::socket_path`.
    #[must_use]
    pub fn default_for(runtime_dir: &Path) -> Self {
        if cfg!(windows) {
            let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_owned());
            let user: String = user
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            Self::Path(PathBuf::from(format!(r"\\.\pipe\bclean-{user}")))
        } else {
            Self::Path(runtime_dir.join("bclean.sock"))
        }
    }

    /// Parses a configured override: a value containing a path separator is a socket path,
    /// anything else a namespaced name.
    #[must_use]
    pub fn from_config(value: &str) -> Self {
        if value.contains('/') || value.contains('\\') {
            Self::Path(PathBuf::from(value))
        } else {
            Self::Namespaced(value.to_owned())
        }
    }

    fn name(&self) -> io::Result<Name<'_>> {
        use interprocess::local_socket::{
            GenericFilePath, GenericNamespaced, ToFsName as _, ToNsName as _,
        };
        match self {
            Self::Path(path) => path.as_path().to_fs_name::<GenericFilePath>(),
            Self::Namespaced(name) => name.as_str().to_ns_name::<GenericNamespaced>(),
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(path) => write!(f, "{}", path.display()),
            Self::Namespaced(name) => write!(f, "{name}"),
        }
    }
}

/// Binds a listener at `endpoint`, replacing a stale socket file left by a dead daemon.
///
/// # Errors
///
/// * If the endpoint name is invalid on this platform
/// * If the socket cannot be created (for example another live daemon owns it)
pub fn listen(endpoint: &Endpoint) -> io::Result<Listener> {
    // Named pipe paths (`\\.\pipe\...`) have no directory to create.
    if cfg!(unix)
        && let Endpoint::Path(path) = endpoint
        && let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent)?;
        restrict_permissions(parent);
    }
    ListenerOptions::new()
        .name(endpoint.name()?)
        .try_overwrite(true)
        .create_tokio()
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    // Best effort: the socket directory should only be accessible by its owner.
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
const fn restrict_permissions(_dir: &Path) {}

/// Accepts the next connection and splits it into framed halves.
///
/// # Errors
///
/// * If accepting the connection fails
pub async fn accept(listener: &Listener) -> io::Result<(FrameReader, FrameWriter)> {
    Ok(split(listener.accept().await?))
}

/// Connects to the daemon at `endpoint`.
///
/// # Errors
///
/// * If no daemon is listening or the endpoint name is invalid
pub async fn connect(endpoint: &Endpoint) -> io::Result<(FrameReader, FrameWriter)> {
    Ok(split(Stream::connect(endpoint.name()?).await?))
}

fn split(stream: Stream) -> (FrameReader, FrameWriter) {
    let shared = Arc::new(stream);
    (
        FrameReader {
            inner: BufReader::new(Shared(Arc::clone(&shared))),
            buf: Vec::new(),
        },
        FrameWriter {
            inner: Shared(shared),
        },
    )
}

/// One direction of a stream shared between reader and writer (`&Stream` is read/write).
struct Shared(Arc<Stream>);

impl AsyncRead for Shared {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut &*self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Shared {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut &*self.0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut &*self.0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut &*self.0).poll_shutdown(cx)
    }
}

/// Reading half of a framed connection.
pub struct FrameReader {
    inner: BufReader<Shared>,
    buf: Vec<u8>,
}

impl FrameReader {
    /// Reads the next frame, or `None` at end of stream.
    ///
    /// # Errors
    ///
    /// * [`TransportError::Io`] on socket errors
    /// * [`TransportError::FrameTooLarge`] if the peer sends an oversized frame
    /// * [`TransportError::Codec`] if the frame is not valid JSON for `T`
    pub async fn read<T: DeserializeOwned>(&mut self) -> Result<Option<T>, TransportError> {
        self.buf.clear();
        loop {
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(TransportError::Io(io::ErrorKind::UnexpectedEof.into()))
                };
            }
            if let Some(pos) = available.iter().position(|b| *b == b'\n') {
                self.buf.extend_from_slice(&available[..pos]);
                self.inner.consume(pos + 1);
                break;
            }
            let len = available.len();
            self.buf.extend_from_slice(available);
            self.inner.consume(len);
            if self.buf.len() > MAX_FRAME_BYTES {
                return Err(TransportError::FrameTooLarge(self.buf.len()));
            }
        }
        if self.buf.len() > MAX_FRAME_BYTES {
            return Err(TransportError::FrameTooLarge(self.buf.len()));
        }
        Ok(Some(serde_json::from_slice(&self.buf)?))
    }
}

/// Writing half of a framed connection.
pub struct FrameWriter {
    inner: Shared,
}

impl FrameWriter {
    /// Writes one frame and flushes it.
    ///
    /// # Errors
    ///
    /// * [`TransportError::Codec`] if `frame` cannot be serialized
    /// * [`TransportError::FrameTooLarge`] if the encoded frame is oversized
    /// * [`TransportError::Io`] on socket errors
    pub async fn write<T: Serialize + Sync>(&mut self, frame: &T) -> Result<(), TransportError> {
        let mut bytes = serde_json::to_vec(frame)?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(TransportError::FrameTooLarge(bytes.len()));
        }
        bytes.push(b'\n');
        self.inner.write_all(&bytes).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_frames_over_a_socket() {
        let dir = std::env::temp_dir().join(format!("bclean-proto-{}", std::process::id()));
        let endpoint = if cfg!(windows) {
            Endpoint::Namespaced(format!("bclean-test-{}", std::process::id()))
        } else {
            Endpoint::Path(dir.join("t.sock"))
        };
        let listener = listen(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let (mut reader, mut writer) = accept(&listener).await.unwrap();
            while let Some(value) = reader.read::<serde_json::Value>().await.unwrap() {
                writer.write(&value).await.unwrap();
            }
        });
        let (mut reader, mut writer) = connect(&endpoint).await.unwrap();
        let big = "x".repeat(200_000);
        for value in [serde_json::json!({"a": 1}), serde_json::json!({"big": big})] {
            writer.write(&value).await.unwrap();
            assert_eq!(
                reader.read::<serde_json::Value>().await.unwrap(),
                Some(value)
            );
        }
        drop(writer);
        drop(reader);
        server.await.unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
