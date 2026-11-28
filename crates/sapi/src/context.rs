use crate::{callbacks::guard, types::CgiExchange, *};
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::{CStr, c_char},
    fmt::NumBuffer,
    os::{raw::c_void, unix::ffi::OsStrExt},
    path::Path,
    ptr::{null, null_mut},
    sync::{Mutex, OnceLock},
};

/// The entrypoint paths of the worker since its last boot or classic reload.
#[derive(Clone, Copy)]
pub(crate) struct ScriptPaths {
    /// `SCRIPT_FILENAME`, `SCRIPT_NAME` and `DOCUMENT_ROOT` of `$_SERVER`, as permanent interned strings.
    pub(crate) server_filename: Permanent,
    pub(crate) server_script_name: Permanent,
    pub(crate) server_document_root: Permanent,
}

impl ScriptPaths {
    pub(crate) fn filename(&self) -> &CStr {
        // SAFETY: PHP terminates the permanent string with NUL. The configured path passed the file check, and a path from the file system has no NUL.
        unsafe { CStr::from_ptr((*self.server_filename.0).val.as_ptr()) }
    }
}

/// Declares `ServerStrings` and its constructor. The constructor makes one permanent interned string for each field.
macro_rules! server_strings {
    ($($field:ident: $text:literal,)*) => {
        /// The fixed `$_SERVER` names and values. All workers share their precomputed hashes.
        pub(crate) struct ServerStrings {
            $(pub(crate) $field: Permanent,)*
        }

        impl ServerStrings {
            fn intern() -> Self {
                Self {
                    $($field: Permanent::intern($text),)*
                }
            }
        }
    };
}

server_strings! {
    // The fixed names, in the order of register_server_variables.
    php_self: b"PHP_SELF",
    document_uri: b"DOCUMENT_URI",
    document_root: b"DOCUMENT_ROOT",
    request_scheme: b"REQUEST_SCHEME",
    remote_host: b"REMOTE_HOST",
    remote_port: b"REMOTE_PORT",
    remote_ident: b"REMOTE_IDENT",
    request_method: b"REQUEST_METHOD",
    request_uri: b"REQUEST_URI",
    query_string: b"QUERY_STRING",
    script_filename: b"SCRIPT_FILENAME",
    script_name: b"SCRIPT_NAME",
    server_protocol: b"SERVER_PROTOCOL",
    server_software: b"SERVER_SOFTWARE",
    server_name: b"SERVER_NAME",
    server_port: b"SERVER_PORT",
    remote_addr: b"REMOTE_ADDR",
    gateway_interface: b"GATEWAY_INTERFACE",
    https: b"HTTPS",
    auth_type: b"AUTH_TYPE",
    remote_user: b"REMOTE_USER",
    content_type: b"CONTENT_TYPE",
    content_length: b"CONTENT_LENGTH",
    // The values.
    rapira: b"Rapira",
    cgi_1_1: b"CGI/1.1",
    scheme_http: b"http",
    scheme_https: b"https",
    on: b"on",
    http_1_0: b"HTTP/1.0",
    http_1_1: b"HTTP/1.1",
    empty: b"",
    slash: b"/",
    zero: b"0",
    method_get: b"GET",
    method_post: b"POST",
    method_put: b"PUT",
    method_delete: b"DELETE",
    method_head: b"HEAD",
    method_options: b"OPTIONS",
    method_patch: b"PATCH",
    // Common HTTP_* meta-variable names.
    http_host: b"HTTP_HOST",
    http_user_agent: b"HTTP_USER_AGENT",
    http_accept: b"HTTP_ACCEPT",
    http_accept_language: b"HTTP_ACCEPT_LANGUAGE",
    http_accept_encoding: b"HTTP_ACCEPT_ENCODING",
    http_connection: b"HTTP_CONNECTION",
    http_content_type: b"HTTP_CONTENT_TYPE",
    http_content_length: b"HTTP_CONTENT_LENGTH",
    http_cookie: b"HTTP_COOKIE",
    http_authorization: b"HTTP_AUTHORIZATION",
    http_referer: b"HTTP_REFERER",
    http_origin: b"HTTP_ORIGIN",
    http_cache_control: b"HTTP_CACHE_CONTROL",
    http_pragma: b"HTTP_PRAGMA",
    http_x_forwarded_for: b"HTTP_X_FORWARDED_FOR",
    http_x_forwarded_proto: b"HTTP_X_FORWARDED_PROTO",
    http_x_forwarded_host: b"HTTP_X_FORWARDED_HOST",
    http_x_real_ip: b"HTTP_X_REAL_IP",
    http_x_requested_with: b"HTTP_X_REQUESTED_WITH",
    http_if_none_match: b"HTTP_IF_NONE_MATCH",
    http_if_modified_since: b"HTTP_IF_MODIFIED_SINCE",
    http_upgrade_insecure_requests: b"HTTP_UPGRADE_INSECURE_REQUESTS",
    http_sec_fetch_dest: b"HTTP_SEC_FETCH_DEST",
    http_sec_fetch_mode: b"HTTP_SEC_FETCH_MODE",
    http_sec_fetch_site: b"HTTP_SEC_FETCH_SITE",
    http_sec_fetch_user: b"HTTP_SEC_FETCH_USER",
    http_dnt: b"HTTP_DNT",
    http_sec_gpc: b"HTTP_SEC_GPC",
}

impl ServerStrings {
    /// Returns the pre-interned `HTTP_*` meta-variable name for an ASCII header name (case-insensitive).
    pub(crate) fn http_header_ci(&self, field: &[u8]) -> Option<Permanent> {
        let mut lower = [0u8; 32];
        if field.len() > lower.len() {
            return None;
        }
        for (dst, &b) in lower[..field.len()].iter_mut().zip(field) {
            *dst = b.to_ascii_lowercase();
        }
        match &lower[..field.len()] {
            b"host" => Some(self.http_host),
            b"user-agent" => Some(self.http_user_agent),
            b"accept" => Some(self.http_accept),
            b"accept-language" => Some(self.http_accept_language),
            b"accept-encoding" => Some(self.http_accept_encoding),
            b"connection" => Some(self.http_connection),
            b"content-type" => Some(self.http_content_type),
            b"content-length" => Some(self.http_content_length),
            b"cookie" => Some(self.http_cookie),
            b"authorization" => Some(self.http_authorization),
            b"referer" => Some(self.http_referer),
            b"origin" => Some(self.http_origin),
            b"cache-control" => Some(self.http_cache_control),
            b"pragma" => Some(self.http_pragma),
            b"x-forwarded-for" => Some(self.http_x_forwarded_for),
            b"x-forwarded-proto" => Some(self.http_x_forwarded_proto),
            b"x-forwarded-host" => Some(self.http_x_forwarded_host),
            b"x-real-ip" => Some(self.http_x_real_ip),
            b"x-requested-with" => Some(self.http_x_requested_with),
            b"if-none-match" => Some(self.http_if_none_match),
            b"if-modified-since" => Some(self.http_if_modified_since),
            b"upgrade-insecure-requests" => Some(self.http_upgrade_insecure_requests),
            b"sec-fetch-dest" => Some(self.http_sec_fetch_dest),
            b"sec-fetch-mode" => Some(self.http_sec_fetch_mode),
            b"sec-fetch-site" => Some(self.http_sec_fetch_site),
            b"sec-fetch-user" => Some(self.http_sec_fetch_user),
            b"dnt" => Some(self.http_dnt),
            b"sec-gpc" => Some(self.http_sec_gpc),
            _ => None,
        }
    }

    pub(crate) fn method(&self, method: &[u8]) -> Option<Permanent> {
        match method {
            b"GET" => Some(self.method_get),
            b"POST" => Some(self.method_post),
            b"PUT" => Some(self.method_put),
            b"DELETE" => Some(self.method_delete),
            b"HEAD" => Some(self.method_head),
            b"OPTIONS" => Some(self.method_options),
            b"PATCH" => Some(self.method_patch),
            _ => None,
        }
    }
}

/// The HTTP pool identity shared by the dispatcher and CGI.
#[derive(Clone, Copy)]
pub struct HttpIdentity {
    pub port: u16,
    pub(crate) server_name: Permanent,
    pub(crate) server_port: Permanent,
}

static SERVER_STRINGS: OnceLock<ServerStrings> = OnceLock::new();

thread_local! {
    pub(crate) static SCRIPT: Cell<Option<ScriptPaths>> = const { Cell::new(None) };
    static SERVER_IDENTITY: Cell<Option<HttpIdentity>> = const { Cell::new(None) };
}

#[derive(Clone, Copy)]
pub struct Permanent(*mut zend_string);

// SAFETY: the bytes, flags and hash stay immutable for the life of the process.
unsafe impl Send for Permanent {}
unsafe impl Sync for Permanent {}

impl Permanent {
    /// Interns `bytes` as a process-lifetime Zend string shared across workers. PHP's permanent intern table is read-only during requests. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_string.c#L265-L280
    pub fn intern(bytes: &[u8]) -> Self {
        let mut table = PERMANENT.lock().unwrap();
        if let Some(s) = table.get(bytes) {
            return *s;
        }
        // SAFETY: the shim copies the bytes and computes the hash. The persistent allocator calls zend_out_of_memory() if malloc fails. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_alloc.c#L4069-L4076
        // zend_out_of_memory() aborts the process. https://github.com/php/php-src/blob/php-8.6.0/Zend/zend_alloc.c#L3403-L3407
        let s = Permanent(unsafe {
            rapira_permanent_string(crate::zend::ptr_or_empty(bytes), bytes.len())
        });
        table.insert(bytes.into(), s);
        s
    }

    pub fn as_ptr(self) -> *mut zend_string {
        self.0
    }

    /// A zval of the string. PHP counts no references to an interned string, so the zval needs no destructor.
    pub(crate) fn zval(self) -> zval {
        // SAFETY: zval is plain data, and all zero bits are a valid value of it.
        let mut v: zval = unsafe { std::mem::zeroed() };
        v.value.str_ = self.0;
        v.u1.type_info = IS_STRING;
        v
    }
}

static PERMANENT: Mutex<BTreeMap<Box<[u8]>, Permanent>> = Mutex::new(BTreeMap::new());

/// The engine initializes the fixed strings before it starts any workers.
pub(crate) fn intern_server_strings() {
    SERVER_STRINGS.get_or_init(ServerStrings::intern);
}

pub(crate) fn server_strings() -> &'static ServerStrings {
    SERVER_STRINGS
        .get()
        .expect("the engine has initialized the server strings")
}

impl HttpIdentity {
    pub fn new(name: &str, port: u16) -> Self {
        Self {
            port,
            server_name: Permanent::intern(name.as_bytes()),
            server_port: Permanent::intern(port.format_into(&mut NumBuffer::new()).as_bytes()),
        }
    }

    pub fn enter(self) {
        SERVER_IDENTITY.set(Some(self));
    }

    pub fn current() -> Self {
        SERVER_IDENTITY
            .get()
            .expect("the HTTP worker has its pool identity")
    }

    pub fn name(self) -> &'static str {
        // SAFETY: new() copies a UTF-8 name into an immutable process-lifetime Zend string.
        unsafe { std::str::from_utf8_unchecked(zend::zstr_bytes(self.server_name.as_ptr())) }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_http_server_name() -> *mut zend_string {
    HttpIdentity::current().server_name.as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_http_server_port() -> zend_long {
    HttpIdentity::current().port.into()
}

/// Resolves the configured entrypoint to its real path and gives that path to PHP: the script file, `SCRIPT_FILENAME`, `DOCUMENT_ROOT` and the boot argv. OPcache then keys the entrypoint by the path of the release, so a symlink deploy and a reload run the new release. A path that does not resolve stays as configured.
/// Each app boot and each classic reload calls it. The same real path keeps the current paths, so only a deploy builds new permanent strings.
pub(crate) fn resolve_script(configured: &Path) {
    let real = std::fs::canonicalize(configured).unwrap_or_else(|e| {
        tracing::error!(target: "rapira", "resolving the entrypoint {}: {e}", configured.display());
        configured.to_path_buf()
    });
    if SCRIPT
        .get()
        .is_some_and(|paths| paths.filename().to_bytes() == real.as_os_str().as_bytes())
    {
        return;
    }
    let document_root = real.parent().map_or(&b""[..], |p| p.as_os_str().as_bytes());
    let script_name = configured
        .file_name()
        .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
    SCRIPT.set(Some(ScriptPaths {
        server_filename: Permanent::intern(real.as_os_str().as_bytes()),
        server_script_name: Permanent::intern(script_name.as_bytes()),
        server_document_root: Permanent::intern(document_root),
    }));
}

pub(crate) fn script() -> ScriptPaths {
    SCRIPT
        .get()
        .expect("the worker has resolved its entrypoint")
}

/// Empties the realpath cache of PHP, so a path through a switched symlink resolves again. A cache entry lives `realpath_cache_ttl` seconds, 120 by default.
pub(crate) fn clean_realpath_cache() {
    // SAFETY: the PHP thread calls it. The realpath cache is per thread on ZTS, and on NTS only the PHP thread uses it.
    unsafe { realpath_cache_clean() };
}

/// # Safety
/// The result aliases the per-thread `SG(server_context)`. It is sound only while a worker serves one request at a time. Do not hold the reference across another `ctx()` call.
pub unsafe fn ctx<'a>() -> Option<&'a mut CgiExchange> {
    unsafe { ((*rapira_sg()).server_context as *mut CgiExchange).as_mut() }
}

pub(crate) fn with_ctx<T: Copy>(default: T, f: impl FnOnce(&mut CgiExchange) -> T) -> T {
    guard(default, move || match unsafe { ctx() } {
        Some(ctx) => f(ctx),
        None => default,
    })
}

pub(crate) fn bind_server_context(ctx: &mut CgiExchange) {
    unsafe {
        (*rapira_sg()).server_context = (ctx as *mut CgiExchange) as *mut c_void;
    }
}

/// Clears SG(server_context) and the SG(request_info) pointers into the unit.
pub(crate) fn unbind_server_context() {
    unsafe {
        let sg = rapira_sg();
        (*sg).server_context = null_mut();
        let ri = &mut (*sg).request_info;
        ri.request_method = null();
        ri.query_string = null_mut();
        ri.request_uri = null_mut();
        ri.path_translated = null_mut();
        ri.content_type = null();
        ri.cookie_data = null_mut();
    }
}

/// Must run after sapi_activate, which resets proto_num to 1000 (main/SAPI.c:448).
pub(crate) unsafe fn apply_proto_num(ctx: &CgiExchange) {
    let proto_num = match ctx.req().protocol {
        "HTTP/1.0" => 1000,
        "HTTP/2.0" => 2000,
        _ => 1001,
    };
    unsafe { (*rapira_sg()).request_info.proto_num = proto_num };
}

/// Resets `http_response_code`, because the engine keeps the status of the previous request. The reset in main/SAPI.c:435-437 is commented out.
pub(crate) unsafe fn populate_request_context(ctx: &mut CgiExchange) {
    let reqc = ctx.req();
    let sg = unsafe { rapira_sg() };
    unsafe { (*sg).sapi_headers.http_response_code = 200 };
    let ri: &mut sapi_request_info = unsafe { &mut (*sg).request_info };
    ri.request_method = reqc.method.as_ptr();
    ri.query_string = reqc.query.as_ptr() as *mut c_char;
    ri.request_uri = reqc.uri.as_ptr() as *mut c_char;
    ri.path_translated = script().filename().as_ptr() as *mut c_char;
    ri.content_type = reqc.ctype.as_ref().map_or(null(), |s| s.as_ptr());
    ri.content_length = reqc.body.len() as i64;
}
