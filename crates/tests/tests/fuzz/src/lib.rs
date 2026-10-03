//! Code that two or more fuzz targets use.

use std::path::PathBuf;

use http::{HeaderMap, HeaderName, HeaderValue};

/// The largest input of connect_json and multipart_echo. The CI runs use the same -max_len.
pub const MAX_LEN: usize = 4096;

/// One spool dir for all runs. A parsed body removes its spooled files when it drops.
pub fn spool_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("rapira-fuzz-spool");
    std::fs::create_dir_all(&dir).expect("creating the spool dir");
    dir
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
