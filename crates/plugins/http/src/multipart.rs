//! The multipart/form-data parser of dispatcher mode and the spool dirs of the workers. https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1

use std::io::Write;
use std::path::{Path, PathBuf};

use hyper::body::Bytes;
use memchr::memmem;

use crate::check::Rejection;

/// `unlink` is the one remover: seal calls it at finalize; Drop removes a file that a bailout (fatal or timeout) leaves behind.
pub struct SpooledFile {
    pub path: PathBuf,
    /// Set by the first `unlink`: a recycled file name is never removed twice.
    unlinked: bool,
}

impl SpooledFile {
    /// Removes the file once; a later call and the Drop do nothing.
    pub fn unlink(&mut self) {
        if std::mem::replace(&mut self.unlinked, true) {
            return;
        }
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(target: "rapira", "removing spool file {}: {e}", self.path.display());
        }
    }
}

impl Drop for SpooledFile {
    fn drop(&mut self) {
        self.unlink();
    }
}

/// The field lines of one name in a part header section, in field line order.
#[derive(Debug, PartialEq)]
pub struct HeaderValues {
    pub name: String,
    pub values: Vec<Vec<u8>>,
}

pub struct FormField {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
    /// One entry per name in first-seen order.
    pub headers: Vec<HeaderValues>,
}

pub struct UploadedFile {
    pub name: Vec<u8>,
    pub client_filename: Vec<u8>,
    /// content-type verbatim; an empty or OWS-only value maps to None upstream.
    pub client_media_type: Option<Vec<u8>>,
    /// One entry per name in first-seen order.
    pub headers: Vec<HeaderValues>,
    pub file: SpooledFile,
    /// Bytes written to disk; a 64-bit zend_long is assumed at the FFI edge.
    pub size: u64,
}

pub struct MultipartBody {
    pub fields: Vec<FormField>,
    pub files: Vec<UploadedFile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub dir: PathBuf,
    pub max_file_size: u64,
    pub max_field_size: usize,
    pub max_files: usize,
    pub max_parts: usize,
    pub max_part_headers: usize,
}

/// Creates the spool dir of this worker process under `base`, owner-only. The master at boot and each worker at its start sweep the dirs whose process is gone.
pub(crate) fn create_worker_spool_dir(base: &Path) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = base.join(format!("rapira-spool-{}", std::process::id()));
    // A dir with this pid is left by a killed worker that had the same pid.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("creating spool dir {}: {e}", dir.display()))?;
    // systemd-tmpfiles does not age a dir with a BSD lock. The fd stays open until the worker exits: https://www.freedesktop.org/software/systemd/man/latest/tmpfiles.d.html#Age
    if let Ok(lock) = std::fs::File::open(&dir)
        && lock.lock_shared().is_ok()
    {
        std::mem::forget(lock);
    }
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

/// Case-insensitive, quoted form unquoted, unquoted form terminated by `,`, as php-src does: https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L707-L751
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
        return Ok(val.to_vec());
    }
    Err(bad("multipart/form-data without a boundary parameter"))
}

/// The body parts as one byte sequence. Every method takes global byte indices.
struct View<'a> {
    parts: &'a [Bytes],
    /// The global index of the first byte of each part.
    starts: Vec<usize>,
    len: usize,
}

impl<'a> View<'a> {
    fn new(parts: &'a [Bytes]) -> Self {
        let mut starts = Vec::with_capacity(parts.len());
        let mut len = 0;
        for part in parts {
            starts.push(len);
            len += part.len();
        }
        Self { parts, starts, len }
    }

    /// The bytes from i towards b that the part of i holds. i is less than b, and b is at most len.
    fn run(&self, i: usize, b: usize) -> &'a [u8] {
        // The last part that starts at or before i holds i. An empty part starts at the index of the next part, so it is never that last part.
        let k = self.starts.partition_point(|&s| s <= i) - 1;
        let local = i - self.starts[k];
        let part = &self.parts[k];
        &part[local..part.len().min(local + b - i)]
    }

    /// The first match at or after from. A match inside a part comes before a match that starts in that part and ends after it.
    fn find(&self, finder: &memmem::Finder<'_>, from: usize) -> Option<usize> {
        let reach = finder.needle().len() - 1;
        let mut i = from;
        while i < self.len {
            let run = self.run(i, self.len);
            if let Some(o) = finder.find(run) {
                return Some(i + o);
            }
            let end = i + run.len();
            // Each match in this window starts before end and ends after it.
            let a = i.max(end.saturating_sub(reach));
            let window = self.copy(a, self.len.min(end + reach));
            if let Some(o) = finder.find(&window) {
                return Some(a + o);
            }
            i = end;
        }
        None
    }

    fn memchr(&self, b: u8, from: usize) -> Option<usize> {
        let mut i = from;
        while i < self.len {
            let run = self.run(i, self.len);
            if let Some(o) = memchr::memchr(b, run) {
                return Some(i + o);
            }
            i += run.len();
        }
        None
    }

    fn get(&self, i: usize) -> Option<u8> {
        (i < self.len).then(|| self.run(i, i + 1)[0])
    }

    fn starts_with(&self, i: usize, needle: &[u8]) -> bool {
        (i..).zip(needle).all(|(at, &b)| self.get(at) == Some(b))
    }

    fn copy(&self, a: usize, b: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(b - a);
        let mut i = a;
        while i < b {
            let run = self.run(i, b);
            out.extend_from_slice(run);
            i += run.len();
        }
        out
    }

    fn write_to(&self, file: &mut std::fs::File, a: usize, b: usize) -> std::io::Result<()> {
        let mut i = a;
        while i < b {
            let run = self.run(i, b);
            file.write_all(run)?;
            i += run.len();
        }
        Ok(())
    }
}

struct DelimHit {
    line_start: usize,
    after: usize,
    close: bool,
}

/// A delimiter must start a line; a line merely beginning with the boundary bytes is content, so the scan continues at the next line. The scan is linear in the body: a run of boundary bytes inside a line is skipped in one step.
fn next_delimiter(
    view: &View<'_>,
    mut from: usize,
    finder: &memmem::Finder<'_>,
    dlen: usize,
) -> Option<DelimHit> {
    while let Some(i) = view.find(finder, from) {
        if i == 0 || view.get(i - 1) == Some(b'\n') {
            let mut j = i + dlen;
            if view.starts_with(j, b"--") {
                // after the dashes a close-delimiter allows only transport padding and a line ending or end of body, RFC 2046 §5.1.1
                let mut k = j + 2;
                while matches!(view.get(k), Some(b' ' | b'\t')) {
                    k += 1;
                }
                if matches!(view.get(k), None | Some(b'\n')) || view.starts_with(k, b"\r\n") {
                    return Some(DelimHit {
                        line_start: i,
                        after: j + 2,
                        close: true,
                    });
                }
            }
            while matches!(view.get(j), Some(b' ' | b'\t')) {
                j += 1;
            }
            match view.get(j) {
                Some(b'\n') => {
                    return Some(DelimHit {
                        line_start: i,
                        after: j + 1,
                        close: false,
                    });
                }
                Some(b'\r') if view.get(j + 1) == Some(b'\n') => {
                    return Some(DelimHit {
                        line_start: i,
                        after: j + 2,
                        close: false,
                    });
                }
                _ => {}
            }
        }
        from = view.memchr(b'\n', i)? + 1;
    }
    None
}

/// Parses a non-empty body from the parts that the transport delivered; the empty-body case lands on the contract's string arm (`$body === ''`) instead.
/// https://www.rfc-editor.org/rfc/rfc7578
pub fn parse(body: &[Bytes], boundary: &[u8], limits: &Limits) -> Result<MultipartBody, Rejection> {
    let view = View::new(body);
    let delim: Vec<u8> = [b"--".as_slice(), boundary].concat();
    let finder = memmem::Finder::new(&delim);

    let mut fields: Vec<FormField> = Vec::new();
    let mut files: Vec<UploadedFile> = Vec::new();

    let opening = next_delimiter(&view, 0, &finder, delim.len())
        .ok_or_else(|| bad("no opening boundary line"))?;
    if opening.close {
        return Ok(MultipartBody { fields, files });
    }
    let mut part_start = opening.after;

    loop {
        if fields.len() + files.len() + 1 > limits.max_parts {
            return Err(over("part count over max_parts"));
        }
        let ending = next_delimiter(&view, part_start, &finder, delim.len())
            .ok_or_else(|| bad("no closing boundary line"))?;
        // the line terminator preceding a delimiter line belongs to the delimiter, not the part, RFC 2046 §5.1.1
        let mut part_end = ending.line_start;
        if part_end > part_start && view.get(part_end - 1) == Some(b'\n') {
            part_end -= 1;
            if part_end > part_start && view.get(part_end - 1) == Some(b'\r') {
                part_end -= 1;
            }
        }
        parse_part(&view, part_start, part_end, limits, &mut fields, &mut files)?;

        if ending.close {
            return Ok(MultipartBody { fields, files });
        }
        part_start = ending.after;
    }
}

/// The name and filename parameters of a content-disposition value.
struct Disposition {
    name: Option<Vec<u8>>,
    filename: Option<Vec<u8>>,
}

/// Reads the name and filename parameters whatever the disposition type says, as php-src does: https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L804-L815
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
                    // php-src unescapes only \\ and \" and keeps every other '\': https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L495
                    // A '\' at the end of a value escapes the closing quote, as in php-src.
                    Some(b'\\') if matches!(v.get(k + 1), Some(b'\\' | b'"')) => {
                        out.push(v[k + 1]);
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
    Ok(Disposition { name, filename })
}

/// Spool file names are unique per process; the fuzz targets share one dir across processes, so the pid stays in the name.
static SPOOL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The SpooledFile wraps the path before the write, so a failed write unlinks the file on drop.
fn spool(view: &View<'_>, a: usize, b: usize, dir: &Path) -> std::io::Result<SpooledFile> {
    use std::os::unix::fs::OpenOptionsExt;
    let n = SPOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("{}-{n}", std::process::id()));
    let mut file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let spooled = SpooledFile {
        path,
        unlinked: false,
    };
    view.write_to(&mut file, a, b)?;
    Ok(spooled)
}

/// The part is the range from part_start to part_end of the view. A filename parameter, even an empty one, makes the part a file part.
fn parse_part(
    view: &View<'_>,
    part_start: usize,
    part_end: usize,
    limits: &Limits,
    fields: &mut Vec<FormField>,
    files: &mut Vec<UploadedFile>,
) -> Result<(), Rejection> {
    // The header section ends at the first empty line, CRLF or bare LF. A line starts at the part start or after a LF, as in httparse.
    let mut line = part_start;
    let section_end = loop {
        if line < part_end && view.get(line) == Some(b'\n') {
            break line + 1;
        }
        if line + 2 <= part_end && view.starts_with(line, b"\r\n") {
            break line + 2;
        }
        match view.memchr(b'\n', line) {
            Some(lf) if lf < part_end => line = lf + 1,
            _ => return Err(bad("part without a header/body separator")),
        }
    };
    let section = view.copy(part_start, section_end);
    let mut hbuf = vec![httparse::EMPTY_HEADER; limits.max_part_headers];
    let (content_start, parsed) = match httparse::parse_headers(&section, &mut hbuf) {
        // len includes the empty line
        Ok(httparse::Status::Complete((len, headers))) => (part_start + len, headers),
        Ok(httparse::Status::Partial) => return Err(bad("part without a header/body separator")),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(over("part headers over max_part_headers"));
        }
        // httparse enforces RFC 9110 token field names, stricter than rfc1867.c
        Err(_) => return Err(bad("unparseable part header section")),
    };

    let mut headers: Vec<HeaderValues> = Vec::with_capacity(parsed.len());
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
        match headers.iter_mut().find(|header| header.name == h.name) {
            Some(header) => header.values.push(h.value.to_vec()),
            None => headers.push(HeaderValues {
                name: h.name.to_owned(),
                values: vec![h.value.to_vec()],
            }),
        }
    }
    let Some(disposition) = disposition else {
        return Err(bad("part without content-disposition"));
    };

    let Disposition { name, filename } = disposition_params(disposition)?;
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(bad(
            "content-disposition without a non-empty name parameter",
        ));
    };
    // httparse trims the value already.
    let client_media_type = media_type.filter(|v| !v.is_empty()).map(<[u8]>::to_vec);

    let len = part_end - content_start;
    match filename {
        Some(client_filename) => {
            if files.len() + 1 > limits.max_files {
                return Err(over("file parts over max_files"));
            }
            if len as u64 > limits.max_file_size {
                return Err(over("file part over max_file_size"));
            }
            let file =
                spool(view, content_start, part_end, &limits.dir).map_err(|e| Rejection {
                    status: http::StatusCode::INTERNAL_SERVER_ERROR,
                    reason: format!("upload spool failed: {e}"),
                })?;
            files.push(UploadedFile {
                name,
                client_filename,
                client_media_type,
                headers,
                file,
                size: len as u64,
            });
        }
        None => {
            if len > limits.max_field_size {
                return Err(over("field part over max_field_size"));
            }
            fields.push(FormField {
                name,
                value: view.copy(content_start, part_end),
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
            max_part_headers: 32,
        }
    }

    fn ok(body: &[u8]) -> MultipartBody {
        parse(&[Bytes::copy_from_slice(body)], b"B", &limits())
            .unwrap_or_else(|_| panic!("expected a parse"))
    }

    /// The status of the rejection.
    fn rejected(body: &[u8], l: &Limits) -> u16 {
        match parse(&[Bytes::copy_from_slice(body)], b"B", l) {
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
        struct Case {
            name: &'static str,
            close: &'static [u8],
        }
        let cases = [
            Case {
                name: "padding at the end of the body",
                close: b"--B-- \t",
            },
            Case {
                name: "line ending and epilogue",
                close: b"--B--\r\nepilogue",
            },
            Case {
                name: "padding, line ending and epilogue",
                close: b"--B-- \t\r\nepilogue",
            },
        ];
        let field = b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n";
        for case in cases {
            let body = [field.as_slice(), case.close].concat();
            let mb = ok(&body);
            assert_eq!(mb.fields[0].value, b"v", "{}", case.name);
        }
    }

    /// The contract shape of FormField and UploadedFile `$headers`: one entry per name in first-seen order, the values in field line order, names compared byte for byte.
    #[test]
    fn part_headers_group_by_name() {
        struct Case {
            name: &'static str,
            body: &'static [u8],
            want: &'static [(&'static str, &'static [&'static [u8]])],
        }
        let cases = [
            Case {
                name: "a repeated name gives one entry with the values in line order",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\nx-a: 2\r\nx-a: 1\r\n\r\nv\r\n--B--",
                want: &[
                    ("content-disposition", &[b"form-data; name=x"]),
                    ("x-a", &[b"2", b"1"]),
                ],
            },
            Case {
                name: "two names keep first-seen order",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\nx-b: 1\r\nx-a: 2\r\nx-b: 3\r\n\r\nv\r\n--B--",
                want: &[
                    ("content-disposition", &[b"form-data; name=x"]),
                    ("x-b", &[b"1", b"3"]),
                    ("x-a", &[b"2"]),
                ],
            },
            Case {
                name: "two names that differ only in case stay two entries",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\nX-A: 1\r\nx-a: 2\r\n\r\nv\r\n--B--",
                want: &[
                    ("content-disposition", &[b"form-data; name=x"]),
                    ("X-A", &[b"1"]),
                    ("x-a", &[b"2"]),
                ],
            },
        ];
        for case in cases {
            let mut want = Vec::new();
            for (name, values) in case.want {
                want.push(HeaderValues {
                    name: (*name).to_owned(),
                    values: values.iter().map(|v| v.to_vec()).collect(),
                });
            }
            let mb = ok(case.body);
            assert_eq!(mb.fields[0].headers, want, "{}", case.name);
        }
    }

    #[test]
    fn malformed_bodies_reject_with_400() {
        struct Case {
            name: &'static str,
            body: &'static [u8],
        }
        let cases = [
            Case {
                name: "no opening boundary line",
                body: b"no boundary anywhere",
            },
            Case {
                name: "no closing boundary line",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nunclosed",
            },
            Case {
                name: "part without content-disposition",
                body: b"--B\r\nno-disposition: here\r\n\r\nv\r\n--B--",
            },
            Case {
                name: "duplicated content-disposition",
                body: b"--B\r\ncontent-disposition: form-data; name=a\r\ncontent-disposition: form-data; name=b\r\n\r\nv\r\n--B--",
            },
            Case {
                name: "duplicated name parameter",
                body: b"--B\r\ncontent-disposition: form-data; name=a; name=b\r\n\r\nv\r\n--B--",
            },
            Case {
                name: "part without a header/body separator",
                body: b"--B\r\nheaderwithoutseparator",
            },
        ];
        let l = limits();
        for case in cases {
            assert_eq!(rejected(case.body, &l), 400, "{}", case.name);
        }
    }

    #[test]
    fn missing_or_empty_boundary_rejects_with_400() {
        struct Case {
            name: &'static str,
            content_type: &'static [u8],
        }
        let cases = [
            Case {
                name: "no boundary parameter",
                content_type: b"multipart/form-data",
            },
            Case {
                name: "empty boundary parameter",
                content_type: b"multipart/form-data; boundary=",
            },
        ];
        for case in cases {
            let status = boundary(case.content_type)
                .map(|_| ())
                .map_err(|r| r.status.as_u16());
            assert_eq!(status, Err(400), "{}", case.name);
        }
    }

    #[test]
    fn limits_reject_with_413() {
        struct Case {
            name: &'static str,
            limits: Limits,
            body: &'static [u8],
        }
        let cases = [
            Case {
                name: "field part over max_field_size",
                limits: Limits {
                    max_field_size: 2,
                    ..limits()
                },
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\ntoolong\r\n--B--",
            },
            Case {
                name: "file part over max_file_size",
                limits: Limits {
                    max_file_size: 2,
                    ..limits()
                },
                body: b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\ntoolong\r\n--B--",
            },
            Case {
                name: "part count over max_parts",
                limits: Limits {
                    max_parts: 1,
                    ..limits()
                },
                body: b"--B\r\ncontent-disposition: form-data; name=a\r\n\r\n1\r\n--B\r\ncontent-disposition: form-data; name=b\r\n\r\n2\r\n--B--",
            },
            Case {
                name: "file parts over max_files",
                limits: Limits {
                    max_files: 1,
                    ..limits()
                },
                body: b"--B\r\ncontent-disposition: form-data; name=f; filename=a\r\n\r\nx\r\n--B\r\ncontent-disposition: form-data; name=g; filename=b\r\n\r\ny\r\n--B--",
            },
        ];
        for case in cases {
            assert_eq!(rejected(case.body, &case.limits), 413, "{}", case.name);
        }
    }

    #[test]
    fn the_multipart_trigger_is_exact() {
        struct Case {
            name: &'static str,
            content_type: &'static [u8],
            multipart: bool,
        }
        let cases = [
            Case {
                name: "exact media type",
                content_type: b"multipart/form-data",
                multipart: true,
            },
            Case {
                name: "upper case",
                content_type: b"MULTIPART/FORM-DATA",
                multipart: true,
            },
            Case {
                name: "space before the parameters",
                content_type: b"multipart/form-data ; boundary=x",
                multipart: true,
            },
            Case {
                name: "longer subtype",
                content_type: b"multipart/form-data-foo",
                multipart: false,
            },
            Case {
                name: "other multipart subtype",
                content_type: b"multipart/mixed; boundary=x",
                multipart: false,
            },
            Case {
                name: "other media type",
                content_type: b"text/plain",
                multipart: false,
            },
        ];
        for case in cases {
            assert_eq!(
                is_multipart(case.content_type),
                case.multipart,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn non_utf8_boundary_bytes_round_trip() {
        let b = boundary(b"multipart/form-data; boundary=RAP\xff\xfeIRA").unwrap();
        assert_eq!(b, b"RAP\xff\xfeIRA");
        let mut body = Vec::new();
        body.extend_from_slice(b"--RAP\xff\xfeIRA\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--RAP\xff\xfeIRA--");
        let mb = parse(&[Bytes::from(body)], &b, &limits())
            .map_err(|_| ())
            .expect("parses");
        assert_eq!(mb.fields[0].value, b"v");
    }

    /// Compares two parse results field by field, with the bytes of each spooled file.
    fn assert_same(
        case: &str,
        want: &Result<MultipartBody, Rejection>,
        got: &Result<MultipartBody, Rejection>,
    ) {
        match (want, got) {
            (Ok(want), Ok(got)) => {
                assert_eq!(want.fields.len(), got.fields.len(), "{case}: field count");
                for (w, g) in want.fields.iter().zip(&got.fields) {
                    assert_eq!(
                        (&w.name, &w.value, &w.headers),
                        (&g.name, &g.value, &g.headers),
                        "{case}: field"
                    );
                }
                assert_eq!(want.files.len(), got.files.len(), "{case}: file count");
                for (w, g) in want.files.iter().zip(&got.files) {
                    assert_eq!(
                        (&w.name, &w.client_filename, &w.client_media_type),
                        (&g.name, &g.client_filename, &g.client_media_type),
                        "{case}: file"
                    );
                    assert_eq!(
                        (&w.headers, w.size),
                        (&g.headers, g.size),
                        "{case}: file headers and size"
                    );
                    let spooled = |f: &UploadedFile| std::fs::read(&f.file.path).expect("spooled");
                    assert_eq!(spooled(w), spooled(g), "{case}: spooled bytes");
                }
            }
            (Err(w), Err(g)) => assert_eq!(
                (w.status, &w.reason),
                (g.status, &g.reason),
                "{case}: rejection"
            ),
            _ => panic!("{case}: one parse is Ok and the other is a rejection"),
        }
    }

    /// Each split of a body gives the result of the one-part body: at every position, and into single-byte parts.
    #[test]
    fn split_bodies_parse_the_same() {
        struct Case {
            name: &'static str,
            body: &'static [u8],
        }
        let cases = [
            Case {
                name: "two fields",
                body: b"--B\r\ncontent-disposition: form-data; name=a\r\n\r\none\r\n--B\r\ncontent-disposition: form-data; name=b\r\n\r\ntwo\r\n--B--",
            },
            Case {
                name: "a field and a file",
                body: b"--B\r\ncontent-disposition: form-data; name=a\r\n\r\none\r\n--B\r\ncontent-disposition: form-data; name=f; filename=a.bin\r\ncontent-type: application/octet-stream\r\n\r\nPAYLOAD\r\n--B--\r\n",
            },
            Case {
                name: "LF-only lines, preamble and padding",
                body: b"preamble to ignore\n--B \t\ncontent-disposition: form-data; name=x\n\nv\n--B--\nepilogue",
            },
            Case {
                name: "close padding at the end of the body",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--B-- \t",
            },
            Case {
                name: "close line ending and epilogue",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--B--\r\nepilogue",
            },
            Case {
                name: "close padding, line ending and epilogue",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--B-- \t\r\nepilogue",
            },
            Case {
                name: "boundary-like line inside a value",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nbefore\r\n--B--junk\r\nafter\r\n--B--",
            },
            Case {
                name: "part without a header/body separator",
                body: b"--B\r\nheaderwithoutseparator\r\n--B--",
            },
        ];
        let l = limits();
        for case in cases {
            let whole = parse(&[Bytes::from_static(case.body)], b"B", &l);
            for at in 1..case.body.len() {
                let (head, tail) = case.body.split_at(at);
                let split = [Bytes::from_static(head), Bytes::from_static(tail)];
                let name = format!("{} split at {at}", case.name);
                assert_same(&name, &whole, &parse(&split, b"B", &l));
            }
            let single: Vec<Bytes> = case.body.chunks(1).map(Bytes::from_static).collect();
            let name = format!("{} in single bytes", case.name);
            assert_same(&name, &whole, &parse(&single, b"B", &l));
        }
    }
}
