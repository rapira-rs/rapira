//! The observability thread serves Prometheus metrics and health probes. Each `/metrics` or `/readyz` request reads the pool scoreboard.

pub mod config;
mod probes;
mod serve;
mod stats;
mod text;

pub use serve::Server;
pub use text::Build;
