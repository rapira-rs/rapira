//! Request headers to `Context::$metadata` of a gRPC call. The input is one `name:value` field per line.
//! The target gets the expected metadata from the contract and the gRPC spec, and compares it with the metadata from rapira.

#![no_main]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use rapira_grpc::php::call::context_metadata;

/// The transport-reserved names of `Context::$metadata`: https://github.com/rapira-rs/contract/blob/master/src/Grpc/Call/Context.php
const RESERVED_PREFIXES: [&str; 4] = ["grpc-", "connect-", "content-", "trailer-"];
const RESERVED_NAMES: [&str; 9] = [
    "te",
    "trailer",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "host",
    "accept-encoding",
];

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let headers = rapira_fuzz::header_map(data);
    let mut want = Vec::new();
    for name in headers.keys() {
        let n = name.as_str();
        if RESERVED_PREFIXES.iter().any(|p| n.starts_with(p)) || RESERVED_NAMES.contains(&n) {
            continue;
        }
        let mut values = Vec::new();
        for v in headers.get_all(name) {
            let v = v.as_bytes();
            if !n.ends_with("-bin") {
                // A text value is printable ASCII: https://github.com/rapira-rs/contract/blob/master/src/Grpc/Metadata.php
                if v.iter().all(|b| (0x20..=0x7e).contains(b)) {
                    values.push(v.to_vec());
                }
                continue;
            }
            // Split on "," first, then decode base64 with or without padding: https://github.com/grpc/grpc/blob/master/doc/PROTOCOL-HTTP2.md#requests
            for piece in v.split(|&b| b == b',') {
                // RFC 9110 OWS around a list element: https://www.rfc-editor.org/rfc/rfc9110#section-5.6.1
                let piece = piece.trim_ascii();
                // #190: rapira also accepts a padding that is shorter than the canonical one, https://github.com/rapira-rs/rapira/issues/190
                let end = piece.iter().rposition(|&b| b != b'=').map_or(0, |i| i + 1);
                let (b64, padding) = piece.split_at(end);
                if padding.len() > (4 - b64.len() % 4) % 4 {
                    continue;
                }
                if let Ok(d) = STANDARD_NO_PAD.decode(b64) {
                    values.push(d);
                }
            }
        }
        if !values.is_empty() {
            want.push((name.clone(), values));
        }
    }
    assert_eq!(context_metadata(&headers), want);
});
