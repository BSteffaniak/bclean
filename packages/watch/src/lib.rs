#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Filesystem change sources and volume usage monitoring for bclean.
//!
//! * [`list_volumes`] / [`volume_for`]: mounted volume capacity as
//!   [`bclean_model::VolumeStats`] (macOS `getfsstat`, Linux `mountinfo` + `statvfs`, Windows
//!   `GetDiskFreeSpaceExW`).
//! * [`open_change_source`]: a [`ChangeSource`] emitting debounced, collapsed
//!   [`ChangeBatch`]es of directories to rescan (macOS FSEvents with event-id replay, Linux
//!   fanotify or inotify, Windows `ReadDirectoryChangesW`).

pub mod change;
pub mod volume;

pub use change::{
    ChangeBatch, ChangeSource, WatchBackend, WatchConfig, WatchCursor, WatchError, collapse_dirty,
    open_change_source,
};
pub use volume::{
    VolumeError, VolumeFilter, list_volumes, list_volumes_with, volume_for, volume_own_used,
};
