use crate::{callbacks::guard, types::Context, *};
use std::{
    cell::Cell,
    collections::BTreeMap,
    ffi::{CString, c_char},
    fmt::NumBuffer,
    os::{raw::c_void, unix::ffi::OsStrExt},
    path::Path,
    ptr::{null, null_mut},
};

/// The script paths of the current app instance. In classic mode, they are the paths of the requests since the worker start or the last reload.
pub(crate) struct ScriptPaths {
    /// `SG(request_info).path_translated` points at it while a unit runs.
    pub(crate) filename: CString,
    pub(crate) document_root: String,
    /// `SCRIPT_FILENAME`, `SCRIPT_NAME` and `DOCUMENT_ROOT` of `$_SERVER`, as permanent interned strings.
    pub(crate) server_filename: *mut zend_string,
    pub(crate) server_script_name: *mut zend_string,
    pub(crate) server_document_root: *mut zend_string,
}

/// Declares `ServerStrings` and its constructor. The constructor makes one permanent interned string for each field.
macro_rules! server_strings {
    ($($field:ident: $text:literal,)*) => {
        /// The `$_SERVER` strings that are the same for every request of the worker, as permanent interned strings. PHP inserts each one with no hash, no allocation and no copy. They live until the worker process exits.
        pub(crate) struct ServerStrings {
            $(pub(crate) $field: *mut zend_string,)*
        }

        impl ServerStrings {
            fn intern() -> Self {
                Self {
                    $($field: permanent($text),)*
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
}

/// `SERVER_NAME` and `SERVER_PORT` as permanent interned strings, with the `[http]` values they come from.
#[derive(Clone, Copy)]
pub(crate) struct ServerIdentity {
    name: &'static str,
    port: u16,
    pub(crate) server_name: *mut zend_string,
    pub(crate) server_port: *mut zend_string,
}

thread_local! {
    /// A box from `resolve_script`, or null. Only the PHP thread sets and reads it, and it sets it between units only.
    static SCRIPT: Cell<*mut ScriptPaths> = const { Cell::new(null_mut()) };
    /// The env pairs of the pool as permanent interned strings, from `intern_server_env`.
    pub(crate) static SERVER_ENV: Cell<&'static [(*mut zend_string, *mut zend_string)]> = const { Cell::new(&[]) };
    /// Set by `intern_server_strings`.
    static SERVER_STRINGS: Cell<Option<&'static ServerStrings>> = const { Cell::new(None) };
    /// The identity strings of the last request, from `server_identity`.
    static SERVER_IDENTITY: Cell<Option<ServerIdentity>> = const { Cell::new(None) };
}

/// Returns a permanent interned copy of `bytes`. PHP never refcounts or frees it, so it lives until the worker process exits, and the process exits without module teardown.
fn permanent(bytes: &[u8]) -> *mut zend_string {
    // SAFETY: the pointer and the length come from a live slice, and the shim copies the bytes.
    unsafe { rapira_permanent_string(bytes.as_ptr().cast(), bytes.len()) }
}

/// Makes the env pairs of the pool into permanent interned strings once, for the `$_SERVER` of each request.
pub(crate) fn intern_server_env(env: &BTreeMap<String, String>) {
    let rows: Box<[_]> = env
        .iter()
        .map(|(name, value)| (permanent(name.as_bytes()), permanent(value.as_bytes())))
        .collect();
    SERVER_ENV.set(Box::leak(rows));
}

/// Builds the `ServerStrings` of this worker once, on the PHP thread.
pub(crate) fn intern_server_strings() {
    SERVER_STRINGS.set(Some(Box::leak(Box::new(ServerStrings::intern()))));
}

pub(crate) fn server_strings() -> &'static ServerStrings {
    SERVER_STRINGS
        .get()
        .expect("the PHP thread interns the server strings before the first request")
}

/// The identity strings for the server name and port of a request. Every request of a worker carries the same pair, so the first request builds the strings and the next requests reuse them.
pub(crate) fn server_identity(name: &'static str, port: u16) -> ServerIdentity {
    let cached = SERVER_IDENTITY
        .get()
        .filter(|identity| identity.name == name && identity.port == port);
    if let Some(identity) = cached {
        return identity;
    }
    let identity = ServerIdentity {
        name,
        port,
        server_name: permanent(name.as_bytes()),
        server_port: permanent(port.format_into(&mut NumBuffer::new()).as_bytes()),
    };
    SERVER_IDENTITY.set(Some(identity));
    identity
}

/// Resolves the configured entrypoint to its real path and gives that path to PHP: the script file, `SCRIPT_FILENAME`, `DOCUMENT_ROOT` and the boot argv. OPcache then keys the entrypoint by the path of the release, so a symlink deploy and a reload run the new release. A path that does not resolve stays as configured.
/// Each app boot and each classic reload calls it. The same real path keeps the current paths, so only a deploy builds new permanent strings.
pub(crate) fn resolve_script(configured: &Path) {
    let real = std::fs::canonicalize(configured).unwrap_or_else(|e| {
        tracing::error!(target: "rapira", "resolving the entrypoint {}: {e}", configured.display());
        configured.to_path_buf()
    });
    // SAFETY: the pointer is null or a live box from this function.
    let current = unsafe { SCRIPT.get().as_ref() };
    if current.is_some_and(|paths| paths.filename.as_bytes() == real.as_os_str().as_bytes()) {
        return;
    }
    let document_root = real
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let script_name = configured
        .file_name()
        .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
    let filename = CString::new(real.as_os_str().as_bytes()).unwrap_or_default();
    let paths = Box::new(ScriptPaths {
        server_filename: permanent(filename.as_bytes()),
        server_script_name: permanent(script_name.as_bytes()),
        server_document_root: permanent(document_root.as_bytes()),
        filename,
        document_root,
    });
    let old = SCRIPT.replace(Box::into_raw(paths));
    if !old.is_null() {
        // SAFETY: old came from Box::into_raw above. The PHP thread replaces the paths only between units, and no reader keeps the reference past its unit.
        drop(unsafe { Box::from_raw(old) });
    }
}

/// Valid until the next `resolve_script` on this thread.
pub(crate) fn script() -> &'static ScriptPaths {
    // SAFETY: the pointer is null or a live box from resolve_script.
    unsafe { SCRIPT.get().as_ref() }
        .expect("the PHP thread resolves the script paths before the first request")
}

/// Enters the directory of the resolved entrypoint and empties the realpath cache of PHP, so a path through a switched symlink resolves again. A cache entry lives `realpath_cache_ttl` seconds, 120 by default.
pub(crate) fn enter_entrypoint_dir() {
    let dir = &script().document_root;
    if let Err(e) = std::env::set_current_dir(dir) {
        tracing::error!(target: "rapira", "entering the entrypoint directory {dir}: {e}");
    }
    // SAFETY: NTS keeps one realpath cache per process, and only the PHP thread uses it.
    unsafe { realpath_cache_clean() };
}

/// # Safety
/// The result aliases the per-thread `SG(server_context)`. It is sound only while a worker serves one request at a time. Do not hold the reference across another `ctx()` call.
pub unsafe fn ctx<'a>() -> Option<&'a mut Context> {
    unsafe { ((*rapira_sg()).server_context as *mut Context).as_mut() }
}

pub fn with_ctx<T: Copy>(default: T, f: impl FnOnce(&mut Context) -> T) -> T {
    guard(default, move || match unsafe { ctx() } {
        Some(ctx) => f(ctx),
        None => default,
    })
}

pub(crate) fn bind_server_context(ctx: &mut Context) {
    unsafe {
        (*rapira_sg()).server_context = (ctx as *mut Context) as *mut c_void;
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
pub(crate) unsafe fn apply_proto_num(ctx: &Context) {
    let proto_num = match ctx.req.protocol {
        "HTTP/1.0" => 1000,
        "HTTP/2.0" => 2000,
        _ => 1001,
    };
    unsafe { (*rapira_sg()).request_info.proto_num = proto_num };
}

/// Resets `http_response_code`, because the engine keeps the status of the previous request. The reset in main/SAPI.c:435-437 is commented out.
pub(crate) unsafe fn populate_request_context(ctx: &mut Context) {
    let reqc = &ctx.c;
    let sg = unsafe { rapira_sg() };
    unsafe { (*sg).sapi_headers.http_response_code = 200 };
    let ri: &mut sapi_request_info = unsafe { &mut (*sg).request_info };
    ri.request_method = reqc.method.as_ptr();
    ri.query_string = reqc.query.as_ptr() as *mut c_char;
    ri.request_uri = reqc.uri.as_ptr() as *mut c_char;
    ri.path_translated = script().filename.as_ptr() as *mut c_char;
    ri.content_type = reqc.ctype.as_ref().map_or(null(), |s| s.as_ptr());
    ri.content_length = ctx.req.body.len() as i64;
}
