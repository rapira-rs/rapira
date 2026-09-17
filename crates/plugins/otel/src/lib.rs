//! Shared telemetry and the OTLP exporter plugin.

#[cfg(feature = "exporter")]
pub mod exporter;
#[cfg(feature = "sdk")]
pub mod ipc;
#[cfg(feature = "exporter")]
pub mod process;

mod context;
#[cfg(feature = "sdk")]
mod metrics;
#[cfg(feature = "sdk")]
mod sdk;

pub use context::{server_span, trace_context};
#[cfg(feature = "sdk")]
pub use metrics::{flush_metrics, metrics_interval, record_duration, request_finished};
#[cfg(feature = "sdk")]
pub use sdk::{after_fork, layer};

#[cfg(not(feature = "sdk"))]
pub fn after_fork(_pool: &'static str) {}

#[cfg(not(feature = "sdk"))]
pub fn request_finished(_method: &str, _status: u16, _elapsed: std::time::Duration) {}

#[cfg(not(feature = "sdk"))]
pub fn record_duration(_operation: &'static str, _elapsed: std::time::Duration) {}

#[cfg(not(feature = "sdk"))]
pub fn metrics_interval() -> Option<std::time::Duration> {
    None
}

#[cfg(not(feature = "sdk"))]
pub fn flush_metrics() {}

#[cfg(all(test, feature = "sdk"))]
mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    use opentelemetry_proto::tonic::common::v1::{KeyValue, any_value::Value};
    use rapira_config::OtelSettings;

    use crate::ipc::Sender;

    pub(crate) fn settings() -> OtelSettings {
        OtelSettings {
            enabled: true,
            endpoint: "http://localhost:4318".into(),
            service_name: "sdk-test".into(),
            sample_ratio: 1.0,
            traces: true,
            logs: true,
            metrics: false,
            batch_size: 16,
            queue_size: 64,
            flush_interval_ms: 1000,
            export_timeout_secs: 5,
            headers: Default::default(),
        }
    }

    pub(crate) struct Wire {
        _dir: tempfile::TempDir,
        listener: UnixListener,
        pub(crate) sender: Sender,
    }

    impl Wire {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("sdk.sock");
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            Self {
                _dir: dir,
                listener,
                sender: Sender::new(path),
            }
        }

        pub(crate) fn records(&self) -> Vec<(u8, Vec<u8>)> {
            let mut records = Vec::new();
            loop {
                let (mut stream, _) = match self.listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("accept failed: {error}"),
                };
                stream.set_nonblocking(true).unwrap();
                let mut bytes = Vec::new();
                if let Err(error) = stream.read_to_end(&mut bytes) {
                    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
                }
                let mut remaining = bytes.as_slice();
                while !remaining.is_empty() {
                    let length = u32::from_be_bytes(remaining[..4].try_into().unwrap()) as usize;
                    assert!(length > 0 && remaining.len() >= length + 4);
                    records.push((remaining[4], remaining[5..length + 4].to_vec()));
                    remaining = &remaining[length + 4..];
                }
            }
            records
        }
    }

    pub(crate) fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a Value> {
        attributes
            .iter()
            .find(|attribute| attribute.key == key)?
            .value
            .as_ref()?
            .value
            .as_ref()
    }
}
