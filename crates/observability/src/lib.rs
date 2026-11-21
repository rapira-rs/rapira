//! The observability process runs without PHP. The master forks and supervises it as a pool of one. It serves the Prometheus metrics and the livez and readyz probes. `/metrics` and `/readyz` read the worker scoreboard at each request.

pub mod config;
mod memory;
mod probes;
mod serve;
mod stats;
mod text;

pub use serve::Server;
pub use text::Build;
