//! Code that two or more fuzz targets use.

use std::fmt::Display;
use std::path::PathBuf;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue};
use rapira_http::check::Rejection;
use rapira_http::multipart::{MultipartBody, UploadedFile};

/// The largest input of connect_json and multipart_echo. The CI runs use the same -max_len.
pub const MAX_LEN: usize = 4096;

/// One spool dir for all runs. A parsed body removes its spooled files when it drops.
pub fn spool_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("rapira-fuzz-spool");
    std::fs::create_dir_all(&dir).expect("creating the spool dir");
    dir
}

/// The body in up to 8 parts. Each split position is the little-endian u16 of the two bytes at index 0, 97, 194 and so on, modulo len + 1; a position of 0 or len gives an empty part, which the view accepts and the transport never delivers.
pub fn split(body: &[u8]) -> Vec<Bytes> {
    let mut at = Vec::new();
    for i in (0..body.len().saturating_sub(1)).step_by(97).take(7) {
        at.push(u16::from_le_bytes([body[i], body[i + 1]]) as usize % (body.len() + 1));
    }
    at.sort_unstable();
    at.dedup();
    let mut parts = Vec::new();
    let mut start = 0;
    for end in at {
        parts.push(Bytes::copy_from_slice(&body[start..end]));
        start = end;
    }
    parts.push(Bytes::copy_from_slice(&body[start..]));
    parts
}

/// Asserts that two parses of one body agree: the status and the reason of a rejection, or each field and each file with the bytes of its spooled file.
pub fn assert_same_parse(
    ctx: impl Display,
    want: &Result<MultipartBody, Rejection>,
    got: &Result<MultipartBody, Rejection>,
) {
    match (want, got) {
        (Ok(want), Ok(got)) => {
            assert_eq!(want.fields.len(), got.fields.len(), "field count: {ctx}");
            for (w, g) in want.fields.iter().zip(&got.fields) {
                assert_eq!(
                    (&w.name, &w.value, &w.headers),
                    (&g.name, &g.value, &g.headers),
                    "field: {ctx}"
                );
            }
            assert_eq!(want.files.len(), got.files.len(), "file count: {ctx}");
            for (w, g) in want.files.iter().zip(&got.files) {
                assert_eq!(
                    (&w.name, &w.client_filename, &w.client_media_type),
                    (&g.name, &g.client_filename, &g.client_media_type),
                    "file: {ctx}"
                );
                assert_eq!(
                    (&w.headers, w.size),
                    (&g.headers, g.size),
                    "file headers and size: {ctx}"
                );
                let spooled = |f: &UploadedFile| std::fs::read(&f.file.path).expect("spooled file");
                assert_eq!(spooled(w), spooled(g), "spooled bytes: {ctx}");
            }
        }
        (Err(w), Err(g)) => assert_eq!(
            (w.status, &w.reason),
            (g.status, &g.reason),
            "rejection: {ctx}"
        ),
        (Ok(_), Err(g)) => panic!("only the split parse rejects with {g}: {ctx}"),
        (Err(w), Ok(_)) => panic!("only the one-part parse rejects with {w}: {ctx}"),
    }
}

/// One `name:value` field per line. A line that the http crate rejects as a field is left out.
pub fn header_map(data: &[u8]) -> HeaderMap {
    data.split(|&b| b == b'\n')
        .filter_map(|line| {
            let colon = line.iter().position(|&b| b == b':')?;
            let name = HeaderName::from_bytes(&line[..colon]).ok()?;
            Some((name, HeaderValue::from_bytes(&line[colon + 1..]).ok()?))
        })
        .collect()
}

/// The `HTTP_*` variable name changes `-` to `_`: https://www.rfc-editor.org/rfc/rfc3875#section-4.1.18
/// A field name with only these bytes cannot give the same variable as another field name.
pub fn safe_name(name: &HeaderName) -> bool {
    name.as_str()
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
