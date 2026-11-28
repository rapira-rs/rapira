use crate::context::{Permanent, ctx, with_ctx};
use crate::diagnostics::syslog_to_level;
use crate::types::{BodyCursor, RequestHeaders, ResponseHeaders};
use crate::*;
use ::http::header::{AUTHORIZATION, COOKIE, HeaderName, HeaderValue};
use core::slice;
use std::ffi::{CStr, CString, c_void};
use std::fmt::NumBuffer;
use std::io::Read;
use std::os::raw::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{null, null_mut};

// Only this catch_unwind turns a panic inside PHP into a failed request and a
// recycle, so the process does not abort. The build checks the panic strategy.
// https://doc.rust-lang.org/reference/conditional-compilation.html#panic
#[cfg(panic = "abort")]
compile_error!("rapira_sapi needs panic = \"unwind\": callbacks::guard relies on catch_unwind");

pub fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!(target: "rapira", "panic caught at the FFI boundary; default substituted");
        default
    })
}

/// http_response_code is a c_int that the app controls, so the clamp stops a wrap of the u16 cast.
fn head_status(h: &sapi_headers_struct) -> u16 {
    if h.http_response_code != 0 {
        h.http_response_code.clamp(100, 599) as u16
    } else {
        200
    }
}

fn head_lines(h: &sapi_headers_struct) -> impl Iterator<Item = &sapi_header_struct> {
    let mut el: *mut _zend_llist_element = h.headers.head;
    std::iter::from_fn(move || {
        let e: &_zend_llist_element = unsafe { el.as_ref()? };
        el = e.next;
        Some(unsafe { &*(e.data.as_ptr() as *const sapi_header_struct) })
    })
}

fn header_field(sh: &sapi_header_struct) -> Option<(HeaderName, HeaderValue)> {
    if sh.header.is_null() || sh.header_len == 0 {
        return None;
    }
    let line: &[u8] = unsafe { slice::from_raw_parts(sh.header as *const u8, sh.header_len) };
    let Some(field) = split_header_line(line) else {
        tracing::debug!(
            target: "php",
            "dropped unrepresentable response header: {}",
            String::from_utf8_lossy(line)
        );
        return None;
    };
    Some(field)
}

/// `HeaderName::from_bytes` accepts only the RFC 9110 `tchar` set and `HeaderValue::from_bytes` only the field-value bytes.
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.5
fn split_header_line(line: &[u8]) -> Option<(HeaderName, HeaderValue)> {
    let i: usize = line.iter().position(|&b| b == b':')?;
    let name = HeaderName::from_bytes(line[..i].trim_ascii()).ok()?;
    let value = HeaderValue::from_bytes(line[i + 1..].trim_ascii()).ok()?;
    Some((name, value))
}

/// The separator that joins the repeats of `name`. `None` marks a singleton field, which keeps the first line and drops the rest.
/// A join is legal only for comma-list fields. https://www.rfc-editor.org/rfc/rfc9110#section-5.3
/// `Cookie` lines join on `"; "`. https://www.rfc-editor.org/rfc/rfc6265#section-4.2.1
fn field_line_separator(name: &[u8]) -> Option<&'static [u8]> {
    const SINGLETON: [&[u8]; 6] = [
        b"authorization",
        b"proxy-authorization",
        b"content-type",
        b"content-length",
        b"referer",
        b"from",
    ];
    if SINGLETON.iter().any(|s| name.eq_ignore_ascii_case(s)) {
        None
    } else if name.eq_ignore_ascii_case(b"cookie") {
        Some(b"; ")
    } else {
        Some(b", ")
    }
}

/// Returns the one CGI value of `name`. Repeated lines join into `buf` on the field separator; a single line or a singleton field returns the first line and leaves `buf` unchanged.
pub(crate) fn joined_field<'a>(
    headers: &'a RequestHeaders,
    name: &HeaderName,
    buf: &'a mut Vec<u8>,
) -> Option<&'a [u8]> {
    let mut values = headers.get_all(name).iter().map(HeaderValue::as_bytes);
    let first = values.next()?;
    let (Some(sep), Some(second)) = (
        field_line_separator(name.as_str().as_bytes()),
        values.next(),
    ) else {
        return Some(first);
    };
    buf.clear();
    buf.extend_from_slice(first);
    for value in std::iter::once(second).chain(values) {
        buf.extend_from_slice(sep);
        buf.extend_from_slice(value);
    }
    Some(buf)
}

/// Formats `HTTP_<FIELD>` on the stack when it fits, or on the heap for long names.
/// A field name is an RFC 9110 token, so it has no NUL, ' ' or '['. '.' maps to '_' as php_register_variable_ex mangles it.
/// https://www.rfc-editor.org/rfc/rfc3875#section-4.1.18
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.1
fn with_cgi_header_name(field: &[u8], put: &mut impl FnMut(&CStr)) {
    let encode = |b: u8| {
        if b == b'-' || b == b'.' {
            b'_'
        } else {
            b.to_ascii_uppercase()
        }
    };
    let total = 5 + field.len() + 1;
    if total <= 128 {
        let mut stack = [0u8; 128];
        stack[..5].copy_from_slice(b"HTTP_");
        for (dst, &b) in stack[5..5 + field.len()].iter_mut().zip(field) {
            *dst = encode(b);
        }
        stack[total - 1] = 0;
        let name = CStr::from_bytes_until_nul(&stack[..total]).unwrap_or_default();
        put(name);
    } else {
        let mut heap = Vec::with_capacity(total);
        heap.extend_from_slice(b"HTTP_");
        heap.extend(field.iter().map(|&b| encode(b)));
        heap.push(0);
        let name = CStr::from_bytes_until_nul(&heap).unwrap_or_default();
        put(name);
    }
}

/// Owns header storage across calls into Zend, including a PHP bailout.
pub struct CgiHeaders {
    pub(crate) raw: RequestHeaders,
    pub(crate) cookie: Option<CString>,
    joined: Vec<u8>,
}

impl CgiHeaders {
    pub fn new(raw: RequestHeaders) -> Self {
        let mut joined = Vec::new();
        let cookie = joined_field(&raw, &COOKIE, &mut joined)
            .map(|v| crate::types::cgi_cstring("Cookie", v));
        Self {
            raw,
            cookie,
            joined,
        }
    }

    fn visit_unique(&mut self, mut visit: impl FnMut(&[u8], &[u8])) {
        let joined = &mut self.joined;
        let cookie = self.cookie.as_ref();
        // http 1.5.0 groups values by identical HeaderName reference in first-insertion order.
        // https://docs.rs/http/1.5.0/src/http/header/map.rs.html#2323-2356
        let mut fields = self.raw.iter().peekable();
        while let Some((name, first)) = fields.next() {
            let name_bytes = name.as_str().as_bytes();
            let mut rest = std::iter::from_fn(|| {
                fields
                    .next_if(|&(n, _)| std::ptr::eq(n, name))
                    .map(|(_, v)| v.as_bytes())
            });
            if name == COOKIE
                && let Some(c) = cookie
                && !c.as_bytes().is_empty()
            {
                for _ in rest {}
                visit(name_bytes, c.as_bytes());
                continue;
            }
            let Some(second) = rest.next() else {
                visit(name_bytes, first.as_bytes());
                continue;
            };
            if let Some(sep) = field_line_separator(name_bytes) {
                joined.clear();
                joined.extend_from_slice(first.as_bytes());
                joined.extend_from_slice(sep);
                joined.extend_from_slice(second);
                for val in rest {
                    joined.extend_from_slice(sep);
                    joined.extend_from_slice(val);
                }
                visit(name_bytes, joined.as_slice());
            } else {
                for _ in rest {}
                visit(name_bytes, first.as_bytes());
            }
        }
    }

    /// Registers one variable at the first occurrence of each field name.
    pub fn register(&mut self, mut put: impl FnMut(&CStr, &[u8])) {
        self.visit_unique(|field, value| {
            with_cgi_header_name(field, &mut |name| put(name, value));
        });
    }

    /// Registers `HTTP_*` entries using pre-interned keys for common headers and stack formatting for custom headers.
    pub(crate) fn register_server(
        &mut self,
        s: &crate::context::ServerStrings,
        mut put_interned: impl FnMut(Permanent, &[u8]),
        mut put_raw: impl FnMut(&CStr, &[u8]),
    ) {
        self.visit_unique(|field, value| {
            if let Some(perm) = s.http_header_ci(field) {
                put_interned(perm, value);
            } else {
                with_cgi_header_name(field, &mut |name| put_raw(name, value));
            }
        });
    }
}

/// # Safety
/// `buf` must point at `len` readable bytes and `aborted` at a writable `bool`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ub_write(
    buf: *const c_char,
    len: usize,
    aborted: *mut bool,
) -> usize {
    guard(None, || {
        let data = unsafe { slice::from_raw_parts(buf.cast::<u8>(), len) };
        let Some(ex) = (unsafe { ctx() }) else {
            tracing::info!(target: "php", "{}", String::from_utf8_lossy(data));
            return Some(len);
        };
        match ex.write_body(data, || unsafe {
            let headers = &raw const (*rapira_sg()).sapi_headers;
            head_status(&*headers)
        }) {
            Ok(written) => Some(written),
            Err(()) => {
                unsafe { *aborted = true };
                Some(0)
            }
        }
    })
    // None means that guard caught a panic in the body.
    .unwrap_or_else(|| {
        unsafe { *aborted = true };
        0
    })
}

/// # Safety
/// A SAPI callback that PHP calls. `h` must be a valid `*mut sapi_headers_struct`
pub unsafe extern "C" fn send_headers(h: *mut sapi_headers_struct) -> c_int {
    guard(SAPI_HEADER_SEND_FAILED as c_int, || {
        let Some(ex) = (unsafe { ctx() }) else {
            return SAPI_HEADER_SENT_SUCCESSFULLY as c_int;
        };
        ex.send_headers(|| {
            let h = unsafe { &*h };
            let mut headers = ResponseHeaders::new();
            for (name, value) in head_lines(h).filter_map(header_field) {
                headers.append(name, value);
            }
            (head_status(h), headers)
        });
        SAPI_HEADER_SENT_SUCCESSFULLY as c_int
    })
}

pub(crate) unsafe extern "C" fn read_post(buf: *mut c_char, count: usize) -> usize {
    with_ctx(0, |ctx| {
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), count) };
        ctx.req_mut().body.read(dst).unwrap_or(0)
    })
}
/// Opens a zero-copy seekable body cursor backed by the bound request's raw byte chunks. Returns NULL when the body is empty.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_body_open() -> *mut c_void {
    with_ctx(null_mut(), |ctx| match ctx.req_mut().body.body_data() {
        Some(data) => Box::into_raw(Box::new(BodyCursor::new(data))).cast(),
        None => null_mut(),
    })
}

/// Opens a fresh cursor at byte 0 sharing the chunks of an existing `BodyCursor`.
/// # Safety
/// `cursor` must be a live pointer returned by `rapira_rs_body_open` or `rapira_rs_body_clone`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_body_clone(cursor: *const c_void) -> *mut c_void {
    guard(null_mut(), || {
        let src = unsafe { &*cursor.cast::<BodyCursor>() };
        Box::into_raw(Box::new(src.clone_at_start())).cast()
    })
}

/// Reads up to `count` bytes from `cursor` into `buf` and writes EOF state to `eof`.
/// # Safety
/// `cursor` must be live, `buf` writable for `count` bytes, and `eof` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_body_read(
    cursor: *mut c_void,
    buf: *mut c_char,
    count: usize,
    eof: *mut bool,
) -> usize {
    guard(0, || {
        let cur = unsafe { &mut *cursor.cast::<BodyCursor>() };
        let dst = unsafe { slice::from_raw_parts_mut(buf.cast::<u8>(), count) };
        let (n, is_eof) = cur.read(dst);
        unsafe { *eof = is_eof };
        n
    })
}

/// Seeks `cursor`, matching `php_stream_memory_seek`. Returns 0 on success, -1 on failure.
/// # Safety
/// `cursor` must be live and `newoffs` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_body_seek(
    cursor: *mut c_void,
    offset: i64,
    whence: c_int,
    newoffs: *mut i64,
) -> c_int {
    guard(-1, || {
        let cur = unsafe { &mut *cursor.cast::<BodyCursor>() };
        match cur.seek(offset, whence) {
            Some(pos) => {
                unsafe { *newoffs = pos };
                0
            }
            None => {
                unsafe { *newoffs = -1 };
                -1
            }
        }
    })
}

/// Returns the total byte length of the body behind `cursor`.
/// # Safety
/// `cursor` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_body_len(cursor: *const c_void) -> usize {
    guard(0, || unsafe { (*cursor.cast::<BodyCursor>()).len() })
}

/// Drops a `BodyCursor` created by `rapira_rs_body_open` or `rapira_rs_body_clone`.
/// # Safety
/// `cursor` must be live and dropped at most once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_body_drop(cursor: *mut c_void) {
    guard((), || {
        drop(unsafe { Box::from_raw(cursor.cast::<BodyCursor>()) });
    });
}

/// Returns the Authorization field of the bound request for php_handle_auth_data in rapira_activate (module.c). Returns NULL when no request is bound or the field is absent.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_authorization() -> *const c_char {
    with_ctx(null(), |ctx| {
        ctx.req()
            .authorization
            .as_ref()
            .map_or(null(), |a| a.as_ptr())
    })
}
pub(crate) unsafe extern "C" fn read_cookies() -> *mut c_char {
    with_ctx(null_mut(), |ctx| {
        ctx.req()
            .headers
            .cookie
            .as_ref()
            .map_or(null_mut(), |c| c.as_ptr() as *mut c_char)
    })
}
/// The upper bound of $_SERVER entries besides HTTP_*: 23 names of register_server_variables, then PHP_AUTH_USER, PHP_AUTH_PW, PHP_AUTH_DIGEST, REQUEST_TIME_FLOAT and REQUEST_TIME from php_register_server_variables (main/php_variables.c).
const SERVER_VARS_MAX: u32 = 28;

pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    // Every name is valid for php_register_known_variable: the boot names as written, HTTP_* through cgi_header_name.
    let put_bytes = |name: &CStr, val: &[u8]| unsafe {
        rapira_register_known_stringl(
            name.as_ptr(),
            name.to_bytes().len(),
            val.as_ptr() as *const c_char,
            val.len(),
            track_vars_array,
        );
    };
    // No request is bound in two cases: the boot run of the entrypoint in worker and dispatcher mode, and the request that makes the OPcache reset call.
    guard((), || {
        let Some(ctx) = (unsafe { ctx() }) else {
            unsafe { register_boot_variables(track_vars_array, put_bytes) };
            return;
        };
        let reqc = ctx.req_mut();
        // One allocation for the whole array: the env pairs of the pool, the names below, one HTTP_* name per field, and the entries php_register_server_variables adds after this callback.
        let ht = unsafe { (*track_vars_array).value.arr };
        // Inserts a permanent interned name and value. The insert takes no hash, no allocation and no copy.
        let put_interned = |name: Permanent, value: Permanent| unsafe {
            zend_hash_update(ht, name.as_ptr(), &mut value.zval());
        };
        // Inserts a permanent interned name and a value of this request, which PHP copies.
        let put = |name: Permanent, value: &[u8]| unsafe {
            rapira_hash_update_stringl(ht, name.as_ptr(), value.as_ptr().cast(), value.len());
        };
        // The env pairs of the pool go in first, so the request variables below override a pair with the same name.
        // Each name goes in as written. The environment import of PHP skips a name with a space, '.' or '[', and it stores an all-digit name as an integer key. Such a name is therefore different in the boot $_SERVER.
        crate::env::with_pool_pairs(|env| {
            let size = SERVER_VARS_MAX + reqc.headers.raw.len() as u32 + env.len() as u32;
            unsafe { zend_hash_extend(ht, size, false) };
            for &(name, value) in env {
                put_interned(name, value);
            }
        });
        let s = crate::context::server_strings();
        let script = crate::context::script();
        let identity = crate::HttpIdentity::current();
        put_interned(s.php_self, script.server_script_name);
        let uri_bytes = reqc.uri.to_bytes();
        let doc_uri = &uri_bytes[..reqc.path_len];
        if doc_uri == b"/" {
            put_interned(s.document_uri, s.slash);
        } else {
            put(s.document_uri, doc_uri);
        }
        put_interned(s.document_root, script.server_document_root);
        let scheme = if reqc.https {
            s.scheme_https
        } else {
            s.scheme_http
        };
        put_interned(s.request_scheme, scheme);
        // REMOTE_ADDR must hold a hostnumber (RFC 3875 §4.1.8), so a unix peer gets the loopback address.
        let mut ip_buf = [0u8; 39];
        let mut port_buf = NumBuffer::new();
        let (remote_addr, remote_port): (&[u8], &[u8]) = match &reqc.remote {
            Addr::Inet(sa) => (
                crate::exchange::ip_text(sa.ip(), &mut ip_buf).as_bytes(),
                sa.port().format_into(&mut port_buf).as_bytes(),
            ),
            Addr::Unix(_) => (b"127.0.0.1", b"0"),
        };
        put(s.remote_host, remote_addr);
        put(s.remote_port, remote_port);
        // REMOTE_IDENT is optional per CGI/1.1; rapira runs no RFC 1413 ident lookup, so it is empty.
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.10
        // https://www.rfc-editor.org/rfc/rfc1413
        put_interned(s.remote_ident, s.empty);
        let method_bytes = reqc.method.to_bytes();
        match s.method(method_bytes) {
            Some(m) => put_interned(s.request_method, m),
            None => put(s.request_method, method_bytes),
        }
        if uri_bytes == b"/" {
            put_interned(s.request_uri, s.slash);
        } else {
            put(s.request_uri, uri_bytes);
        }
        let query_bytes = reqc.query.to_bytes();
        if query_bytes.is_empty() {
            put_interned(s.query_string, s.empty);
        } else {
            put(s.query_string, query_bytes);
        }
        put_interned(s.script_filename, script.server_filename);
        put_interned(s.script_name, script.server_script_name);
        match reqc.protocol {
            "HTTP/1.1" => put_interned(s.server_protocol, s.http_1_1),
            "HTTP/1.0" => put_interned(s.server_protocol, s.http_1_0),
            other => put(s.server_protocol, other.as_bytes()),
        }
        put_interned(s.server_software, s.rapira);
        put_interned(s.server_name, identity.server_name);
        put_interned(s.server_port, identity.server_port);
        put(s.remote_addr, remote_addr);
        put_interned(s.gateway_interface, s.cgi_1_1);
        if reqc.https {
            put_interned(s.https, s.on);
        } else {
            put_interned(s.https, s.empty);
        }

        let auth_type: &[u8] = reqc
            .headers
            .raw
            .get(AUTHORIZATION)
            .and_then(|v| {
                v.as_bytes()
                    .split(|b| b.is_ascii_whitespace())
                    .find(|s| !s.is_empty())
            })
            .unwrap_or(b"");
        if auth_type.is_empty() {
            put_interned(s.auth_type, s.empty);
        } else {
            put(s.auth_type, auth_type);
        }

        let auth_user = unsafe { (*rapira_sg()).request_info.auth_user };
        if !auth_user.is_null() {
            let user: &CStr = unsafe { CStr::from_ptr(auth_user as *const c_char) };
            put(s.remote_user, user.to_bytes());
        }

        if let Some(ct) = &reqc.ctype {
            put(s.content_type, ct.to_bytes());
        }

        let content_length = reqc.body.len();
        if content_length == 0 {
            put_interned(s.content_length, s.zero);
        } else {
            put(
                s.content_length,
                content_length.format_into(&mut NumBuffer::new()).as_bytes(),
            );
        }
        reqc.headers.register_server(s, put, put_bytes);
    })
}

/// The variables of `php entrypoint.php`, in its order: the env table (env.rs), then the entrypoint path, which overrides an env variable of the same name.
/// https://github.com/php/php-src/blob/php-8.5.11/sapi/cli/php_cli.c#L316-L347
unsafe fn register_boot_variables(track_vars_array: *mut zval, put: impl Fn(&CStr, &[u8])) {
    unsafe {
        if let Some(import) = php_import_environment_variables {
            import(track_vars_array);
        }
    }
    let Some(script) = crate::context::SCRIPT.get() else {
        return;
    };
    let path = script.filename().to_bytes();
    put(c"PHP_SELF", path);
    put(c"SCRIPT_NAME", path);
    put(c"SCRIPT_FILENAME", path);
    put(c"PATH_TRANSLATED", path);
    put(c"DOCUMENT_ROOT", b"");
}
pub(crate) unsafe extern "C" fn log_message(message: *const c_char, syslog_type: c_int) {
    guard((), || {
        if message.is_null() {
            return;
        }

        let s = unsafe { CStr::from_ptr(message).to_string_lossy() };
        crate::diagnostics::event_at!("php", syslog_to_level(syslog_type), "{s}");
    })
}
/// php_error_cb records each fatal error before its bailout. A bailout with no such record is a normal end, such as exit(), or a client abort. The record holds only the last diagnostic.
/// https://github.com/php/php-src/blob/php-8.5.11/main/main.c#L1393-L1540
pub(crate) fn fatal_error_recorded() -> bool {
    unsafe {
        let pg = rapira_pg();
        !(*pg).last_error_message.is_null() && (*pg).last_error_type & E_FATAL_ERRORS as c_int != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_line_needs_a_colon_and_a_name() {
        assert!(split_header_line(b"no colon here").is_none());
        assert!(split_header_line(b": value").is_none());
        assert!(split_header_line(b"   : value").is_none());
    }

    #[test]
    fn header_line_trims_both_halves() {
        let (name, value) = split_header_line(b"X-Trace :  hello  ").unwrap();
        assert_eq!(name, "x-trace");
        assert_eq!(value, "hello");
    }

    /// sapi_header_op screens only CR, LF and NUL, so these still arrive here.
    #[test]
    fn unrepresentable_fields_are_rejected() {
        assert!(split_header_line(b"Content Type: text/html").is_none());
        assert!(split_header_line(b"X-Trace: \x01").is_none());
        assert!(split_header_line(b"X-Trace: a\x7fb").is_none());
    }

    #[test]
    fn obs_text_and_underscores_stay_legal() {
        assert_eq!(
            split_header_line(b"X-Bin: \xff\xfe").unwrap().1.as_bytes(),
            b"\xff\xfe"
        );
        assert_eq!(split_header_line(b"X_Custom: 1").unwrap().0, "x_custom");
    }

    fn hdrs(lines: &[(&str, &str)]) -> RequestHeaders {
        let mut h = RequestHeaders::new();
        for (k, v) in lines {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    /// List fields join on ", ", Cookie on "; ", and a singleton field keeps its first line.
    #[test]
    fn repeated_field_lines_join_per_field() {
        struct Case {
            name: &'static str,
            lines: &'static [(&'static str, &'static str)],
            field: &'static [u8],
            want: Option<&'static str>,
        }
        let cases = [
            Case {
                name: "cookie joins on a semicolon",
                lines: &[
                    ("cookie", "a=1"),
                    ("x-forwarded-for", "1.2.3.4"),
                    ("Cookie", "b=2"),
                ],
                field: b"cookie",
                want: Some("a=1; b=2"),
            },
            Case {
                name: "list field joins on a comma",
                lines: &[
                    ("x-forwarded-for", "1.2.3.4"),
                    ("accept", "text/*"),
                    ("X-Forwarded-For", "5.6.7.8"),
                ],
                field: b"x-forwarded-for",
                want: Some("1.2.3.4, 5.6.7.8"),
            },
            Case {
                name: "single line passes through",
                lines: &[("accept", "text/*")],
                field: b"accept",
                want: Some("text/*"),
            },
            Case {
                name: "authorization keeps the first line",
                lines: &[
                    ("authorization", "Bearer one"),
                    ("Authorization", "Bearer two"),
                ],
                field: b"authorization",
                want: Some("Bearer one"),
            },
            Case {
                name: "content-type keeps the first line",
                lines: &[
                    ("content-type", "text/plain"),
                    ("content-type", "text/html"),
                ],
                field: b"content-type",
                want: Some("text/plain"),
            },
            Case {
                name: "absent field has no value",
                lines: &[("accept", "text/*")],
                field: b"referer",
                want: None,
            },
        ];
        for case in cases {
            let headers = hdrs(case.lines);
            let mut buf = Vec::new();
            let field = HeaderName::from_bytes(case.field).unwrap();
            let got = joined_field(&headers, &field, &mut buf);
            assert_eq!(got, case.want.map(str::as_bytes), "{}", case.name);
        }
    }

    /// The `HTTP_*` registration is last-write-wins. Each name registers once, in the order of its first line.
    #[test]
    fn cgi_header_vars_register_each_name_once() {
        let headers = hdrs(&[
            ("x-forwarded-for", "1.2.3.4"),
            ("cookie", "a=1"),
            ("x-forwarded-for", "5.6.7.8"),
            ("cookie", "b=2"),
        ]);
        let mut got = Vec::new();
        CgiHeaders::new(headers).register(|name, value| {
            got.push((
                name.to_str().unwrap().to_owned(),
                String::from_utf8(value.to_vec()).unwrap(),
            ));
        });
        let want = [
            ("HTTP_X_FORWARDED_FOR", "1.2.3.4, 5.6.7.8"),
            ("HTTP_COOKIE", "a=1; b=2"),
        ]
        .map(|(n, v)| (n.to_owned(), v.to_owned()));
        assert_eq!(got, want);
    }

    /// The http plugin screens field names against `[A-Za-z0-9-]`; that screen is complete only while this mapper rewrites no byte of that set but `-`.
    #[test]
    fn cgi_header_name_rewrites_only_dash_and_dot() {
        struct Case {
            name: &'static str,
            raw: &'static [u8],
            want: &'static [u8],
        }
        let cases = [
            Case {
                name: "dash becomes underscore",
                raw: b"x-foo",
                want: b"HTTP_X_FOO",
            },
            Case {
                name: "underscore stays underscore",
                raw: b"x_foo",
                want: b"HTTP_X_FOO",
            },
            Case {
                name: "dot becomes underscore",
                raw: b"x.foo",
                want: b"HTTP_X_FOO",
            },
            Case {
                name: "tilde stays unchanged",
                raw: b"x~foo",
                want: b"HTTP_X~FOO",
            },
            Case {
                name: "longer name writes full string",
                raw: b"accept-language",
                want: b"HTTP_ACCEPT_LANGUAGE",
            },
            Case {
                name: "shorter name after longer leaves no tail",
                raw: b"accept",
                want: b"HTTP_ACCEPT",
            },
        ];
        for case in cases {
            with_cgi_header_name(case.raw, &mut |name: &CStr| {
                assert_eq!(name.to_bytes(), case.want, "{}", case.name);
            });
        }
    }
}
