//! Raw bytes through the multipart parser of the dispatcher mode. The bytes before the first LF are the Content-Type value and the rest is the body. The calls follow parse_multipart in crates/plugins/http/src/handler.rs. The body parses as one part and split into parts, and the two results must agree.

#![no_main]

use std::sync::LazyLock;

use bytes::Bytes;
use rapira_http::check::Rejection;
use rapira_http::multipart::{HeaderValues, Limits, boundary, is_multipart, parse};

/// Small limits, so that a 4 KiB input can go over each count and size limit.
static LIMITS: LazyLock<Limits> = LazyLock::new(|| Limits {
    dir: rapira_fuzz::spool_dir(),
    max_file_size: 1024,
    max_field_size: 1024,
    max_files: 4,
    max_parts: 8,
    max_part_headers: 4,
});

/// A malformed body gives 400 and a body over a limit gives 413. An I/O error gives 500, which the allowed list refuses.
fn rejected(Rejection { status, reason }: Rejection, allowed: &[u16]) {
    assert!(
        allowed.contains(&status.as_u16()),
        "status {status}: {reason}"
    );
}

/// One entry per name, names compared byte for byte, each entry with a value, and at most max_part_headers field lines in all.
fn assert_grouped(headers: &[HeaderValues], l: &Limits) {
    let mut lines = 0;
    for (i, h) in headers.iter().enumerate() {
        assert!(!h.values.is_empty(), "entry without a value: {}", h.name);
        assert!(
            headers[..i].iter().all(|o| o.name != h.name),
            "repeated entry: {}",
            h.name
        );
        lines += h.values.len();
    }
    assert!(lines <= l.max_part_headers, "{lines} field lines");
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
    // A boundary has 1 or more characters, as in php-src.
    assert!(!b.is_empty(), "empty boundary");
    if body.is_empty() {
        return;
    }
    let l = &*LIMITS;
    let whole = parse(&[Bytes::copy_from_slice(body)], &b, l);
    let parts = rapira_fuzz::split(body);
    let lens: Vec<usize> = parts.iter().map(Bytes::len).collect();
    rapira_fuzz::assert_same_parse(
        format_args!("parts {lens:?}"),
        &whole,
        &parse(&parts, &b, l),
    );
    let form = match whole {
        Ok(form) => form,
        Err(e) => return rejected(e, &[400, 413]),
    };
    assert!(form.fields.len() + form.files.len() <= l.max_parts);
    assert!(form.files.len() <= l.max_files);
    for f in &form.fields {
        assert!(!f.name.is_empty());
        assert!(f.value.len() <= l.max_field_size);
        assert_grouped(&f.headers, l);
    }
    for f in &form.files {
        assert!(!f.name.is_empty());
        assert!(f.size <= l.max_file_size);
        assert_grouped(&f.headers, l);
        let len = std::fs::metadata(&f.file.path).expect("spooled file").len();
        assert_eq!(len, f.size, "spooled file size");
    }
});
