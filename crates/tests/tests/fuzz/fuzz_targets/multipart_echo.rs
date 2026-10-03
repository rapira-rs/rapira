//! Encodes generated parts the way a browser does, parses the body, and compares the result with the parts byte for byte.

#![no_main]

use std::sync::LazyLock;

use arbitrary::{Arbitrary, Unstructured};
use rapira_fuzz::MAX_LEN;
use rapira_http::check::Rejection;
use rapira_http::multipart::{Limits, boundary, is_multipart, parse};

#[derive(Arbitrary)]
struct EchoPart<'a> {
    name: &'a [u8],
    filename: Option<&'a [u8]>,
    content_type: Option<&'a [u8]>,
    headers: Vec<(&'a [u8], &'a [u8])>,
    data: &'a [u8],
}

#[derive(Arbitrary)]
struct EchoForm<'a> {
    boundary: &'a [u8],
    parts: Vec<EchoPart<'a>>,
}

/// One generated part as the client encodes it.
struct Sent<'a> {
    name: Vec<u8>,
    filename: Option<Vec<u8>>,
    headers: Vec<(String, Vec<u8>)>,
    data: &'a [u8],
}

const MAX_PARTS: usize = 16;
const MAX_EXTRA_HEADERS: usize = 8;

/// Limits that a generated body never reaches.
static LIMITS: LazyLock<Limits> = LazyLock::new(|| Limits {
    dir: rapira_fuzz::spool_dir(),
    max_file_size: MAX_LEN as u64,
    max_field_size: MAX_LEN,
    max_files: MAX_PARTS,
    max_parts: MAX_PARTS,
    max_part_headers: 32,
});

/// RFC 2046 bchars: https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1
const BCHARS: &[u8] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz'()+_,-./:=? ";

/// Name and filename bytes. The WHATWG encoder escapes LF, CR and `"`, so a name has none of them: https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#multipart/form-data-encoding-algorithm
fn name_bytes(raw: &[u8]) -> Vec<u8> {
    raw.iter()
        .copied()
        .filter(|&b| b != b'"')
        // #165: the parser reads a backslash as a quoted-pair, https://github.com/rapira-rs/rapira/issues/165
        .filter(|&b| b != b'\\')
        // #181: the parser rejects a control byte other than HTAB, https://github.com/rapira-rs/rapira/issues/181
        .filter(|&b| b == b'\t' || (b >= 0x20 && b != 0x7f))
        .collect()
}

/// RFC 9110 field-value bytes without leading or trailing whitespace: https://www.rfc-editor.org/rfc/rfc9110#section-5.5
fn value_bytes(raw: &[u8]) -> Vec<u8> {
    let v: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|&b| b == b'\t' || (b >= 0x20 && b != 0x7f))
        .collect();
    v.trim_ascii().to_vec()
}

/// RFC 9110 tchar: https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_LEN {
        return;
    }
    let Ok(form) = EchoForm::arbitrary_take_rest(Unstructured::new(data)) else {
        return;
    };
    let mut bnd: Vec<u8> = form
        .boundary
        .iter()
        .copied()
        .filter(|b| BCHARS.contains(b))
        .take(70)
        .collect();
    // RFC 2046 bcharsnospace: the last boundary character is not a space.
    while bnd.last() == Some(&b' ') {
        bnd.pop();
    }
    if bnd.is_empty() {
        bnd.push(b'B');
    }
    let content_type = if bnd.iter().any(|&b| !is_tchar(b)) {
        [b"multipart/form-data; boundary=\"", &bnd[..], b"\""].concat()
    } else {
        [b"multipart/form-data; boundary=", &bnd[..]].concat()
    };
    assert!(is_multipart(&content_type));
    match boundary(&content_type) {
        Ok(b) => assert_eq!(b, bnd, "boundary of {}", content_type.escape_ascii()),
        Err(_) => panic!("boundary rejected: {}", content_type.escape_ascii()),
    }

    let delim = [b"--", &bnd[..]].concat();
    let mut body = delim.clone();
    let mut sent = Vec::new();
    for p in form.parts.iter().take(MAX_PARTS) {
        let mut name = name_bytes(p.name);
        if name.is_empty() {
            name.push(b'n');
        }
        let filename = p.filename.map(name_bytes);
        let mut disposition = [b"form-data; name=\"", &name[..], b"\""].concat();
        if let Some(f) = &filename {
            disposition.extend_from_slice(b"; filename=\"");
            disposition.extend_from_slice(f);
            disposition.push(b'"');
        }
        let mut headers = vec![("Content-Disposition".to_owned(), disposition)];
        if let Some(v) = p.content_type.map(value_bytes).filter(|v| !v.is_empty()) {
            headers.push(("Content-Type".to_owned(), v));
        }
        for (n, v) in p.headers.iter().take(MAX_EXTRA_HEADERS) {
            let n: String = n
                .iter()
                .filter(|&&b| is_tchar(b))
                .map(|&b| b as char)
                .collect();
            if n.is_empty() || n.eq_ignore_ascii_case("content-disposition") {
                continue;
            }
            headers.push((n, value_bytes(v)));
        }
        let mut part = Vec::new();
        for (n, v) in &headers {
            part.extend_from_slice(n.as_bytes());
            part.extend_from_slice(b": ");
            part.extend_from_slice(v);
            part.extend_from_slice(b"\r\n");
        }
        part.extend_from_slice(b"\r\n");
        part.extend_from_slice(p.data);
        // A client picks a boundary that no line of a part starts with, RFC 2046 section 5.1.1.
        if part
            .windows(delim.len() + 1)
            .any(|w| w[0] == b'\n' && w[1..] == delim[..])
        {
            return;
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&delim);
        sent.push(Sent {
            name,
            filename,
            headers,
            data: p.data,
        });
    }
    body.extend_from_slice(b"--\r\n");

    let ctx = body.escape_ascii();
    let got = match parse(&body, &bnd, &LIMITS) {
        Ok(got) => got,
        Err(Rejection { status, reason }) => panic!("{status} {reason}: {ctx}"),
    };
    let mut fields = got.fields.iter();
    let mut files = got.files.iter();
    for s in &sent {
        match &s.filename {
            None => {
                let f = fields
                    .next()
                    .unwrap_or_else(|| panic!("missing field: {ctx}"));
                assert_eq!(f.name, s.name, "field name: {ctx}");
                assert_eq!(f.value, s.data, "field value: {ctx}");
                assert_eq!(f.headers, s.headers, "field headers: {ctx}");
            }
            Some(filename) => {
                let f = files
                    .next()
                    .unwrap_or_else(|| panic!("missing file: {ctx}"));
                let media = s
                    .headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty());
                assert_eq!(f.name, s.name, "file name: {ctx}");
                assert_eq!(&f.client_filename, filename, "client filename: {ctx}");
                assert_eq!(f.client_media_type, media, "client media type: {ctx}");
                assert_eq!(f.headers, s.headers, "file headers: {ctx}");
                assert_eq!(f.size, s.data.len() as u64, "file size: {ctx}");
                let spooled = std::fs::read(&f.file.path).expect("spooled file");
                assert_eq!(spooled, s.data, "spooled bytes: {ctx}");
            }
        }
    }
    assert!(fields.next().is_none(), "extra field: {ctx}");
    assert!(files.next().is_none(), "extra file: {ctx}");
});
