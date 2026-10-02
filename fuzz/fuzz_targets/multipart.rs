#![no_main]

use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::LazyLock;

use libfuzzer_sys::fuzz_target;
use rapira_http::multipart::{Limits, ParseError, boundary, is_multipart, parse};

/// Small limits that `[http.uploads]` accepts, so a 4 KiB input can go over each count and size limit. The spool dir does not exist, so a file part fails with NotFound and writes nothing.
static LIMITS: LazyLock<Limits> = LazyLock::new(|| Limits {
    dir: PathBuf::from("/nonexistent/rapira-fuzz"),
    max_file_size: 1024 * 1024,
    max_field_size: 1024,
    max_files: 20,
    max_parts: 8,
    max_part_headers: 4,
});

// The bytes before the first LF are the Content-Type value, the rest is the body. The calls follow parse_multipart in crates/plugins/http/src/handler.rs.
fuzz_target!(|data: &[u8]| {
    let (content_type, body) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    if !is_multipart(content_type) {
        return;
    }
    let b = match boundary(content_type) {
        Ok(b) => b,
        Err(ParseError::Rejected { status, .. }) => {
            assert_eq!(status, 400);
            return;
        }
        Err(ParseError::Io(e)) => panic!("boundary: {e}"),
    };
    assert!((1..=70).contains(&b.len()), "boundary length {}", b.len());
    if body.is_empty() {
        return;
    }
    match parse(body, &b, &LIMITS) {
        Ok(form) => {
            assert!(form.files.is_empty(), "file part without a spool dir");
            assert!(
                form.fields.len() <= LIMITS.max_parts,
                "{} parts",
                form.fields.len()
            );
            for f in &form.fields {
                assert!(
                    f.value.len() <= LIMITS.max_field_size,
                    "field of {} bytes",
                    f.value.len()
                );
            }
        }
        Err(ParseError::Rejected { status, .. }) => {
            assert!(status == 400 || status == 413, "status {status}");
        }
        Err(ParseError::Io(e)) => assert_eq!(e.kind(), ErrorKind::NotFound, "{e}"),
    }
});
