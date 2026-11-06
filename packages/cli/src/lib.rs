#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Command-line interface for bclean

/// Runs the bclean command-line interface.
#[must_use]
pub fn run() -> std::process::ExitCode {
    std::process::ExitCode::SUCCESS
}
