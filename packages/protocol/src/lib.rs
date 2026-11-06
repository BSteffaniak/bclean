#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Versioned IPC protocol messages between bclean clients and the daemon.
//!
//! Transport is a local socket (Unix domain socket on macOS/Linux, named pipe on Windows)
//! carrying newline-delimited JSON frames. Every connection starts with a [`ClientFrame::Hello`]
//! / [`ServerFrame::Hello`] exchange; a client that sees a different [`PROTOCOL_VERSION`] or
//! daemon version restarts the daemon.
//!
//! Large payloads (scan trees) are never sent over the socket: the daemon persists snapshots to
//! the index directory and replies with a [`SnapshotRef`] the client loads from disk.

mod messages;
mod transport;

pub use messages::*;
pub use transport::*;

/// Wire protocol version. Bump on any incompatible message change.
pub const PROTOCOL_VERSION: u32 = 1;
