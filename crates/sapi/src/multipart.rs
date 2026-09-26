// https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::types::{FormField, MultipartBody, SpooledFile, UploadedFile};
use memchr::memmem;

#[derive(Debug, Clone)]
pub struct Limits {
    pub dir: PathBuf,
    pub max_file_size: u64,
    pub max_field_size: usize,
    pub max_files: usize,
    pub max_parts: usize,
    pub max_part_headers: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            dir: std::env::temp_dir(),
            max_file_size: 2 * 1024 * 1024,
            max_field_size: 256 * 1024,
            max_files: 20,
            max_parts: 1024,
            max_part_headers: 32,
        }
    }
}

/// The spool dir of this worker process under `base`. The master sweeps the dirs whose process is gone.
pub fn worker_spool_dir(base: &Path) -> PathBuf {
    base.join(format!("rapira-spool-{}", std::process::id()))
}

#[derive(Debug)]
pub enum ParseError {
    /// The body is malformed (400) or over a limit (413).
    Rejected {
        status: u16,
        reason: String,
    },
    Io(std::io::Error),
}

impl From<std::io::Error> for ParseError {
    fn from(e: std::io::Error) -> Self {
        ParseError::Io(e)
    }
}

fn bad(reason: impl Into<String>) -> ParseError {
    ParseError::Rejected {
        status: 400,
        reason: reason.into(),
    }
}

fn over(reason: impl Into<String>) -> ParseError {
    ParseError::Rejected {
        status: 413,
        reason: reason.into(),
    }
}

fn trim_ows(mut b: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = b {
        b = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = b {
        b = rest;
    }
    b
}

pub fn is_multipart(content_type: &[u8]) -> bool {
    let media = content_type.split(|&b| b == b';').next().unwrap_or(b"");
    trim_ows(media).eq_ignore_ascii_case(b"multipart/form-data")
}

/// Case-insensitive, quoted form unquoted, unquoted form terminated by `,` (php-src rfc1867.c:707-751), capped at the RFC 2046 §5.1.1 70 characters.
pub fn boundary(content_type: &[u8]) -> Result<Vec<u8>, ParseError> {
    for seg in content_type.split(|&b| b == b';').skip(1) {
        let Some(eq) = memchr::memchr(b'=', seg) else {
            continue;
        };
        if !trim_ows(&seg[..eq]).eq_ignore_ascii_case(b"boundary") {
            continue;
        }
        let val = trim_ows(&seg[eq + 1..]);
        let val = if let [b'"', inner @ ..] = val {
            match memchr::memchr(b'"', inner) {
                Some(end) => &inner[..end],
                None => return Err(bad("unterminated quoted boundary parameter")),
            }
        } else {
            match memchr::memchr(b',', val) {
                Some(end) => trim_ows(&val[..end]),
                None => val,
            }
        };
        if val.is_empty() {
            return Err(bad("empty boundary parameter"));
        }
        if val.len() > 70 {
            return Err(bad("boundary parameter longer than 70 characters"));
        }
        return Ok(val.to_vec());
    }
    Err(bad("multipart/form-data without a boundary parameter"))
}

struct DelimHit {
    line_start: usize,
    after: usize,
    close: bool,
}

/// A delimiter must start a line; a line merely beginning with the boundary bytes is content, so the scan continues past it.
fn next_delimiter(
    body: &[u8],
    mut from: usize,
    finder: &memmem::Finder<'_>,
    dlen: usize,
) -> Option<DelimHit> {
    while let Some(i) = finder.find(&body[from..]).map(|o| o + from) {
        if i == 0 || body[i - 1] == b'\n' {
            let mut j = i + dlen;
            if body[j..].starts_with(b"--") {
                // after the dashes a close-delimiter allows only transport padding and a line ending or end of body, RFC 2046 §5.1.1
                let mut k = j + 2;
                while matches!(body.get(k), Some(b' ' | b'\t')) {
                    k += 1;
                }
                if matches!(body.get(k), None | Some(b'\n'))
                    || (body.get(k) == Some(&b'\r') && body.get(k + 1) == Some(&b'\n'))
                {
                    return Some(DelimHit {
                        line_start: i,
                        after: j + 2,
                        close: true,
                    });
                }
            }
            while matches!(body.get(j), Some(b' ' | b'\t')) {
                j += 1;
            }
            match body.get(j) {
                Some(b'\n') => {
                    return Some(DelimHit {
                        line_start: i,
                        after: j + 1,
                        close: false,
                    });
                }
                Some(b'\r') if body.get(j + 1) == Some(&b'\n') => {
                    return Some(DelimHit {
                        line_start: i,
                        after: j + 2,
                        close: false,
                    });
                }
                _ => {}
            }
        }
        from = i + 1;
    }
    None
}

/// Parses a non-empty body; the empty-body case lands on the contract's string arm (`$body === ''`) instead.
/// https://www.rfc-editor.org/rfc/rfc7578
pub fn parse(body: &[u8], boundary: &[u8], limits: &Limits) -> Result<MultipartBody, ParseError> {
    let delim: Vec<u8> = [b"--".as_slice(), boundary].concat();
    let finder = memmem::Finder::new(&delim);

    let mut fields: Vec<FormField> = Vec::new();
    let mut files: Vec<UploadedFile> = Vec::new();

    let opening = next_delimiter(body, 0, &finder, delim.len())
        .ok_or_else(|| bad("no opening boundary line"))?;
    if opening.close {
        return Ok(MultipartBody { fields, files });
    }
    let mut part_start = opening.after;

    loop {
        if fields.len() + files.len() + 1 > limits.max_parts {
            return Err(over("part count over max_parts"));
        }
        let ending = next_delimiter(body, part_start, &finder, delim.len())
            .ok_or_else(|| bad("no closing boundary line"))?;
        // the line terminator preceding a delimiter line belongs to the delimiter, not the part, RFC 2046 §5.1.1
        let mut part_end = ending.line_start;
        if part_end > part_start && body[part_end - 1] == b'\n' {
            part_end -= 1;
            if part_end > part_start && body[part_end - 1] == b'\r' {
                part_end -= 1;
            }
        }
        parse_part(&body[part_start..part_end], limits, &mut fields, &mut files)?;

        if ending.close {
            return Ok(MultipartBody { fields, files });
        }
        part_start = ending.after;
    }
}

/// Header section ends at the first empty line, CRLF or bare LF; returns (head including the terminator, body).
fn split_head(part: &[u8]) -> Result<(&[u8], &[u8]), ParseError> {
    if let Some(rest) = part.strip_prefix(b"\r\n") {
        return Ok((&part[..2], rest));
    }
    if let Some(rest) = part.strip_prefix(b"\n") {
        return Ok((&part[..1], rest));
    }
    let mut i = 0;
    while let Some(nl) = memchr::memchr(b'\n', &part[i..]).map(|o| o + i) {
        match part.get(nl + 1) {
            Some(b'\n') => return Ok((&part[..nl + 2], &part[nl + 2..])),
            Some(b'\r') if part.get(nl + 2) == Some(&b'\n') => {
                return Ok((&part[..nl + 3], &part[nl + 3..]));
            }
            _ => i = nl + 1,
        }
    }
    Err(bad("part without a header/body separator"))
}

/// (name, filename) from a content-disposition value.
type Disposition = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The disposition type token is not enforced: php-src rfc1867.c reads the parameters regardless.
fn disposition_params(v: &[u8]) -> Result<Disposition, ParseError> {
    let mut name: Option<Vec<u8>> = None;
    let mut filename: Option<Vec<u8>> = None;
    let mut i = memchr::memchr(b';', v).map(|i| i + 1).unwrap_or(v.len());
    while i < v.len() {
        let Some(eq) = memchr::memchr(b'=', &v[i..]).map(|o| o + i) else {
            break;
        };
        let key = trim_ows(&v[i..eq]);
        let mut j = eq + 1;
        while matches!(v.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        let (val, next) = if v.get(j) == Some(&b'"') {
            let mut out = Vec::new();
            let mut k = j + 1;
            loop {
                match v.get(k) {
                    None => return Err(bad("unterminated quoted-string in content-disposition")),
                    Some(b'"') => break (out, k + 1),
                    Some(b'\\') => {
                        let Some(&esc) = v.get(k + 1) else {
                            return Err(bad("unterminated quoted-string in content-disposition"));
                        };
                        out.push(esc);
                        k += 2;
                    }
                    Some(&byte) => {
                        out.push(byte);
                        k += 1;
                    }
                }
            }
        } else {
            let end = memchr::memchr(b';', &v[j..])
                .map(|o| o + j)
                .unwrap_or(v.len());
            (trim_ows(&v[j..end]).to_vec(), end)
        };
        let slot = if key.eq_ignore_ascii_case(b"name") {
            Some(&mut name)
        } else if key.eq_ignore_ascii_case(b"filename") {
            Some(&mut filename)
        } else {
            None
        };
        if let Some(slot) = slot {
            if slot.is_some() {
                return Err(bad(
                    "duplicated name/filename parameter in content-disposition",
                ));
            }
            *slot = Some(val);
        }
        i = memchr::memchr(b';', &v[next..])
            .map(|o| o + next + 1)
            .unwrap_or(v.len());
    }
    Ok((name, filename))
}

/// Nothing fallible may sit between keep() and the SpooledFile wrap, or the kept file has no owner to unlink it.
fn spool(bytes: &[u8], dir: &std::path::Path) -> Result<SpooledFile, ParseError> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    let (file, path) = tmp.keep().map_err(|e| ParseError::Io(e.error))?;
    drop(file);
    Ok(SpooledFile { path })
}

/// A filename parameter, even an empty one, makes the part a file part.
fn parse_part(
    part: &[u8],
    limits: &Limits,
    fields: &mut Vec<FormField>,
    files: &mut Vec<UploadedFile>,
) -> Result<(), ParseError> {
    let (head, body) = split_head(part)?;

    let mut hbuf = vec![httparse::EMPTY_HEADER; limits.max_part_headers];
    let parsed = match httparse::parse_headers(head, &mut hbuf) {
        Ok(httparse::Status::Complete((_, headers))) => headers,
        Ok(httparse::Status::Partial) => return Err(bad("truncated part header section")),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(over("part headers over max_part_headers"));
        }
        // httparse enforces RFC 9110 token field names, stricter than rfc1867.c
        Err(_) => return Err(bad("unparseable part header section")),
    };

    let mut headers: Vec<(String, Vec<u8>)> = Vec::with_capacity(parsed.len());
    let mut disposition: Option<&[u8]> = None;
    let mut media_type: Option<&[u8]> = None;
    for h in parsed {
        if h.name.eq_ignore_ascii_case("content-disposition") {
            if disposition.replace(h.value).is_some() {
                return Err(bad("duplicated content-disposition in a part"));
            }
        } else if h.name.eq_ignore_ascii_case("content-type") && media_type.is_none() {
            media_type = Some(h.value);
        }
        headers.push((h.name.to_owned(), h.value.to_vec()));
    }
    let Some(disposition) = disposition else {
        return Err(bad("part without content-disposition"));
    };

    let (name, filename) = disposition_params(disposition)?;
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(bad(
            "content-disposition without a non-empty name parameter",
        ));
    };
    let client_media_type = media_type
        .map(trim_ows)
        .filter(|v| !v.is_empty())
        .map(<[u8]>::to_vec);

    match filename {
        Some(client_filename) => {
            if files.len() + 1 > limits.max_files {
                return Err(over("file parts over max_files"));
            }
            if body.len() as u64 > limits.max_file_size {
                return Err(over("file part over max_file_size"));
            }
            let file = spool(body, &limits.dir)?;
            files.push(UploadedFile {
                name,
                client_filename,
                client_media_type,
                headers,
                file,
                size: body.len() as u64,
            });
        }
        None => {
            if body.len() > limits.max_field_size {
                return Err(over("field part over max_field_size"));
            }
            fields.push(FormField {
                name,
                value: body.to_vec(),
                headers,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits::default()
    }

    fn ok(body: &[u8]) -> MultipartBody {
        parse(body, b"B", &limits()).unwrap_or_else(|_| panic!("expected a parse"))
    }

    /// The status of the rejection.
    fn rejected(body: &[u8], l: &Limits) -> u16 {
        match parse(body, b"B", l) {
            Err(ParseError::Rejected { status, .. }) => status,
            Err(ParseError::Io(e)) => panic!("io error: {e}"),
            Ok(_) => panic!("expected a rejection"),
        }
    }

    #[test]
    fn fields_and_files_arrive_in_document_order() {
        let body = b"--B\r\ncontent-disposition: form-data; name=\"a\"\r\n\r\none\r\n\
--B\r\ncontent-disposition: form-data; name=\"f\"; filename=\"x.bin\"\r\ncontent-type: application/octet-stream\r\n\r\nPAYLOAD\r\n\
--B\r\ncontent-disposition: form-data; name=\"b\"\r\n\r\ntwo\r\n\
--B--";
        let mb = ok(body);
        assert_eq!(mb.fields.len(), 2);
        assert_eq!(mb.fields[0].name, b"a");
        assert_eq!(mb.fields[0].value, b"one");
        assert_eq!(mb.fields[1].name, b"b");
        assert_eq!(mb.fields[1].value, b"two");
        assert_eq!(mb.files.len(), 1);
        let f = &mb.files[0];
        assert_eq!(f.name, b"f");
        assert_eq!(f.client_filename, b"x.bin");
        assert_eq!(
            f.client_media_type.as_deref(),
            Some(&b"application/octet-stream"[..])
        );
        assert_eq!(f.size, 7);
        assert_eq!(std::fs::read(&f.file.path).unwrap(), b"PAYLOAD");
        assert!(
            f.headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("content-disposition"))
        );
    }

    #[test]
    fn lf_only_lines_preamble_and_padding_are_tolerated() {
        let body = b"preamble to ignore\n--B \t\ncontent-disposition: form-data; name=x\n\nv\n--B--\nepilogue";
        let mb = ok(body);
        assert_eq!(mb.fields.len(), 1);
        assert_eq!(mb.fields[0].value, b"v");
    }

    #[test]
    fn quoted_boundary_and_charset_field_parse() {
        let ct = b"multipart/form-data; boundary=\"B\"";
        assert_eq!(boundary(ct).unwrap(), b"B");
        let body =
            b"--B\r\ncontent-disposition: form-data; name=\"_charset_\"\r\n\r\nutf-8\r\n--B--";
        let mb = ok(body);
        assert_eq!(mb.fields[0].name, b"_charset_");
    }

    #[test]
    fn zero_part_body_is_a_valid_empty_multipart() {
        let mb = ok(b"--B--");
        assert!(mb.fields.is_empty() && mb.files.is_empty());
    }

    /// A close-delimiter is `--boundary--` plus padding and a line ending or end of body, RFC 2046 §5.1.1.
    #[test]
    fn close_dashes_with_trailing_junk_are_content_not_close() {
        let body = b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nbefore\r\n--B--junk\r\nafter\r\n--B--";
        let mb = ok(body);
        assert_eq!(mb.fields.len(), 1);
        assert_eq!(mb.fields[0].value, b"before\r\n--B--junk\r\nafter");
    }

    #[test]
    fn close_delimiter_padding_and_epilogue_forms_stay_accepted() {
        let field = b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n";
        for close in [
            &b"--B-- \t"[..],
            b"--B--\r\nepilogue",
            b"--B-- \t\r\nepilogue",
        ] {
            let body = [field.as_slice(), close].concat();
            let mb = ok(&body);
            assert_eq!(mb.fields[0].value, b"v", "close form: {close:?}");
        }
    }

    #[test]
    fn empty_filename_is_a_file_part_and_empty_name_is_a_400() {
        let mb =
            ok(b"--B\r\ncontent-disposition: form-data; name=f; filename=\"\"\r\n\r\n\r\n--B--");
        assert_eq!(mb.files.len(), 1);
        assert_eq!(mb.files[0].client_filename, b"");

        let status = rejected(
            b"--B\r\ncontent-disposition: form-data; name=\"\"\r\n\r\nv\r\n--B--",
            &limits(),
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn malformed_bodies_reject_with_400() {
        let l = limits();
        for body in [
            &b"no boundary anywhere"[..],
            b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nunclosed",
            b"--B\r\nno-disposition: here\r\n\r\nv\r\n--B--",
            b"--B\r\ncontent-disposition: form-data; name=a\r\ncontent-disposition: form-data; name=b\r\n\r\nv\r\n--B--",
            b"--B\r\ncontent-disposition: form-data; name=a; name=b\r\n\r\nv\r\n--B--",
            b"--B\r\nheaderwithoutseparator",
        ] {
            assert_eq!(rejected(body, &l), 400, "body: {body:?}");
        }
        assert!(matches!(
            boundary(b"multipart/form-data"),
            Err(ParseError::Rejected { status: 400, .. })
        ));
        assert!(matches!(
            boundary(b"multipart/form-data; boundary="),
            Err(ParseError::Rejected { status: 400, .. })
        ));
    }

    #[test]
    fn limits_reject_with_413() {
        let mut l = limits();
        l.max_field_size = 2;
        assert_eq!(
            rejected(
                b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\ntoolong\r\n--B--",
                &l
            ),
            413
        );
        let mut l = limits();
        l.max_file_size = 2;
        assert_eq!(
            rejected(
                b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\ntoolong\r\n--B--",
                &l
            ),
            413
        );
        let mut l = limits();
        l.max_parts = 1;
        assert_eq!(
            rejected(
                b"--B\r\ncontent-disposition: form-data; name=a\r\n\r\n1\r\n--B\r\ncontent-disposition: form-data; name=b\r\n\r\n2\r\n--B--",
                &l
            ),
            413
        );
        let mut l = limits();
        l.max_files = 0;
        assert_eq!(
            rejected(
                b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\nx\r\n--B--",
                &l
            ),
            413
        );
    }

    /// Uses its own spool dir: scanning the shared system temp dir races sibling tests.
    #[test]
    fn spooled_files_unlink_on_a_later_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = limits();
        l.dir = dir.path().to_path_buf();
        l.max_field_size = 1;
        let body = b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\nDATA\r\n\
--B\r\ncontent-disposition: form-data; name=x\r\n\r\ntoolong\r\n--B--";
        assert_eq!(rejected(body, &l), 413);
        let leaked: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert!(leaked.is_empty(), "leaked spool files: {leaked:?}");
    }

    #[test]
    fn the_multipart_trigger_is_exact() {
        assert!(is_multipart(b"multipart/form-data"));
        assert!(is_multipart(b"MULTIPART/FORM-DATA"));
        assert!(is_multipart(b"multipart/form-data ; boundary=x"));
        assert!(!is_multipart(b"multipart/form-data-foo"));
        assert!(!is_multipart(b"multipart/mixed; boundary=x"));
        assert!(!is_multipart(b"text/plain"));
    }

    #[test]
    fn non_utf8_boundary_bytes_round_trip() {
        let b = boundary(b"multipart/form-data; boundary=RAP\xff\xfeIRA").unwrap();
        assert_eq!(b, b"RAP\xff\xfeIRA");
        let mut body = Vec::new();
        body.extend_from_slice(b"--RAP\xff\xfeIRA\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--RAP\xff\xfeIRA--");
        let mb = parse(&body, &b, &limits()).map_err(|_| ()).expect("parses");
        assert_eq!(mb.fields[0].value, b"v");
    }
}
