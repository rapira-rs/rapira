use crate::context::{ctx, with_ctx};
use crate::diagnostics::syslog_to_level;
use crate::types::{Context, StreamState};
use crate::*;
use core::slice;
use http::header::{
    AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, FROM, HeaderMap, HeaderName, HeaderValue,
    PROXY_AUTHORIZATION, REFERER,
};
use std::ffi::CStr;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::os::raw::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::null_mut;

// Only this catch_unwind turns a panic inside PHP into a failed request and a
// recycled worker instead of a process abort. The build enforces the strategy.
// https://doc.rust-lang.org/reference/conditional-compilation.html#panic
#[cfg(panic = "abort")]
compile_error!("rapira_sapi needs panic = \"unwind\": callbacks::guard relies on catch_unwind");

pub fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!(target: "rapira", "panic caught at the FFI boundary; default substituted");
        default
    })
}

pub(crate) const MAX_BUFFERED_BODY: usize = 1 << 30;

struct SapiHeaders(*mut sapi_headers_struct);

impl SapiHeaders {
    /// http_response_code is an app-controlled c_int; clamping keeps the u16 cast from wrapping.
    fn status(&self) -> u16 {
        let h = unsafe { &*self.0 };
        if h.http_response_code != 0 {
            h.http_response_code.clamp(100, 599) as u16
        } else {
            200
        }
    }

    fn lines(&self) -> impl Iterator<Item = SapiHeader> {
        let mut el: *mut _zend_llist_element = unsafe { &mut *self.0 }.headers.head;
        std::iter::from_fn(move || {
            let e: &_zend_llist_element = unsafe { el.as_ref()? };
            el = e.next;
            Some(SapiHeader(e.data.as_ptr() as *const sapi_header_struct))
        })
    }
}

struct SapiHeader(*const sapi_header_struct);

impl SapiHeader {
    fn name_value(&self) -> Option<(HeaderName, HeaderValue)> {
        let sh = unsafe { &*self.0 };
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

/// Separator joining repeats of `name`; `None` marks a singleton field, where the first line wins and the rest are dropped.
/// Combining is legal only for comma-list fields: https://www.rfc-editor.org/rfc/rfc9110#section-5.3
/// `Cookie` rejoins on `"; "`: https://www.rfc-editor.org/rfc/rfc6265#section-4.2.1
fn field_line_separator(name: &HeaderName) -> Option<&'static [u8]> {
    static SINGLETON: [HeaderName; 6] = [
        AUTHORIZATION,
        PROXY_AUTHORIZATION,
        CONTENT_TYPE,
        CONTENT_LENGTH,
        REFERER,
        FROM,
    ];
    if SINGLETON.contains(name) {
        None
    } else if name == COOKIE {
        Some(b"; ")
    } else {
        Some(b", ")
    }
}

/// Returns the one CGI value of `name`. Repeated lines join into `buf` on the field separator; a single line or a singleton field returns the first line and leaves `buf` unchanged.
pub(crate) fn joined_field<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
    buf: &'a mut Vec<u8>,
) -> Option<&'a [u8]> {
    let mut values = headers.get_all(name).iter().map(HeaderValue::as_bytes);
    let first = values.next()?;
    let (Some(sep), Some(second)) = (field_line_separator(name), values.next()) else {
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

/// Writes the NUL-terminated `HTTP_` meta-variable name of `field` into `buf`.
/// https://www.rfc-editor.org/rfc/rfc3875#section-4.1.18
fn cgi_header_name<'a>(buf: &'a mut Vec<u8>, field: &str) -> &'a CStr {
    buf.clear();
    buf.extend_from_slice(b"HTTP_");
    for &b in field.as_bytes() {
        buf.push(if b == b'-' {
            b'_'
        } else {
            b.to_ascii_uppercase()
        });
    }
    buf.push(0);
    CStr::from_bytes_until_nul(buf).unwrap_or_default()
}

/// php_register_variable_safe is last-write-wins, so the call order is the precedence rule: CONTENT_LENGTH, then HTTP_*.
/// Each name registers once, with its repeats joined.
/// Owned buffers stay in ManuallyDrop because `put` can bail out over this frame.
fn cgi_header_vars(headers: &HeaderMap, content_length: i64, mut put: impl FnMut(&CStr, &[u8])) {
    if content_length >= 0 {
        let len = ManuallyDrop::new(content_length.to_string());
        put(c"CONTENT_LENGTH", len.as_bytes());
        drop(ManuallyDrop::into_inner(len));
    }
    let mut name = ManuallyDrop::new(Vec::new());
    let mut joined = ManuallyDrop::new(Vec::new());
    for field in headers.keys() {
        if let Some(value) = joined_field(headers, field, &mut joined) {
            put(cgi_header_name(&mut name, field.as_str()), value);
        }
    }
    drop(ManuallyDrop::into_inner(joined));
    drop(ManuallyDrop::into_inner(name));
}

/// # Safety
/// `buf` must point at `len` readable bytes and `aborted` at a writable `bool`
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ub_write(
    buf: *const c_char,
    len: usize,
    aborted: *mut bool,
) -> usize {
    let mut completed = false;
    let written = guard(0, || {
        let n = (|| {
            let ctx = unsafe {
                let Some(c) = ctx() else {
                    let data = slice::from_raw_parts(buf.cast::<u8>(), len);
                    tracing::info!(target: "php", "{}", String::from_utf8_lossy(data));
                    return len;
                };
                c
            };

            if ctx.stream == StreamState::NotSent {
                let status = unsafe { SapiHeaders(&mut (*rapira_sg()).sapi_headers).status() };
                ctx.commit_head(status, HeaderMap::new());
            }

            if let Some(tx) = &ctx.sender {
                if tx.is_closed() {
                    ctx.finish(false);
                    unsafe { *aborted = true };
                    return 0;
                }
                if ctx.body.len() + len > MAX_BUFFERED_BODY {
                    tracing::error!(
                        target: "rapira",
                        "response body exceeds the host buffer cap ({} + {len} > {MAX_BUFFERED_BODY} bytes); aborting the request",
                        ctx.body.len()
                    );
                    ctx.finish(true);
                    unsafe { *aborted = true };
                    return 0;
                }
                let buf = unsafe { slice::from_raw_parts(buf.cast::<u8>(), len) };
                ctx.body.extend_from_slice(buf);
                if !ctx.tearing_down {
                    ctx.stream = StreamState::BodyStreamed;
                }
            }

            len
        })();
        completed = true;
        n
    });
    if !completed {
        unsafe { *aborted = true };
    }
    written
}

/// # Safety
/// A SAPI callback invoked by PHP. `h` must be a valid `*mut sapi_headers_struct`
pub unsafe extern "C" fn send_headers(h: *mut sapi_headers_struct) -> c_int {
    guard(SAPI_HEADER_SEND_FAILED as c_int, || {
        let ctx = unsafe {
            let Some(ctx) = ctx() else {
                return SAPI_HEADER_SENT_SUCCESSFULLY as c_int;
            };

            if ctx.stream != StreamState::NotSent {
                return SAPI_HEADER_SENT_SUCCESSFULLY as c_int;
            }

            ctx
        };

        let h = SapiHeaders(h);
        let headers: HeaderMap = h
            .lines()
            .filter_map(|l: SapiHeader| l.name_value())
            .collect();
        ctx.commit_head(h.status(), headers);
        SAPI_HEADER_SENT_SUCCESSFULLY as c_int
    })
}

pub(crate) unsafe extern "C" fn read_post(buf: *mut c_char, count: usize) -> usize {
    with_ctx(0, |ctx| {
        let crate::types::Body::Raw(reader) = &mut ctx.req.body else {
            tracing::debug!(target: "rapira", "read_post on a host-parsed multipart body");
            return 0;
        };
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), count) };
        reader.read(dst).unwrap_or(0)
    })
}
pub(crate) unsafe extern "C" fn read_cookies() -> *mut c_char {
    with_ctx(null_mut(), |ctx| {
        ctx.c
            .as_ref()
            .and_then(|c| c.cookie.as_ref())
            .map_or(null_mut(), |c| c.as_ptr() as *mut c_char)
    })
}
pub(crate) unsafe extern "C" fn register_server_variables(track_vars_array: *mut zval) {
    with_ctx((), |ctx| {
        let Some(reqc) = ctx.c.as_ref() else { return };
        let put_bytes = |name: &CStr, val: &[u8]| unsafe {
            php_register_variable_safe(
                name.as_ptr(),
                val.as_ptr() as *const c_char,
                val.len(),
                track_vars_array,
            );
        };
        let put = |name: &CStr, val: &str| put_bytes(name, val.as_bytes());
        let script = crate::context::script();
        put(c"PHP_SELF", script.script_name);
        let (doc_uri, query) = ctx
            .req
            .uri
            .split_once('?')
            .unwrap_or((ctx.req.uri.as_str(), ""));
        put(c"DOCUMENT_URI", doc_uri);
        put(c"DOCUMENT_ROOT", script.document_root);
        put(
            c"REQUEST_SCHEME",
            if ctx.req.https { "https" } else { "http" },
        );
        put(c"REMOTE_HOST", &reqc.remote_addr);
        put(c"REMOTE_PORT", &reqc.remote_port);
        // REMOTE_IDENT is optional per CGI/1.1; rapira runs no RFC 1413 ident lookup, so it is empty.
        // https://www.rfc-editor.org/rfc/rfc3875#section-4.1.10
        // https://www.rfc-editor.org/rfc/rfc1413
        put(c"REMOTE_IDENT", "");
        put(c"REQUEST_METHOD", &ctx.req.method);
        put(c"REQUEST_URI", &ctx.req.uri);
        put(c"QUERY_STRING", query);
        put_bytes(c"SCRIPT_FILENAME", reqc.script.to_bytes());
        put(c"SCRIPT_NAME", script.script_name);
        put(c"SERVER_PROTOCOL", &ctx.req.protocol);
        put(c"SERVER_SOFTWARE", "Rapira");
        put(c"SERVER_NAME", &ctx.req.server_name);
        put(c"SERVER_PORT", &reqc.server_port);
        put(c"REMOTE_ADDR", &reqc.remote_addr);
        put(c"GATEWAY_INTERFACE", "CGI/1.1");
        put(c"HTTPS", if ctx.req.https { "on" } else { "" });

        let auth_type: &[u8] = ctx
            .req
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| {
                v.as_bytes()
                    .split(|b| b.is_ascii_whitespace())
                    .find(|s| !s.is_empty())
            })
            .unwrap_or(b"");
        put_bytes(c"AUTH_TYPE", auth_type);

        let auth_user = unsafe { (*rapira_sg()).request_info.auth_user };
        if !auth_user.is_null() {
            let user: &CStr = unsafe { CStr::from_ptr(auth_user as *const c_char) };
            put_bytes(c"REMOTE_USER", user.to_bytes());
        }

        if let Some(ct) = &ctx.req.content_type {
            put_bytes(c"CONTENT_TYPE", ct);
        }

        cgi_header_vars(&ctx.req.headers, ctx.req.content_length, put_bytes);
    })
}
pub(crate) unsafe extern "C" fn log_message(message: *const c_char, syslog_type: c_int) {
    guard((), || {
        if message.is_null() {
            return;
        }

        let s = unsafe { CStr::from_ptr(message).to_string_lossy() };
        crate::diagnostics::php_log!(syslog_to_level(syslog_type), "{s}");
    })
}
pub(crate) fn send_error_head(c: &mut Context, status: u16) {
    if c.stream != StreamState::NotSent {
        return;
    }
    c.commit_head(status, HeaderMap::new());
}

pub(crate) fn finalize_response(c: &mut Context, errored: bool) -> bool {
    let truncated = c.is_truncated(errored);
    if errored {
        send_error_head(c, 500);
    }
    truncated
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

    fn map(lines: &[(&str, &str)]) -> HeaderMap {
        lines
            .iter()
            .map(|(k, v)| {
                (
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                )
            })
            .collect()
    }

    /// List fields join on ", ", Cookie on "; ", and a singleton field keeps its first line.
    #[test]
    fn repeated_field_lines_join_per_field() {
        struct Case {
            name: &'static str,
            lines: &'static [(&'static str, &'static str)],
            field: &'static str,
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
                field: "cookie",
                want: Some("a=1; b=2"),
            },
            Case {
                name: "list field joins on a comma",
                lines: &[
                    ("x-forwarded-for", "1.2.3.4"),
                    ("accept", "text/*"),
                    ("X-Forwarded-For", "5.6.7.8"),
                ],
                field: "x-forwarded-for",
                want: Some("1.2.3.4, 5.6.7.8"),
            },
            Case {
                name: "single line passes through",
                lines: &[("accept", "text/*")],
                field: "accept",
                want: Some("text/*"),
            },
            Case {
                name: "authorization keeps the first line",
                lines: &[
                    ("authorization", "Bearer one"),
                    ("Authorization", "Bearer two"),
                ],
                field: "authorization",
                want: Some("Bearer one"),
            },
            Case {
                name: "content-type keeps the first line",
                lines: &[
                    ("content-type", "text/plain"),
                    ("content-type", "text/html"),
                ],
                field: "content-type",
                want: Some("text/plain"),
            },
            Case {
                name: "absent field has no value",
                lines: &[("accept", "text/*")],
                field: "referer",
                want: None,
            },
        ];
        for case in cases {
            let headers = map(case.lines);
            let mut buf = Vec::new();
            let got = joined_field(&headers, &HeaderName::from_static(case.field), &mut buf);
            assert_eq!(got, case.want.map(str::as_bytes), "{}", case.name);
        }
    }

    /// `HTTP_*` registration is last-write-wins: each name registers once, after CONTENT_LENGTH, in the order of its first line.
    #[test]
    fn cgi_header_vars_register_each_name_once() {
        let headers = map(&[
            ("x-forwarded-for", "1.2.3.4"),
            ("cookie", "a=1"),
            ("x-forwarded-for", "5.6.7.8"),
            ("cookie", "b=2"),
        ]);
        let mut got = Vec::new();
        cgi_header_vars(&headers, 5, |name, value| {
            got.push((
                name.to_str().unwrap().to_owned(),
                String::from_utf8(value.to_vec()).unwrap(),
            ));
        });
        let want = [
            ("CONTENT_LENGTH", "5"),
            ("HTTP_X_FORWARDED_FOR", "1.2.3.4, 5.6.7.8"),
            ("HTTP_COOKIE", "a=1; b=2"),
        ]
        .map(|(n, v)| (n.to_owned(), v.to_owned()));
        assert_eq!(got, want);
    }

    /// The front screens field names against `[A-Za-z0-9-]`; that screen is complete only while this mapper rewrites nothing but `-`.
    /// One buffer serves every name of a request, so a shorter name after a longer one must not keep the old tail.
    #[test]
    fn cgi_header_name_rewrites_only_dash() {
        let mut buf = Vec::new();
        assert_eq!(cgi_header_name(&mut buf, "x-foo").to_bytes(), b"HTTP_X_FOO");
        assert_eq!(cgi_header_name(&mut buf, "x_foo").to_bytes(), b"HTTP_X_FOO");
        assert_eq!(cgi_header_name(&mut buf, "x.foo").to_bytes(), b"HTTP_X.FOO");
        assert_eq!(cgi_header_name(&mut buf, "x~foo").to_bytes(), b"HTTP_X~FOO");
        assert_eq!(
            cgi_header_name(&mut buf, "accept-language").to_bytes(),
            b"HTTP_ACCEPT_LANGUAGE"
        );
        assert_eq!(
            cgi_header_name(&mut buf, "accept").to_bytes(),
            b"HTTP_ACCEPT"
        );
    }
}
