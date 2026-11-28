use std::collections::HashSet;
use std::sync::Arc;

use connectrpc::{ConnectError, Router};
use connectrpc_health::{
    Checker, HealthExt, HealthService, Status, StatusStream, apply_request_limits, request_limits,
};
use tokio::sync::watch;

struct Health {
    names: HashSet<String>,
    status: watch::Receiver<Status>,
}

impl Health {
    fn status(&self, name: &str) -> Result<&watch::Receiver<Status>, ConnectError> {
        if name.is_empty() || self.names.contains(name) {
            return Ok(&self.status);
        }
        // Keep the bounded error text of connectrpc-health's StaticChecker.
        let end = name.floor_char_boundary(128);
        let message = if end == name.len() {
            format!("unknown service {name}")
        } else {
            format!(
                "unknown service {}... [name truncated, {} bytes]",
                &name[..end],
                name.len()
            )
        };
        Err(ConnectError::not_found(message))
    }
}

impl Checker for Health {
    async fn check(&self, name: &str) -> Result<Status, ConnectError> {
        Ok(*self.status(name)?.borrow())
    }

    async fn watch(&self, name: &str) -> Result<StatusStream, ConnectError> {
        Ok(StatusStream::from_watch(self.status(name)?.clone()))
    }
}

pub(crate) fn install(names: impl Iterator<Item = String>) -> (Router, watch::Sender<Status>) {
    let (sender, status) = watch::channel(Status::Serving);
    let health = HealthService::new(Health {
        names: names.collect(),
        status,
    });
    let router = Arc::new(health).register(Router::new());
    (apply_request_limits(router, request_limits()), sender)
}
