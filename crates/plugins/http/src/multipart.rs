// https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1

use std::io::Write;
use std::path::{Path, PathBuf};

use memchr::memmem;
use rapira_sapi::types::{FormField, MultipartBody, SpooledFile, UploadedFile};

use crate::check::Rejection;

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub dir: PathBuf,
    pub max_file_size: u64,
    pub max_field_size: usize,
    pub max_files: usize,
    pub max_parts: usize,
}

/// Header fields allowed in one part; the httparse buffer size.
pub const MAX_PART_HEADERS: usize = 32;

/// The spool dir of this worker process under `base`. The master sweeps the dirs whose process is gone.
fn worker_spool_dir(base: &Path) -> PathBuf {
    base.join(format!("rapira-spool-{}", std::process::id()))
}

/// Creates the spool dir of this worker process under `base`, owner-only.
pub(crate) fn create_worker_spool_dir(base: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = worker_spool_dir(base);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("creating spool dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// kill(pid, 0) probes existence without signaling: ESRCH means the owner is gone, EPERM means it runs under another uid. https://man7.org/linux/man-pages/man2/kill.2.html
fn spool_dir_reclaimable(name: &str) -> bool {
    let Some(pid) = name
        .strip_prefix("rapira-spool-")
        .and_then(|p| p.parse::<i32>().ok())
        .filter(|&p| p > 0)
    else {
        return false;
    };
    let gone = unsafe { libc::kill(pid, 0) } == -1;
    gone && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

/// Removes the spool dirs under `base` whose process is gone.
pub(crate) fn sweep_spool_dirs(base: &Path) {
    match std::fs::read_dir(base) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if !spool_dir_reclaimable(&entry.file_name().to_string_lossy()) {
                    continue;
                }
                let path = entry.path();
                if let Err(e) = std::fs::remove_dir_all(&path) {
                    tracing::warn!(target: "rapira", "sweeping spool dir {}: {e}", path.display());
                }
            }
        }
        Err(e) => {
            tracing::warn!(target: "rapira", "listing {} for the spool sweep: {e}", base.display());
        }
    }
}

fn bad(reason: impl Into<String>) -> Rejection {
    Rejection {
        status: http::StatusCode::BAD_REQUEST,
        reason: reason.into(),
    }
}

fn over(reason: impl Into<String>) -> Rejection {
    Rejection {
        status: http::StatusCode::PAYLOAD_TOO_LARGE,
        reason: reason.into(),
    }
}

pub fn is_multipart(content_type: &[u8]) -> bool {
    let media = content_type.split(|&b| b == b';').next().unwrap_or(b"");
    media
        .trim_ascii()
        .eq_ignore_ascii_case(b"multipart/form-data")
}

/// Case-insensitive, quoted form unquoted, unquoted form terminated by `,` (php-src rfc1867.c:707-751), capped at the RFC 2046 §5.1.1 70 characters.
pub fn boundary(content_type: &[u8]) -> Result<Vec<u8>, Rejection> {
    for seg in content_type.split(|&b| b == b';').skip(1) {
        let Some(eq) = memchr::memchr(b'=', seg) else {
            continue;
        };
        if !seg[..eq].trim_ascii().eq_ignore_ascii_case(b"boundary") {
            continue;
        }
        let val = seg[eq + 1..].trim_ascii();
        let val = if let [b'"', inner @ ..] = val {
            match memchr::memchr(b'"', inner) {
                Some(end) => &inner[..end],
                None => return Err(bad("unterminated quoted boundary parameter")),
            }
        } else {
            match memchr::memchr(b',', val) {
                Some(end) => val[..end].trim_ascii(),
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
pub fn parse(body: &[u8], boundary: &[u8], limits: &Limits) -> Result<MultipartBody, Rejection> {
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

/// (name, filename) from a content-disposition value.
type Disposition = (Option<Vec<u8>>, Option<Vec<u8>>);

/// The disposition type token is not enforced: php-src rfc1867.c reads the parameters regardless.
fn disposition_params(v: &[u8]) -> Result<Disposition, Rejection> {
    let mut name: Option<Vec<u8>> = None;
    let mut filename: Option<Vec<u8>> = None;
    let mut i = memchr::memchr(b';', v).map(|i| i + 1).unwrap_or(v.len());
    while i < v.len() {
        let Some(eq) = memchr::memchr(b'=', &v[i..]).map(|o| o + i) else {
            break;
        };
        let key = v[i..eq].trim_ascii();
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
            (v[j..end].trim_ascii().to_vec(), end)
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

/// Spool file names are unique per process; the fuzz targets share one dir across processes, so the pid stays in the name.
static SPOOL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The SpooledFile wraps the path before the write, so a failed write unlinks the file on drop.
fn spool(bytes: &[u8], dir: &Path) -> std::io::Result<SpooledFile> {
    use std::os::unix::fs::OpenOptionsExt;
    let n = SPOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("{}-{n}", std::process::id()));
    let mut file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let spooled = SpooledFile { path };
    file.write_all(bytes)?;
    Ok(spooled)
}

/// A filename parameter, even an empty one, makes the part a file part.
fn parse_part(
    part: &[u8],
    limits: &Limits,
    fields: &mut Vec<FormField>,
    files: &mut Vec<UploadedFile>,
) -> Result<(), Rejection> {
    let mut hbuf = [httparse::EMPTY_HEADER; MAX_PART_HEADERS];
    let (body, parsed) = match httparse::parse_headers(part, &mut hbuf) {
        // the header section ends at the first empty line, CRLF or bare LF; len includes that line
        Ok(httparse::Status::Complete((len, headers))) => (&part[len..], headers),
        Ok(httparse::Status::Partial) => return Err(bad("part without a header/body separator")),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(over(format!(
                "part with more than {MAX_PART_HEADERS} header fields"
            )));
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
        .map(<[u8]>::trim_ascii)
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
            let file = spool(body, &limits.dir).map_err(|e| Rejection {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                reason: format!("upload spool failed: {e}"),
            })?;
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
        Limits {
            dir: std::env::temp_dir(),
            max_file_size: 2 * 1024 * 1024,
            max_field_size: 256 * 1024,
            max_files: 20,
            max_parts: 1024,
        }
    }

    fn ok(body: &[u8]) -> MultipartBody {
        parse(body, b"B", &limits()).unwrap_or_else(|_| panic!("expected a parse"))
    }

    /// The status of the rejection.
    fn rejected(body: &[u8], l: &Limits) -> u16 {
        match parse(body, b"B", l) {
            Err(Rejection { status, .. }) => status.as_u16(),
            Ok(_) => panic!("expected a rejection"),
        }
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
            Err(Rejection { status, .. }) if status == 400
        ));
        assert!(matches!(
            boundary(b"multipart/form-data; boundary="),
            Err(Rejection { status, .. }) if status == 400
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
