#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared bclean domain and configuration model types.
//!
//! These types are the contract between crates: configuration decodes into them, the rules
//! engine produces [`Classification`]s, the policy engine consumes them, and the protocol
//! sends them over IPC. Serde enum values use `SCREAMING_SNAKE_CASE`; struct keys use
//! `snake_case`.

mod classification;
mod config;
mod dashboard;
mod units;

pub use classification::*;
pub use config::*;
pub use dashboard::*;
pub use units::*;
