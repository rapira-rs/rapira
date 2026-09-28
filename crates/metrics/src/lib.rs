//! The Prometheus endpoint: a process without PHP that the master forks and supervises as a pool of one. It reads the worker scoreboard at each scrape.

pub mod config;
mod memory;
mod serve;
mod stats;
mod text;

pub use serve::Server;
pub use text::Build;
