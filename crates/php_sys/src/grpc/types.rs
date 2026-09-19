use bytes::Bytes;
use std::time::Instant;
use tokio::sync::oneshot;

use crate::types::{Addr, FieldLines, TlsView};

pub struct Request {
    pub method: String,
    pub message: Bytes,
    pub metadata: FieldLines,
    pub remote: Addr,
    pub tls: Option<TlsView>,
    pub received_at: f64,
    pub deadline: Option<f64>,
    pub expires_at: Option<Instant>,
}

#[derive(Debug)]
pub struct Reply {
    pub headers: FieldLines,
    pub trailers: FieldLines,
    pub result: Result<Bytes, Status>,
}

#[derive(Debug)]
pub struct Status {
    pub code: u8,
    pub message: String,
    pub details: Vec<ErrorDetail>,
}

#[derive(Debug)]
pub struct ErrorDetail {
    pub type_url: String,
    pub value: Bytes,
}

#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub name: String,
    pub methods: Vec<MethodInfo>,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub name: String,
    pub input_type: String,
    pub output_type: String,
}

impl Reply {
    pub(crate) fn error(code: u8, message: &str) -> Self {
        Self {
            headers: Vec::new(),
            trailers: Vec::new(),
            result: Err(Status {
                code,
                message: message.into(),
                details: Vec::new(),
            }),
        }
    }
}

pub(crate) struct Job {
    pub request: Request,
    pub sender: oneshot::Sender<Reply>,
}

impl Job {
    pub(crate) fn cancelled(&self) -> bool {
        self.sender.is_closed()
            || self
                .request
                .expires_at
                .is_some_and(|end| Instant::now() >= end)
    }
}
