//! Command-scoped host for ordinary Rust simulation tests.
//!
//! [`fixture_host`] owns the bounded protocol shared with test processes.
//! `run_host` starts the test command, serves fixture requests, launches the
//! supervisor and simulator for an executing test, and returns the original
//! Cargo test outcome.

pub(crate) mod fixture_host;
mod run_host;
