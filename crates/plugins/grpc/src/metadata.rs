use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use extension_api::FieldLines;
use http::{HeaderMap, HeaderName, HeaderValue};
use tonic::Status;

const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub(crate) fn reserved(name: &str) -> bool {
    name.starts_with("grpc-")
        || name.starts_with("content-")
        || name.starts_with("connect-")
        || matches!(
            name,
            "te" | "host"
                | "user-agent"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "transfer-encoding"
                | "upgrade"
                | "trailer"
                | "accept-encoding"
        )
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

fn valid_text(value: &[u8]) -> bool {
    value.iter().all(|b| (0x20..=0x7e).contains(b))
}

// Binary fields can contain comma-joined base64 values.
// https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
pub(crate) fn decode(headers: &HeaderMap) -> Result<FieldLines, Status> {
    let mut fields = Vec::new();
    for (name, value) in headers {
        let name = name.as_str();
        if reserved(name) || !valid_name(name) {
            continue;
        }
        if name.ends_with("-bin") {
            for part in value.as_bytes().split(|b| *b == b',') {
                let value = BASE64
                    .decode(part.trim_ascii())
                    .map_err(|_| Status::invalid_argument("Invalid binary metadata"))?;
                fields.push((name.to_owned(), value));
            }
        } else if valid_text(value.as_bytes()) {
            fields.push((name.to_owned(), value.as_bytes().to_vec()));
        }
    }
    Ok(fields)
}

pub(crate) fn encode(fields: FieldLines) -> Result<HeaderMap, Status> {
    let mut headers = HeaderMap::with_capacity(fields.len());
    for (name, value) in fields {
        let name = name.to_ascii_lowercase();
        if reserved(&name) {
            continue;
        }
        if !valid_name(&name) {
            return Err(Status::internal("Invalid response metadata"));
        }
        let value = if name.ends_with("-bin") {
            BASE64.encode(value).into_bytes()
        } else if valid_text(&value) {
            value
        } else {
            return Err(Status::internal("Invalid response metadata"));
        };
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Status::internal("Invalid response metadata"))?;
        let value = HeaderValue::from_bytes(&value)
            .map_err(|_| Status::internal("Invalid response metadata"))?;
        headers.append(name, value);
    }
    Ok(headers)
}
