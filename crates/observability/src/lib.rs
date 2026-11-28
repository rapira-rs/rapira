//! The observability thread serves Prometheus metrics and health probes. Each `/metrics` or `/readyz` request reads a snapshot of each pool.

pub mod config;
mod serve;
mod stats;
mod text;

pub use serve::Server;
pub use text::Build;
