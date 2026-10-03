//! Ferry mirrors an explicit allowlist of GitHub repositories into Forgejo.
//!
//! The binary in `main.rs` is a thin dispatcher. Everything it does lives in
//! these modules so that integration tests can drive the same code.

pub mod app;
pub mod cli;
pub mod config;
pub mod forge;
pub mod git;
pub mod health;
pub mod scheduler;
pub mod sync;
pub mod telemetry;

/// Crate version plus the git SHA the binary was built from.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FERRY_GIT_SHA"), ")");
