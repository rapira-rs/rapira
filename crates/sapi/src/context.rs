use crate::{callbacks::guard, types::Context, *};
use std::{
    cell::Cell,
    ffi::{CString, c_char},
    os::{raw::c_void, unix::ffi::OsStrExt},
    path::Path,
    ptr::{null, null_mut},
};

/// The script paths of the current app instance. In classic mode, the paths of the requests since the worker start or the last reload.
pub(crate) struct ScriptPaths {
    /// `SG(request_info).path_translated` points at it while a unit runs.
    pub(crate) filename: CString,
    pub(crate) script_name: String,
    pub(crate) document_root: String,
}

thread_local! {
    /// A box from `resolve_script`, or null. Only the PHP thread sets and reads it, and it sets it between units only.
    static SCRIPT: Cell<*mut ScriptPaths> = const { Cell::new(null_mut()) };
}

/// Resolves the configured entrypoint to its real path and gives that path to PHP: the script file, `SCRIPT_FILENAME`, `DOCUMENT_ROOT` and the boot argv. OPcache then keys the entrypoint by the path of the release, so a symlink deploy and a reload run the new release. A path that does not resolve stays as configured.
pub(crate) fn resolve_script(configured: &Path) {
    let real = std::fs::canonicalize(configured).unwrap_or_else(|e| {
        tracing::error!(target: "rapira", "resolving the entrypoint {}: {e}", configured.display());
        configured.to_path_buf()
    });
    let document_root = real
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let script_name = real
        .file_name()
        .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
    let paths = Box::new(ScriptPaths {
        filename: CString::new(real.as_os_str().as_bytes()).unwrap_or_default(),
        script_name,
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
/// Aliases per-thread `SG(server_context)`; sound only while a worker services one request at a time, and the reference must not be held across another `ctx()` call.
pub unsafe fn ctx<'a>() -> Option<&'a mut Context> {
    unsafe { (sapi_globals.server_context as *mut Context).as_mut() }
}

pub fn with_ctx<T: Copy>(default: T, f: impl FnOnce(&mut Context) -> T) -> T {
    guard(default, move || match unsafe { ctx() } {
        Some(ctx) => f(ctx),
        None => default,
    })
}

pub(crate) fn bind_server_context(ctx: &mut Context) {
    unsafe {
        sapi_globals.server_context = (ctx as *mut Context) as *mut c_void;
    }
}

/// Clears SG(server_context) and the SG(request_info) pointers into the unit.
pub(crate) fn unbind_server_context() {
    unsafe {
        let sg = &raw mut sapi_globals;
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

/// Must run after sapi_activate, which hard-resets proto_num to 1000 (main/SAPI.c:448).
pub(crate) unsafe fn apply_proto_num(ctx: &Context) {
    let proto_num = match ctx.req.protocol {
        "HTTP/1.0" => 1000,
        "HTTP/2.0" => 2000,
        _ => 1001,
    };
    unsafe { sapi_globals.request_info.proto_num = proto_num };
}

/// Resets `http_response_code` because the engine keeps the previous request's status (reset commented out in main/SAPI.c:435-437).
pub(crate) unsafe fn populate_request_context(ctx: &mut Context) {
    let reqc = &ctx.c;
    let sg = &raw mut sapi_globals;
    unsafe { (*sg).sapi_headers.http_response_code = 200 };
    let ri: &mut sapi_request_info = unsafe { &mut (*sg).request_info };
    ri.request_method = reqc.method.as_ptr();
    ri.query_string = reqc.query.as_ptr() as *mut c_char;
    ri.request_uri = reqc.uri.as_ptr() as *mut c_char;
    ri.path_translated = script().filename.as_ptr() as *mut c_char;
    ri.content_type = reqc.ctype.as_ref().map_or(null(), |s| s.as_ptr());
    ri.content_length = ctx.req.body.len() as i64;
}
