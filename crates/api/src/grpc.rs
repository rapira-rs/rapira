use bytes::Bytes;

use crate::{Addr, FieldLines, Tls};

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub message: Bytes,
    pub metadata: FieldLines,
    pub remote: Addr,
    pub tls: Option<Tls>,
    pub received_at: f64,
    pub deadline: Option<f64>,
    pub expires_at: Option<std::time::Instant>,
}

#[derive(Debug)]
pub struct Reply {
    pub headers: FieldLines,
    pub trailers: FieldLines,
    pub result: Result<Bytes, Status>,
}

#[derive(Debug, Clone)]
pub struct Status {
    /// Error code in 1..=16.
    pub code: u8,
    pub message: String,
    pub details: Vec<ErrorDetail>,
}

#[derive(Debug, Clone)]
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
