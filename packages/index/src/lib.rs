#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Persistent scan index storage for bclean.
//!
//! * [`snapshot`]: a compact, versioned, checksummed binary snapshot of a
//!   [`bclean_tree::Tree`] per scanned root, written atomically and loaded with bulk array
//!   conversion.
//! * [`history`]: an SQLite database of volume usage samples and directory size rollups with
//!   retention pruning, growth queries, and time-to-full estimation.
//! * [`Store`]: a facade that opens both under a caller-provided data directory.

pub mod history;
mod pathbytes;
pub mod snapshot;
mod store;

pub use history::{
    DirGrowth, History, HistoryError, PruneStats, SCHEMA_VERSION, SizePoint, TimeToFull,
    estimate_time_to_full,
};
pub use snapshot::{Snapshot, SnapshotError, SnapshotHeader, SnapshotMeta};
pub use store::{Store, StoreError};
