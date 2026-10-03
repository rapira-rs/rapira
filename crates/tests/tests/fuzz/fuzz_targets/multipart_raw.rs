//! Raw bytes through the multipart parser of the dispatcher mode. The bytes before the first LF are the Content-Type value and the rest is the body. The calls follow parse_multipart in crates/plugins/http/src/handler.rs.

#![no_main]

use std::sync::LazyLock;

use rapira_http::multipart::{Limits, ParseError, boundary, is_multipart, parse};

/// Small limits, so that a 4 KiB input can go over each count and size limit.
static LIMITS: LazyLock<Limits> = LazyLock::new(|| Limits {
    dir: rapira_fuzz::spool_dir(),
    max_file_size: 1024,
    max_field_size: 1024,
    max_files: 4,
    max_parts: 8,
    max_part_headers: 4,
});

/// A malformed body gives 400 and a body over a limit gives 413. The spool dir exists, so an I/O error is a failure.
fn rejected(e: ParseError, allowed: &[u16]) {
    match e {
        ParseError::Rejected { status, reason } => {
            assert!(
                allowed.contains(&status.as_u16()),
                "status {status}: {reason}"
            );
        }
        ParseError::Io(e) => panic!("io error: {e}"),
    }
}

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let (content_type, body) = match data.iter().position(|&b| b == b'\n') {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    if !is_multipart(content_type) {
        return;
    }
    let b = match boundary(content_type) {
        Ok(b) => b,
        Err(e) => return rejected(e, &[400]),
    };
    // RFC 2046 boundary: 1 to 70 characters, https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1
    assert!((1..=70).contains(&b.len()), "boundary length {}", b.len());
    if body.is_empty() {
        return;
    }
    let l = &*LIMITS;
    let form = match parse(body, &b, l) {
        Ok(form) => form,
        Err(e) => return rejected(e, &[400, 413]),
    };
    assert!(form.fields.len() + form.files.len() <= l.max_parts);
    assert!(form.files.len() <= l.max_files);
    for f in &form.fields {
        assert!(!f.name.is_empty());
        assert!(f.value.len() <= l.max_field_size);
        assert!(f.headers.len() <= l.max_part_headers);
    }
    for f in &form.files {
        assert!(!f.name.is_empty());
        assert!(f.size <= l.max_file_size);
        assert!(f.headers.len() <= l.max_part_headers);
        let len = std::fs::metadata(&f.file.path).expect("spooled file").len();
        assert_eq!(len, f.size, "spooled file size");
    }
});
