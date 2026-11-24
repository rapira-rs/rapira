//! The multipart/form-data parser for dispatcher mode and the process spool directory. https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1

use std::io::Write;
use std::path::{Path, PathBuf};

use memchr::memmem;

use crate::check::Rejection;

/// `unlink` is the only remover. seal calls it at finalize, and Drop removes a file that a bailout after a fatal error or a timeout leaves behind.
pub struct SpooledFile {
    pub path: PathBuf,
    /// The first `unlink` sets it, so a reused file name is never removed twice.
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
    /// The content-type value, verbatim. The parser maps an empty or OWS-only value to None.
    pub client_media_type: Option<Vec<u8>>,
    /// One entry per name in first-seen order.
    pub headers: Vec<HeaderValues>,
    pub file: SpooledFile,
    /// The number of bytes written to disk. The FFI edge assumes a 64-bit zend_long.
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

pub(crate) struct SpoolDir {
    dir: PathBuf,
    _lock: Option<std::fs::File>,
}

impl SpoolDir {
    /// Removes stale directories and sets `limits.dir` to the new process spool directory.
    pub(crate) fn prepare(limits: &mut Limits) -> anyhow::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        sweep_spool_dirs(&limits.dir);
        let dir = limits
            .dir
            .join(format!("rapira-spool-{}", std::process::id()));
        // The OS can reuse the pid of a process that left its directory behind.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| anyhow::anyhow!("creating spool dir {}: {e}", dir.display()))?;
        // A shared lock keeps systemd-tmpfiles from removing the directory. https://www.freedesktop.org/software/systemd/man/latest/tmpfiles.d.html#Age
        let lock = std::fs::File::open(&dir)
            .ok()
            .filter(|f| f.lock_shared().is_ok());
        limits.dir.clone_from(&dir);
        Ok(Self { dir, _lock: lock })
    }
}

impl Drop for SpoolDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            tracing::warn!(target: "rapira", "removing spool dir {}: {e}", self.dir.display());
        }
    }
}

/// kill(pid, 0) checks that the process exists and sends no signal. ESRCH means that the owner is gone, and EPERM means that it runs under another uid. https://man7.org/linux/man-pages/man2/kill.2.html
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
fn sweep_spool_dirs(base: &Path) {
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

/// Reads the boundary parameter as php-src does. The parameter name is case-insensitive. A quoted value loses its quotes, and a `,` ends an unquoted value. https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L707-L751
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

/// The tail of a delimiter line after the delimiter bytes: the two dashes of a close-delimiter, transport padding and the line ending (RFC 2046 §5.1.1).
#[derive(Clone, Copy, Default)]
struct Tail {
    dashes: u8,
    padded: bool,
    cr: bool,
}

/// The result of one tail byte.
enum Step {
    More,
    /// The byte ends a delimiter line.
    Delimiter,
    /// The line is content.
    Content,
}

impl Tail {
    fn step(&mut self, byte: u8) -> Step {
        if self.cr {
            return if byte == b'\n' {
                Step::Delimiter
            } else {
                Step::Content
            };
        }
        match byte {
            b'-' if !self.padded && self.dashes < 2 => self.dashes += 1,
            // One dash is not a close-delimiter.
            _ if self.dashes == 1 => return Step::Content,
            b' ' | b'\t' => self.padded = true,
            b'\r' => self.cr = true,
            b'\n' => return Step::Delimiter,
            _ => return Step::Content,
        }
        Step::More
    }

    fn is_close(self) -> bool {
        self.dashes == 2
    }
}

#[derive(Clone, Copy)]
enum State {
    /// In the header section of a part.
    Headers,
    /// Inside a line of the preamble or of a part content.
    Content,
    /// After a CR that can start the line ending before a delimiter line.
    Cr,
    /// At a line start, after this number of delimiter bytes.
    LineStart(usize),
    /// After the delimiter bytes of a line.
    Tail(Tail),
    /// After the close-delimiter. The parser ignores the epilogue.
    Done,
}

/// The target of the content bytes.
enum Sink {
    /// The preamble, or no open part.
    None,
    /// The part has this error. The parser reports it at the delimiter line that ends the part.
    Skip(Rejection),
    Field(FormField),
    File {
        file: UploadedFile,
        out: std::fs::File,
    },
}

/// Parses a body while the transport delivers it. `feed` takes the body bytes in order, in pieces of any size, and `finish` ends the body. A file part goes to its spool file piece by piece, so the parser never holds the body.
/// https://www.rfc-editor.org/rfc/rfc7578
pub struct Parser<'a> {
    limits: &'a Limits,
    /// Finds `--boundary`.
    finder: memmem::Finder<'static>,
    state: State,
    sink: Sink,
    /// The bytes of a possible delimiter line so far: the line ending before it, the delimiter bytes and the tail. They are content when the line is not a delimiter line.
    held: Vec<u8>,
    /// The header section of the current part so far.
    head: Vec<u8>,
    /// The start of the current line in `head`.
    line: usize,
    /// The opening delimiter line is complete.
    opened: bool,
    fields: Vec<FormField>,
    files: Vec<UploadedFile>,
}

impl<'a> Parser<'a> {
    pub fn new(boundary: &[u8], limits: &'a Limits) -> Self {
        let delimiter = [b"--".as_slice(), boundary].concat();
        Self {
            limits,
            finder: memmem::Finder::new(&delimiter).into_owned(),
            // The body starts at a line start.
            state: State::LineStart(0),
            sink: Sink::None,
            held: Vec::new(),
            head: Vec::new(),
            line: 0,
            opened: false,
            fields: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Takes the next bytes of the body. After an error the parser takes no more bytes.
    pub fn feed(&mut self, mut bytes: &[u8]) -> Result<(), Rejection> {
        while !bytes.is_empty() {
            bytes = match self.state {
                State::Headers => self.headers(bytes)?,
                State::Content => self.content(bytes),
                State::Cr => self.after_cr(bytes),
                State::LineStart(matched) => self.line_start(bytes, matched),
                State::Tail(tail) => self.tail(bytes, tail)?,
                State::Done => return Ok(()),
            };
        }
        Ok(())
    }

    /// Ends the body. A close-delimiter can end the body without a line ending.
    pub fn finish(mut self) -> Result<MultipartBody, Rejection> {
        match self.state {
            State::Done => {}
            State::Tail(tail) if tail.is_close() && !tail.cr => self.end_part(true)?,
            State::Headers if self.is_delimiter_line(&self.head[self.line..], true) => {
                return Err(bad("part without a header/body separator"));
            }
            _ if self.opened => return Err(bad("no closing boundary line")),
            _ => return Err(bad("no opening boundary line")),
        }
        Ok(MultipartBody {
            fields: self.fields,
            files: self.files,
        })
    }

    /// Takes header lines up to the empty line, CRLF or bare LF. A line starts at the part start or after a LF, as in httparse. A delimiter line before the empty line ends a part that has no header/body separator.
    fn headers<'b>(&mut self, bytes: &'b [u8]) -> Result<&'b [u8], Rejection> {
        let mut from = 0;
        while let Some(lf) = memchr::memchr(b'\n', &bytes[from..]) {
            let end = from + lf + 1;
            self.head.extend_from_slice(&bytes[from..end]);
            from = end;
            let line = &self.head[self.line..];
            if matches!(line, b"\n" | b"\r\n") {
                self.sink = self.open_part().unwrap_or_else(Sink::Skip);
                // The content starts at a line start.
                self.state = State::LineStart(0);
                return Ok(&bytes[from..]);
            }
            if self.is_delimiter_line(line, false) {
                return Err(bad("part without a header/body separator"));
            }
            self.line = self.head.len();
        }
        self.head.extend_from_slice(&bytes[from..]);
        Ok(&[])
    }

    /// `at_end` is true for the last line of a body, which has no line ending.
    fn is_delimiter_line(&self, line: &[u8], at_end: bool) -> bool {
        let Some(rest) = line.strip_prefix(self.finder.needle()) else {
            return false;
        };
        let mut tail = Tail::default();
        for &byte in rest {
            match tail.step(byte) {
                Step::More => {}
                Step::Delimiter => return true,
                Step::Content => return false,
            }
        }
        at_end && tail.is_close() && !tail.cr
    }

    /// Scans bytes that start inside a line. A delimiter must start a line. A line that only begins with the delimiter bytes is content, so the scan continues at the next line. The scan is linear in the body, because it skips a run of delimiter bytes inside a line in one step.
    fn content<'b>(&mut self, bytes: &'b [u8]) -> &'b [u8] {
        let delimiter = self.finder.needle().len();
        let mut from = 0;
        while let Some(found) = self.finder.find(&bytes[from..]) {
            let at = from + found;
            if at > 0 && bytes[at - 1] == b'\n' {
                // The line terminator before a delimiter line belongs to the delimiter, not to the part (RFC 2046 §5.1.1).
                let mut end = at - 1;
                if end > 0 && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                self.emit(&bytes[..end]);
                self.held.extend_from_slice(&bytes[end..at + delimiter]);
                self.state = State::Tail(Tail::default());
                return &bytes[at + delimiter..];
            }
            match memchr::memchr(b'\n', &bytes[at..]) {
                Some(lf) => from = at + lf,
                None => break,
            }
        }
        // The last bytes can start a delimiter line: a line ending with delimiter bytes after it, or a CR.
        let window = bytes.len().saturating_sub(delimiter);
        let keep = match memchr::memrchr(b'\n', &bytes[window..]) {
            Some(lf) if self.finder.needle().starts_with(&bytes[window + lf + 1..]) => {
                let lf = window + lf;
                let start = if lf > 0 && bytes[lf - 1] == b'\r' {
                    lf - 1
                } else {
                    lf
                };
                Some((start, State::LineStart(bytes.len() - lf - 1)))
            }
            _ if bytes.last() == Some(&b'\r') => Some((bytes.len() - 1, State::Cr)),
            _ => None,
        };
        match keep {
            Some((start, state)) => {
                self.emit(&bytes[..start]);
                self.held.extend_from_slice(&bytes[start..]);
                self.state = state;
            }
            None => self.emit(bytes),
        }
        &[]
    }

    fn after_cr<'b>(&mut self, bytes: &'b [u8]) -> &'b [u8] {
        if bytes[0] != b'\n' {
            self.release();
            return bytes;
        }
        self.held.push(b'\n');
        self.state = State::LineStart(0);
        &bytes[1..]
    }

    fn line_start<'b>(&mut self, bytes: &'b [u8], matched: usize) -> &'b [u8] {
        let want = &self.finder.needle()[matched..];
        let len = want.len().min(bytes.len());
        if bytes[..len] != want[..len] {
            self.release();
            return bytes;
        }
        self.state = if len == want.len() {
            State::Tail(Tail::default())
        } else {
            State::LineStart(matched + len)
        };
        self.held.extend_from_slice(&bytes[..len]);
        &bytes[len..]
    }

    fn tail<'b>(&mut self, bytes: &'b [u8], mut tail: Tail) -> Result<&'b [u8], Rejection> {
        for (i, &byte) in bytes.iter().enumerate() {
            match tail.step(byte) {
                Step::More => {}
                Step::Delimiter => {
                    self.end_part(tail.is_close())?;
                    return Ok(&bytes[i + 1..]);
                }
                Step::Content => {
                    self.release();
                    self.emit(&bytes[..i]);
                    return Ok(&bytes[i..]);
                }
            }
        }
        self.held.extend_from_slice(bytes);
        self.state = State::Tail(tail);
        Ok(&[])
    }

    /// The held bytes are content, and the next byte is inside a line.
    fn release(&mut self) {
        let held = std::mem::take(&mut self.held);
        self.emit(&held);
        self.held = held;
        self.held.clear();
        self.state = State::Content;
    }

    /// Gives content bytes to the current part.
    fn emit(&mut self, bytes: &[u8]) {
        let error = match &mut self.sink {
            Sink::None | Sink::Skip(_) => return,
            Sink::Field(field) => {
                if field.value.len() + bytes.len() <= self.limits.max_field_size {
                    field.value.extend_from_slice(bytes);
                    return;
                }
                over("field part over max_field_size")
            }
            Sink::File { file, out } => {
                if file.size + bytes.len() as u64 > self.limits.max_file_size {
                    over("file part over max_file_size")
                } else if let Err(e) = out.write_all(bytes) {
                    spool_failed(e)
                } else {
                    file.size += bytes.len() as u64;
                    return;
                }
            }
        };
        // The drop of a file part removes its spool file.
        self.sink = Sink::Skip(error);
    }

    /// Ends the preamble or the current part at a delimiter line.
    fn end_part(&mut self, close: bool) -> Result<(), Rejection> {
        // The delimiter line has no line ending before it when it follows the empty line of the header section.
        let bare = self.held.first() == Some(&b'-');
        self.held.clear();
        match std::mem::replace(&mut self.sink, Sink::None) {
            Sink::None => {}
            _ if bare => return Err(bad("part without a header/body separator")),
            Sink::Skip(rejection) => return Err(rejection),
            Sink::Field(field) => self.fields.push(field),
            Sink::File { file, .. } => self.files.push(file),
        }
        self.opened = true;
        if close {
            self.state = State::Done;
            return Ok(());
        }
        if self.fields.len() + self.files.len() + 1 > self.limits.max_parts {
            return Err(over("part count over max_parts"));
        }
        self.head.clear();
        self.line = 0;
        self.state = State::Headers;
        Ok(())
    }

    /// Reads the header section and opens the field or the file of the part. A filename parameter, even an empty one, makes the part a file part.
    fn open_part(&self) -> Result<Sink, Rejection> {
        let mut hbuf = vec![httparse::EMPTY_HEADER; self.limits.max_part_headers];
        let parsed = match httparse::parse_headers(&self.head, &mut hbuf) {
            Ok(httparse::Status::Complete((_, headers))) => headers,
            Ok(httparse::Status::Partial) => {
                return Err(bad("part without a header/body separator"));
            }
            Err(httparse::Error::TooManyHeaders) => {
                return Err(over("part headers over max_part_headers"));
            }
            // httparse accepts only RFC 9110 token field names. This is stricter than rfc1867.c.
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
        let Some(client_filename) = filename else {
            return Ok(Sink::Field(FormField {
                name,
                value: Vec::new(),
                headers,
            }));
        };
        if self.files.len() + 1 > self.limits.max_files {
            return Err(over("file parts over max_files"));
        }
        let (out, file) = spool(&self.limits.dir).map_err(spool_failed)?;
        Ok(Sink::File {
            file: UploadedFile {
                name,
                client_filename,
                // httparse trims the value already.
                client_media_type: media_type.filter(|v| !v.is_empty()).map(<[u8]>::to_vec),
                headers,
                file,
                size: 0,
            },
            out,
        })
    }
}

/// The name and filename parameters of a content-disposition value.
struct Disposition {
    name: Option<Vec<u8>>,
    filename: Option<Vec<u8>>,
}

/// Reads the name and filename parameters whatever the disposition type says, as php-src does. https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L804-L815
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
                    // php-src unescapes only \\ and \" and keeps every other '\'. https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L495
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

/// The number of subdirectories of the spool directory. A file create and an unlink take the lock of their directory. With one directory, all IO threads and PHP threads wait for the same lock.
const SPOOL_SUBDIRS: u64 = 64;

/// Creates the spool file of a file part. The SpooledFile removes the file when it drops.
fn spool(dir: &Path) -> std::io::Result<(std::fs::File, SpooledFile)> {
    use std::os::unix::fs::OpenOptionsExt;
    let n = SPOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let subdir = dir.join((n % SPOOL_SUBDIRS).to_string());
    let path = subdir.join(format!("{}-{n}", std::process::id()));
    let open = || {
        std::fs::File::options()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
    };
    let out = match open() {
        // The first file of a subdirectory creates it. Another thread can create it at the same time.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Err(e) = std::fs::create_dir(&subdir)
                && e.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(e);
            }
            open()?
        }
        out => out?,
    };
    let file = SpooledFile {
        path,
        unlinked: false,
    };
    Ok((out, file))
}

fn spool_failed(e: std::io::Error) -> Rejection {
    Rejection {
        status: http::StatusCode::INTERNAL_SERVER_ERROR,
        reason: format!("upload spool failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        // The spool makes subdirectories, so the tests use a directory of their own.
        let dir = std::env::temp_dir().join("rapira-unit-spool");
        std::fs::create_dir_all(&dir).expect("creating the spool dir");
        Limits {
            dir,
            max_file_size: 2 * 1024 * 1024,
            max_field_size: 256 * 1024,
            max_files: 20,
            max_parts: 1024,
            max_part_headers: 32,
        }
    }

    /// Feeds the pieces in order and ends the body.
    fn parse(
        pieces: &[&[u8]],
        boundary: &[u8],
        limits: &Limits,
    ) -> Result<MultipartBody, Rejection> {
        let mut parser = Parser::new(boundary, limits);
        for piece in pieces {
            parser.feed(piece)?;
        }
        parser.finish()
    }

    fn ok(body: &[u8]) -> MultipartBody {
        parse(&[body], b"B", &limits()).unwrap_or_else(|_| panic!("expected a parse"))
    }

    /// The status of the rejection.
    fn rejected(body: &[u8], l: &Limits) -> u16 {
        match parse(&[body], b"B", l) {
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

    /// A close-delimiter is `--boundary--`, then padding and a line ending or the end of the body (RFC 2046 §5.1.1).
    #[test]
    fn close_dashes_with_trailing_junk_are_content_not_close() {
        let body = b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nbefore\r\n--B--junk\r\nafter\r\n--B--";
        let mb = ok(body);
        assert_eq!(mb.fields.len(), 1);
        assert_eq!(mb.fields[0].value, b"before\r\n--B--junk\r\nafter");
    }

    /// A delimiter line is the line ending, `--boundary`, optional `--`, padding and a line ending (RFC 2046 §5.1.1). Every other byte sequence is content.
    #[test]
    fn content_keeps_the_bytes_that_are_not_a_delimiter_line() {
        struct Case {
            name: &'static str,
            content: &'static [u8],
            want: &'static [u8],
        }
        let cases = [
            Case {
                name: "a CR before the line ending",
                content: b"v\r\r\n--B--",
                want: b"v\r",
            },
            Case {
                name: "one dash after the delimiter bytes",
                content: b"a\r\n--B-\r\nb\r\n--B--",
                want: b"a\r\n--B-\r\nb",
            },
            Case {
                name: "a CR without a LF after the delimiter bytes",
                content: b"a\r\n--B\rb\r\n--B--",
                want: b"a\r\n--B\rb",
            },
            Case {
                name: "junk after the padding of a close-delimiter",
                content: b"a\r\n--B-- x\r\n--B--",
                want: b"a\r\n--B-- x",
            },
            Case {
                name: "delimiter bytes inside a line",
                content: b"a--B--\r\n--B--",
                want: b"a--B--",
            },
            Case {
                name: "a part of the delimiter bytes at a line start",
                content: b"a\r\n--\r\n-\r\n--B--",
                want: b"a\r\n--\r\n-",
            },
            Case {
                name: "an empty content",
                content: b"\r\n--B--",
                want: b"",
            },
            Case {
                name: "a content that ends with a line ending",
                content: b"a\r\n\r\n--B--",
                want: b"a\r\n",
            },
            Case {
                name: "a bare LF before the delimiter line",
                content: b"a\r\n\n--B--",
                want: b"a\r\n",
            },
        ];
        let head = b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\n";
        for case in cases {
            let body = [head.as_slice(), case.content].concat();
            let mb = ok(&body);
            assert_eq!(mb.fields[0].value, case.want, "{}", case.name);
        }
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
            Case {
                name: "close-delimiter inside the header section",
                body: b"--B\r\nx: y\r\n--B--",
            },
            Case {
                name: "delimiter line directly after the empty line",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\n--B--",
            },
            Case {
                name: "CR without a LF after the close-delimiter",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--B--\r",
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
        let mb = parse(&[&body], &b, &limits())
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

    /// Each split of a body gives the result of the one-part body. The test splits at every position and into single-byte parts.
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
            Case {
                name: "delimiter line directly after the empty line",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\n--B--",
            },
            Case {
                name: "CR, dash and delimiter look-alike lines inside a value",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\r\n--B-\r\n--B\rb\r\n--\r\n-\r\n--B-- x\r\n\n--B--",
            },
            Case {
                name: "delimiter look-alike lines inside a file",
                body: b"--B\r\ncontent-disposition: form-data; name=f; filename=a.bin\r\n\r\nPAY\r\n--Bx\r\n-\r\r\n--B \t\r\ncontent-disposition: form-data; name=a\r\n\r\n\r\n--B--",
            },
            Case {
                name: "CR without a LF after the close-delimiter",
                body: b"--B\r\ncontent-disposition: form-data; name=x\r\n\r\nv\r\n--B--\r",
            },
        ];
        let l = limits();
        for case in cases {
            let whole = parse(&[case.body], b"B", &l);
            for at in 1..case.body.len() {
                let (head, tail) = case.body.split_at(at);
                let name = format!("{} split at {at}", case.name);
                assert_same(&name, &whole, &parse(&[head, tail], b"B", &l));
            }
            let single: Vec<&[u8]> = case.body.chunks(1).collect();
            let name = format!("{} in single bytes", case.name);
            assert_same(&name, &whole, &parse(&single, b"B", &l));
        }
    }
}
