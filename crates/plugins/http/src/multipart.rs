//! The multipart/form-data parser for dispatcher mode and the process spool directory. https://www.rfc-editor.org/rfc/rfc2046#section-5.1.1

use std::borrow::Cow;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};

use http::StatusCode;
use memchr::memmem;

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

/// A half-open index range into one of the flat buffers of `MultipartBody`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: u32,
    end: u32,
}

impl Span {
    #[inline]
    fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end && end <= u32::MAX as usize);
        Self {
            start: start as u32,
            end: end as u32,
        }
    }

    #[inline]
    fn range(self) -> Range<usize> {
        (self.start as usize)..(self.end as usize)
    }
}

/// One grouped header entry inside `MultipartBody`.
#[derive(Debug, Clone, Copy)]
struct HeaderGroup {
    name: Span,
    values: Span,
}

/// Flat contiguous storage for all field names, field values, header names, header values and upload metadata in one multipart body.
#[derive(Default)]
struct Arena {
    bytes: Vec<u8>,
    header_values: Vec<Span>,
    headers: Vec<HeaderGroup>,
}

impl Arena {
    fn push_bytes(&mut self, data: &[u8]) -> Span {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(data);
        Span::new(start, self.bytes.len())
    }

    fn slice(&self, span: Span) -> &[u8] {
        &self.bytes[span.range()]
    }
}

/// Borrowed view of one grouped part header in `MultipartBody`.
#[derive(Clone, Copy)]
pub struct HeaderRef<'a> {
    body: &'a MultipartBody,
    group: HeaderGroup,
}

impl<'a> HeaderRef<'a> {
    pub fn name(&self) -> &'a str {
        // SAFETY: httparse validates field names as RFC 9110 ASCII tokens before open_part copies them into the arena.
        unsafe { std::str::from_utf8_unchecked(self.body.arena.slice(self.group.name)) }
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = &'a [u8]> + 'a {
        let body = self.body;
        body.arena.header_values[self.group.values.range()]
            .iter()
            .map(move |&span| body.arena.slice(span))
    }

    pub fn to_owned_values(&self) -> HeaderValues {
        HeaderValues {
            name: self.name().to_owned(),
            values: self.values().map(<[u8]>::to_vec).collect(),
        }
    }
}

pub struct FormField {
    name: Span,
    value: Span,
    /// One entry per name in first-seen order.
    headers: Span,
}

pub struct UploadedFile {
    name: Span,
    client_filename: Span,
    /// The content-type value, verbatim. The parser maps an empty or OWS-only value to None.
    client_media_type: Option<Span>,
    /// One entry per name in first-seen order.
    headers: Span,
    pub file: SpooledFile,
    /// The number of bytes written to disk. The FFI edge assumes a 64-bit zend_long.
    pub size: u64,
}

pub struct MultipartBody {
    arena: Arena,
    pub fields: Vec<FormField>,
    pub files: Vec<UploadedFile>,
}

impl MultipartBody {
    pub fn field_name<'a>(&'a self, field: &FormField) -> &'a [u8] {
        self.arena.slice(field.name)
    }

    pub fn field_value<'a>(&'a self, field: &FormField) -> &'a [u8] {
        self.arena.slice(field.value)
    }

    pub fn field_headers<'a>(
        &'a self,
        field: &FormField,
    ) -> impl ExactSizeIterator<Item = HeaderRef<'a>> + 'a {
        self.arena.headers[field.headers.range()]
            .iter()
            .map(move |&group| HeaderRef { body: self, group })
    }

    pub fn file_name<'a>(&'a self, file: &UploadedFile) -> &'a [u8] {
        self.arena.slice(file.name)
    }

    pub fn file_client_filename<'a>(&'a self, file: &UploadedFile) -> &'a [u8] {
        self.arena.slice(file.client_filename)
    }

    pub fn file_client_media_type<'a>(&'a self, file: &UploadedFile) -> Option<&'a [u8]> {
        file.client_media_type.map(|s| self.arena.slice(s))
    }

    pub fn file_headers<'a>(
        &'a self,
        file: &UploadedFile,
    ) -> impl ExactSizeIterator<Item = HeaderRef<'a>> + 'a {
        self.arena.headers[file.headers.range()]
            .iter()
            .map(move |&group| HeaderRef { body: self, group })
    }
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
    _cleanup: rapira_sapi::exit::CleanupGuard,
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
        let cleanup_dir = dir.clone();
        let cleanup = rapira_sapi::exit::register(move || {
            let _ = std::fs::remove_dir_all(&cleanup_dir);
        });
        Ok(Self {
            dir,
            _lock: lock,
            _cleanup: cleanup,
        })
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

pub fn is_multipart(content_type: &[u8]) -> bool {
    let media = content_type.split(|&b| b == b';').next().unwrap_or(b"");
    media
        .trim_ascii()
        .eq_ignore_ascii_case(b"multipart/form-data")
}

/// Reads the boundary parameter as php-src does. The parameter name is case-insensitive. A quoted value loses its quotes, and a `,` ends an unquoted value. https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L707-L751
pub fn boundary(content_type: &[u8]) -> Result<Vec<u8>, StatusCode> {
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
                None => return Err(StatusCode::BAD_REQUEST),
            }
        } else {
            match memchr::memchr(b',', val) {
                Some(end) => val[..end].trim_ascii(),
                None => val,
            }
        };
        if val.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        return Ok(val.to_vec());
    }
    Err(StatusCode::BAD_REQUEST)
}

/// Maximum transport padding bytes (spaces or tabs) accepted on one delimiter line.
/// Excess padding releases the held bytes as content so a delimiter look-alike cannot buffer megabytes in memory.
const MAX_TAIL_PADDING: u16 = 256;

/// Maximum byte size of one multipart part's header section (including the terminating blank line).
const MAX_PART_HEADER_BYTES: usize = 64 << 10;

/// The tail of a delimiter line after the delimiter bytes: the two dashes of a close-delimiter, transport padding and the line ending (RFC 2046 §5.1.1).
#[derive(Clone, Copy, Default)]
struct Tail {
    dashes: u8,
    padding: u16,
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
            b'-' if self.padding == 0 && self.dashes < 2 => self.dashes += 1,
            // One dash is not a close-delimiter.
            _ if self.dashes == 1 => return Step::Content,
            b' ' | b'\t' => {
                if self.padding == MAX_TAIL_PADDING {
                    return Step::Content;
                }
                self.padding += 1;
            }
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

struct FileSink {
    file: UploadedFile,
    out: std::fs::File,
}

/// The target of the content bytes. Heavy variants are boxed to keep `Sink` small (16 bytes).
enum Sink {
    /// The preamble, or no open part.
    None,
    /// The part has this error. The parser reports it at the delimiter line that ends the part.
    Skip(StatusCode),
    /// Index of the in-progress `FormField` already pushed into `Parser::fields`.
    Field(usize),
    File(Box<FileSink>),
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
    /// The header section of the current part so far when a header section spans chunks.
    head: Vec<u8>,
    /// The start of the current line in `head`.
    line: usize,
    /// The opening delimiter line is complete.
    opened: bool,
    arena: Arena,
    fields: Vec<FormField>,
    files: Vec<UploadedFile>,
}

impl<'a> Parser<'a> {
    /// Creates a streaming multipart parser, optionally pre-reserving flat arena storage from a `Content-Length` hint.
    pub fn new(boundary: &[u8], limits: &'a Limits, size_hint: Option<usize>) -> Self {
        let delimiter = [b"--".as_slice(), boundary].concat();
        // Pre-reserve only for medium form bodies (<= 64 KiB); larger bodies are often spooled files where arena.bytes stays tiny.
        let (bytes_cap, part_cap) = match size_hint {
            Some(n) if n <= (64 << 10) => (n, (n / 64).min(limits.max_parts).min(256)),
            _ => (0, 0),
        };
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
            arena: Arena {
                bytes: Vec::with_capacity(bytes_cap),
                headers: Vec::with_capacity(part_cap),
                header_values: Vec::with_capacity(part_cap),
            },
            fields: Vec::with_capacity(part_cap),
            files: Vec::new(),
        }
    }

    /// Takes the next bytes of the body. After an error the parser takes no more bytes.
    pub fn feed(&mut self, mut bytes: &[u8]) -> Result<(), StatusCode> {
        while !bytes.is_empty() {
            bytes = match self.state {
                State::Headers => self.headers(bytes)?,
                State::Content => self.content(bytes)?,
                State::Cr => self.after_cr(bytes),
                State::LineStart(matched) => self.line_start(bytes, matched)?,
                State::Tail(tail) => self.tail(bytes, tail)?,
                State::Done => return Ok(()),
            };
        }
        Ok(())
    }

    /// Ends the body. A close-delimiter can end the body without a line ending.
    pub fn finish(mut self) -> Result<MultipartBody, StatusCode> {
        match self.state {
            State::Done => {}
            State::Tail(tail) if tail.is_close() && !tail.cr => {
                let bare = self.held.first() == Some(&b'-');
                self.end_part(true, bare)?;
            }
            State::Headers if self.is_delimiter_line(&self.head[self.line..], true) => {
                return Err(StatusCode::BAD_REQUEST);
            }
            _ => return Err(StatusCode::BAD_REQUEST),
        }
        Ok(MultipartBody {
            arena: self.arena,
            fields: self.fields,
            files: self.files,
        })
    }

    /// Takes header lines up to the empty line, CRLF or bare LF. A line starts at the part start or after a LF, as in httparse. A delimiter line before the empty line ends a part that has no header/body separator.
    fn headers<'b>(&mut self, bytes: &'b [u8]) -> Result<&'b [u8], StatusCode> {
        if self.head.is_empty() {
            let mut from = 0;
            while let Some(lf) = memchr::memchr(b'\n', &bytes[from..]) {
                let end = from + lf + 1;
                if end > MAX_PART_HEADER_BYTES {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                let line = &bytes[from..end];
                if matches!(line, b"\n" | b"\r\n") {
                    open_part(
                        &mut self.sink,
                        &mut self.arena,
                        &mut self.fields,
                        self.limits,
                        self.files.len(),
                        &bytes[..end],
                    );
                    // The content starts at a line start.
                    self.state = State::LineStart(0);
                    return Ok(&bytes[end..]);
                }
                if self.is_delimiter_line(line, false) {
                    return Err(StatusCode::BAD_REQUEST);
                }
                from = end;
            }
            if bytes.len() > MAX_PART_HEADER_BYTES {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            self.head.extend_from_slice(bytes);
            self.line = from;
            return Ok(&[]);
        }
        let mut from = 0;
        while let Some(lf) = memchr::memchr(b'\n', &bytes[from..]) {
            let end = from + lf + 1;
            if self.head.len() + (end - from) > MAX_PART_HEADER_BYTES {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }
            self.head.extend_from_slice(&bytes[from..end]);
            from = end;
            let line = &self.head[self.line..];
            if matches!(line, b"\n" | b"\r\n") {
                open_part(
                    &mut self.sink,
                    &mut self.arena,
                    &mut self.fields,
                    self.limits,
                    self.files.len(),
                    &self.head,
                );
                // The content starts at a line start.
                self.state = State::LineStart(0);
                return Ok(&bytes[from..]);
            }
            if self.is_delimiter_line(line, false) {
                return Err(StatusCode::BAD_REQUEST);
            }
            self.line = self.head.len();
        }
        let rem = &bytes[from..];
        if self.head.len() + rem.len() > MAX_PART_HEADER_BYTES {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        self.head.extend_from_slice(rem);
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
    fn content<'b>(&mut self, bytes: &'b [u8]) -> Result<&'b [u8], StatusCode> {
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
                let after = &bytes[at + delimiter..];
                if after.starts_with(b"\r\n") {
                    self.emit(&bytes[..end]);
                    self.end_part(false, false)?;
                    return Ok(&after[2..]);
                }
                if after.starts_with(b"\n") {
                    self.emit(&bytes[..end]);
                    self.end_part(false, false)?;
                    return Ok(&after[1..]);
                }
                if after.starts_with(b"--\r\n") {
                    self.emit(&bytes[..end]);
                    self.end_part(true, false)?;
                    return Ok(&after[4..]);
                }
                if after.starts_with(b"--\n") {
                    self.emit(&bytes[..end]);
                    self.end_part(true, false)?;
                    return Ok(&after[3..]);
                }
                self.emit(&bytes[..end]);
                self.held.extend_from_slice(&bytes[end..at + delimiter]);
                self.state = State::Tail(Tail::default());
                return Ok(after);
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
        Ok(&[])
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

    fn line_start<'b>(&mut self, bytes: &'b [u8], matched: usize) -> Result<&'b [u8], StatusCode> {
        let want = &self.finder.needle()[matched..];
        if bytes[0] != want[0] {
            if self.held.is_empty() {
                self.state = State::Content;
            } else {
                self.release();
            }
            return Ok(bytes);
        }
        let len = want.len().min(bytes.len());
        if bytes[..len] != want[..len] {
            if self.held.is_empty() {
                self.state = State::Content;
            } else {
                self.release();
            }
            return Ok(bytes);
        }
        if len == want.len() {
            let after = &bytes[len..];
            let bare = self.held.is_empty() || self.held[0] == b'-';
            if after.starts_with(b"\r\n") {
                self.end_part(false, bare)?;
                return Ok(&after[2..]);
            }
            if after.starts_with(b"\n") {
                self.end_part(false, bare)?;
                return Ok(&after[1..]);
            }
            if after.starts_with(b"--\r\n") {
                self.end_part(true, bare)?;
                return Ok(&after[4..]);
            }
            if after.starts_with(b"--\n") {
                self.end_part(true, bare)?;
                return Ok(&after[3..]);
            }
            self.state = State::Tail(Tail::default());
        } else {
            self.state = State::LineStart(matched + len);
        }
        self.held.extend_from_slice(&bytes[..len]);
        Ok(&bytes[len..])
    }

    fn tail<'b>(&mut self, bytes: &'b [u8], mut tail: Tail) -> Result<&'b [u8], StatusCode> {
        for (i, &byte) in bytes.iter().enumerate() {
            match tail.step(byte) {
                Step::More => {}
                Step::Delimiter => {
                    let bare = self.held.first() == Some(&b'-');
                    self.end_part(tail.is_close(), bare)?;
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
        if bytes.is_empty() {
            return;
        }
        let error = match &mut self.sink {
            Sink::None | Sink::Skip(_) => return,
            Sink::Field(idx) => {
                let field = &mut self.fields[*idx];
                let len = self.arena.bytes.len() - (field.value.start as usize);
                if len + bytes.len() <= self.limits.max_field_size {
                    self.arena.bytes.extend_from_slice(bytes);
                    field.value.end = self.arena.bytes.len() as u32;
                    return;
                }
                self.fields.pop();
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Sink::File(f) => {
                if f.file.size + bytes.len() as u64 > self.limits.max_file_size {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else if f.out.write_all(bytes).is_err() {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    f.file.size += bytes.len() as u64;
                    return;
                }
            }
        };
        // The drop of a file part removes its spool file.
        self.sink = Sink::Skip(error);
    }

    /// Ends the preamble or the current part at a delimiter line.
    /// `bare` is true when no line ending separated the header section from the delimiter line.
    fn end_part(&mut self, close: bool, bare: bool) -> Result<(), StatusCode> {
        self.held.clear();
        match &self.sink {
            Sink::None => {}
            Sink::Field(_) => {
                self.sink = Sink::None;
                if bare {
                    self.fields.pop();
                    return Err(StatusCode::BAD_REQUEST);
                }
            }
            _ => match std::mem::replace(&mut self.sink, Sink::None) {
                Sink::None | Sink::Field(_) => unreachable!(),
                _ if bare => return Err(StatusCode::BAD_REQUEST),
                Sink::Skip(status) => return Err(status),
                Sink::File(f) => self.files.push(f.file),
            },
        }
        self.opened = true;
        if close {
            self.state = State::Done;
            return Ok(());
        }
        if self.fields.len() + self.files.len() + 1 > self.limits.max_parts {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        self.head.clear();
        self.line = 0;
        self.state = State::Headers;
        Ok(())
    }
}

/// Reads the header section and writes the opened field, file, or error directly into `sink`.
/// A filename parameter, even an empty one, makes the part a file part.
fn open_part(
    sink: &mut Sink,
    arena: &mut Arena,
    fields: &mut Vec<FormField>,
    limits: &Limits,
    file_count: usize,
    head: &[u8],
) {
    debug_assert!(matches!(sink, Sink::None));
    if let Err(status) = parse_part(sink, arena, fields, limits, file_count, head) {
        *sink = Sink::Skip(status);
    }
}

fn parse_part(
    sink: &mut Sink,
    arena: &mut Arena,
    fields: &mut Vec<FormField>,
    limits: &Limits,
    file_count: usize,
    head: &[u8],
) -> Result<(), StatusCode> {
    const STACK_HEADERS: usize = 8;
    let mut stack_buf = [httparse::EMPTY_HEADER; STACK_HEADERS];
    let mut heap_buf;
    let stack_cap = limits.max_part_headers.min(STACK_HEADERS);
    let parsed = match httparse::parse_headers(head, &mut stack_buf[..stack_cap]) {
        Ok(httparse::Status::Complete((_, headers))) => headers,
        Ok(httparse::Status::Partial) => {
            return Err(StatusCode::BAD_REQUEST);
        }
        Err(httparse::Error::TooManyHeaders) if limits.max_part_headers > STACK_HEADERS => {
            heap_buf = vec![httparse::EMPTY_HEADER; limits.max_part_headers];
            match httparse::parse_headers(head, &mut heap_buf) {
                Ok(httparse::Status::Complete((_, headers))) => headers,
                Ok(httparse::Status::Partial) => {
                    return Err(StatusCode::BAD_REQUEST);
                }
                Err(httparse::Error::TooManyHeaders) => {
                    return Err(StatusCode::PAYLOAD_TOO_LARGE);
                }
                Err(_) => return Err(StatusCode::BAD_REQUEST),
            }
        }
        Err(httparse::Error::TooManyHeaders) => {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        // httparse accepts only RFC 9110 token field names. This is stricter than rfc1867.c.
        Err(_) => return Err(StatusCode::BAD_REQUEST),
    };

    let mut disposition: Option<&[u8]> = None;
    let mut media_type: Option<&[u8]> = None;
    for h in parsed.iter() {
        if h.name.eq_ignore_ascii_case("content-disposition") {
            if disposition.replace(h.value).is_some() {
                return Err(StatusCode::BAD_REQUEST);
            }
        } else if h.name.eq_ignore_ascii_case("content-type") && media_type.is_none() {
            media_type = Some(h.value);
        }
    }
    let Some(disposition) = disposition else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let Disposition { name, filename } = disposition_params(disposition)?;
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let headers_start = arena.headers.len();
    for i in 0..parsed.len() {
        let hname = parsed[i].name;
        if parsed[..i].iter().any(|prev| prev.name == hname) {
            continue;
        }
        let name_span = arena.push_bytes(hname.as_bytes());
        let values_start = arena.header_values.len();
        for h in &parsed[i..] {
            if h.name == hname {
                let vspan = arena.push_bytes(h.value);
                arena.header_values.push(vspan);
            }
        }
        arena.headers.push(HeaderGroup {
            name: name_span,
            values: Span::new(values_start, arena.header_values.len()),
        });
    }
    let headers = Span::new(headers_start, arena.headers.len());

    let name_span = arena.push_bytes(&name);
    let Some(client_filename) = filename else {
        let value_start = arena.bytes.len();
        let idx = fields.len();
        fields.push(FormField {
            name: name_span,
            value: Span::new(value_start, value_start),
            headers,
        });
        *sink = Sink::Field(idx);
        return Ok(());
    };
    if file_count + 1 > limits.max_files {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let filename_span = arena.push_bytes(&client_filename);
    // httparse trims the value already.
    let media_span = media_type
        .filter(|v| !v.is_empty())
        .map(|v| arena.push_bytes(v));
    let (out, file) = spool(&limits.dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    *sink = Sink::File(Box::new(FileSink {
        file: UploadedFile {
            name: name_span,
            client_filename: filename_span,
            client_media_type: media_span,
            headers,
            file,
            size: 0,
        },
        out,
    }));
    Ok(())
}

/// The name and filename parameters of a content-disposition value.
struct Disposition<'a> {
    name: Option<Cow<'a, [u8]>>,
    filename: Option<Cow<'a, [u8]>>,
}

#[inline]
fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

#[inline]
fn find_quote_or_bs(haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == b'"' || b == b'\\')
}

/// Reads the name and filename parameters whatever the disposition type says, as php-src does. https://github.com/php/php-src/blob/php-8.5.8/main/rfc1867.c#L804-L815
fn disposition_params(v: &[u8]) -> Result<Disposition<'_>, StatusCode> {
    let mut name: Option<Cow<'_, [u8]>> = None;
    let mut filename: Option<Cow<'_, [u8]>> = None;
    let mut i = find_byte(v, b';').map(|i| i + 1).unwrap_or(v.len());
    while i < v.len() {
        let Some(eq) = find_byte(&v[i..], b'=').map(|o| o + i) else {
            break;
        };
        let key = v[i..eq].trim_ascii();
        let mut j = eq + 1;
        while matches!(v.get(j), Some(b' ' | b'\t')) {
            j += 1;
        }
        let (val, next) = if v.get(j) == Some(&b'"') {
            let start = j + 1;
            let rest = &v[start..];
            match find_quote_or_bs(rest) {
                None => return Err(StatusCode::BAD_REQUEST),
                Some(pos) if rest[pos] == b'"' => (Cow::Borrowed(&rest[..pos]), start + pos + 1),
                Some(pos) => {
                    let mut out = Vec::with_capacity(pos + 16);
                    out.extend_from_slice(&rest[..pos]);
                    let mut k = start + pos;
                    loop {
                        match v.get(k) {
                            None => return Err(StatusCode::BAD_REQUEST),
                            Some(b'"') => break (Cow::Owned(out), k + 1),
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
                }
            }
        } else {
            let end = find_byte(&v[j..], b';').map(|o| o + j).unwrap_or(v.len());
            (Cow::Borrowed(v[j..end].trim_ascii()), end)
        };
        let slot = if key.len() == 4 && key.eq_ignore_ascii_case(b"name") {
            Some(&mut name)
        } else if key.len() == 8 && key.eq_ignore_ascii_case(b"filename") {
            Some(&mut filename)
        } else {
            None
        };
        if let Some(slot) = slot {
            if slot.is_some() {
                return Err(StatusCode::BAD_REQUEST);
            }
            *slot = Some(val);
        }
        i = find_byte(&v[next..], b';')
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
    use std::fmt::NumBuffer;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::OpenOptionsExt;

    static PID: std::sync::OnceLock<Box<[u8]>> = std::sync::OnceLock::new();
    let pid = PID.get_or_init(|| {
        std::process::id()
            .format_into(&mut NumBuffer::new())
            .as_bytes()
            .into()
    });
    let n = SPOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir_bytes = dir.as_os_str().as_bytes();
    let mut sub_buf = NumBuffer::new();
    let sub = (n % SPOOL_SUBDIRS).format_into(&mut sub_buf).as_bytes();
    let mut seq_buf = NumBuffer::new();
    let seq = n.format_into(&mut seq_buf).as_bytes();

    let mut raw =
        Vec::with_capacity(dir_bytes.len() + 1 + sub.len() + 1 + pid.len() + 1 + seq.len());
    raw.extend_from_slice(dir_bytes);
    if !raw.ends_with(b"/") {
        raw.push(b'/');
    }
    raw.extend_from_slice(sub);
    raw.push(b'/');
    raw.extend_from_slice(pid);
    raw.push(b'-');
    raw.extend_from_slice(seq);
    let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
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
            if let Some(subdir) = path.parent()
                && let Err(e) = std::fs::create_dir(subdir)
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
    ) -> Result<MultipartBody, StatusCode> {
        let mut parser = Parser::new(boundary, limits, None);
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
            Err(status) => status.as_u16(),
            Ok(_) => panic!("expected a rejection"),
        }
    }

    #[test]
    fn lf_only_lines_preamble_and_padding_are_tolerated() {
        let body = b"preamble to ignore\n--B \t\ncontent-disposition: form-data; name=x\n\nv\n--B--\nepilogue";
        let mb = ok(body);
        assert_eq!(mb.fields.len(), 1);
        assert_eq!(mb.field_value(&mb.fields[0]), b"v");
    }

    #[test]
    fn quoted_boundary_and_charset_field_parse() {
        let ct = b"multipart/form-data; boundary=\"B\"";
        assert_eq!(boundary(ct).unwrap(), b"B");
        let body =
            b"--B\r\ncontent-disposition: form-data; name=\"_charset_\"\r\n\r\nutf-8\r\n--B--";
        let mb = ok(body);
        assert_eq!(mb.field_name(&mb.fields[0]), b"_charset_");
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
        assert_eq!(
            mb.field_value(&mb.fields[0]),
            b"before\r\n--B--junk\r\nafter"
        );
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
            assert_eq!(mb.field_value(&mb.fields[0]), case.want, "{}", case.name);
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
            assert_eq!(mb.field_value(&mb.fields[0]), b"v", "{}", case.name);
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
            let got: Vec<HeaderValues> = mb
                .field_headers(&mb.fields[0])
                .map(|h| h.to_owned_values())
                .collect();
            assert_eq!(got, want, "{}", case.name);
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
                .map_err(|s| s.as_u16());
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
        assert_eq!(mb.field_value(&mb.fields[0]), b"v");
    }

    /// Compares two parse results field by field, with the bytes of each spooled file.
    fn assert_same(
        case: &str,
        want: &Result<MultipartBody, StatusCode>,
        got: &Result<MultipartBody, StatusCode>,
    ) {
        match (want, got) {
            (Ok(want), Ok(got)) => {
                assert_eq!(want.fields.len(), got.fields.len(), "{case}: field count");
                for (w, g) in want.fields.iter().zip(&got.fields) {
                    let wh: Vec<HeaderValues> =
                        want.field_headers(w).map(|h| h.to_owned_values()).collect();
                    let gh: Vec<HeaderValues> =
                        got.field_headers(g).map(|h| h.to_owned_values()).collect();
                    assert_eq!(
                        (want.field_name(w), want.field_value(w), wh),
                        (got.field_name(g), got.field_value(g), gh),
                        "{case}: field"
                    );
                }
                assert_eq!(want.files.len(), got.files.len(), "{case}: file count");
                for (w, g) in want.files.iter().zip(&got.files) {
                    assert_eq!(
                        (
                            want.file_name(w),
                            want.file_client_filename(w),
                            want.file_client_media_type(w)
                        ),
                        (
                            got.file_name(g),
                            got.file_client_filename(g),
                            got.file_client_media_type(g)
                        ),
                        "{case}: file"
                    );
                    let wh: Vec<HeaderValues> =
                        want.file_headers(w).map(|h| h.to_owned_values()).collect();
                    let gh: Vec<HeaderValues> =
                        got.file_headers(g).map(|h| h.to_owned_values()).collect();
                    assert_eq!((wh, w.size), (gh, g.size), "{case}: file headers and size");
                    let spooled = |f: &UploadedFile| std::fs::read(&f.file.path).expect("spooled");
                    assert_eq!(spooled(w), spooled(g), "{case}: spooled bytes");
                }
            }
            (Err(w), Err(g)) => assert_eq!(w, g, "{case}: rejection"),
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

    #[test]
    fn bounded_tail_padding_and_part_header_section() {
        struct Case {
            name: &'static str,
            body: Vec<u8>,
            chunk_size: usize,
            max_file_size: u64,
            want_status: Option<u16>,
            want_field: Option<Vec<u8>>,
        }
        let pad_256 = [
            b"--B".as_slice(),
            &vec![b' '; 256],
            b"\r\ncontent-disposition: form-data; name=a\r\n\r\nok\r\n--B--",
        ]
        .concat();
        let pad_257_in_field = [
            b"--B\r\ncontent-disposition: form-data; name=a\r\n\r\nstart\r\n--B".as_slice(),
            &vec![b' '; 257],
            b"\r\n--B--",
        ]
        .concat();
        let mut want_pad_257 = b"start\r\n--B".to_vec();
        want_pad_257.extend(std::iter::repeat_n(b' ', 257));
        let pad_over_file_limit = [
            b"--B\r\ncontent-disposition: form-data; name=f; filename=a.bin\r\n\r\n--B".as_slice(),
            &vec![b' '; 4096],
            b"\r\n--B--",
        ]
        .concat();
        let huge_header = [
            b"--B\r\ncontent-disposition: form-data; name=a\r\nx-pad: ".as_slice(),
            &vec![b'h'; 65 << 10],
            b"\r\n\r\nval\r\n--B--",
        ]
        .concat();
        let unterminated_huge_header =
            [b"--B\r\nx-pad: ".as_slice(), &vec![b'h'; 65 << 10]].concat();

        let cases = [
            Case {
                name: "256-byte tail padding still matches delimiter",
                body: pad_256,
                chunk_size: 1024,
                max_file_size: 1024,
                want_status: None,
                want_field: Some(b"ok".to_vec()),
            },
            Case {
                name: "257-byte tail padding releases held bytes as content",
                body: pad_257_in_field,
                chunk_size: 64,
                max_file_size: 1024,
                want_status: None,
                want_field: Some(want_pad_257),
            },
            Case {
                name: "excess tail padding in file part triggers max_file_size instead of buffering in held",
                body: pad_over_file_limit,
                chunk_size: 128,
                max_file_size: 512,
                want_status: Some(413),
                want_field: None,
            },
            Case {
                name: "single-chunk part header section over 64 KiB rejected with 413",
                body: huge_header.clone(),
                chunk_size: 128 << 10,
                max_file_size: 1024,
                want_status: Some(413),
                want_field: None,
            },
            Case {
                name: "split part header section over 64 KiB rejected with 413",
                body: huge_header,
                chunk_size: 4096,
                max_file_size: 1024,
                want_status: Some(413),
                want_field: None,
            },
            Case {
                name: "unterminated part header section over 64 KiB rejected during feed",
                body: unterminated_huge_header,
                chunk_size: 4096,
                max_file_size: 1024,
                want_status: Some(413),
                want_field: None,
            },
        ];

        for case in cases {
            let mut l = limits();
            l.max_file_size = case.max_file_size;
            let pieces: Vec<&[u8]> = case.body.chunks(case.chunk_size).collect();
            let res = parse(&pieces, b"B", &l);
            match case.want_status {
                Some(code) => {
                    let Err(status) = res else {
                        panic!("{}: expected rejection {code}", case.name);
                    };
                    assert_eq!(status.as_u16(), code, "{}", case.name);
                }
                None => {
                    let mp = res.expect(case.name);
                    assert_eq!(mp.fields.len(), 1, "{}", case.name);
                    assert_eq!(
                        mp.field_value(&mp.fields[0]),
                        case.want_field.as_deref().unwrap(),
                        "{}",
                        case.name
                    );
                }
            }
        }
    }
}
