#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! bclean daemon: index ownership, watching, policies, alerts, and service install.

pub mod priority;
pub mod runtime;
pub mod server;
pub mod service;

pub use runtime::{DaemonHandler, endpoint_for, handle, run_daemon, run_monitor};
pub use server::{Handler, Server};
