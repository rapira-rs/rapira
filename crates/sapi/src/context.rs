use crate::{callbacks::guard, types::Context, *};
use std::{
    ffi::c_char,
    os::raw::c_void,
    path::Path,
    ptr::{null, null_mut},
    sync::{PoisonError, RwLock},
};

/// The script paths of this worker process, the same for every request.
#[derive(Clone, Copy)]
pub(crate) struct ScriptPaths {
    pub(crate) filename: &'static str,
    pub(crate) script_name: &'static str,
    pub(crate) document_root: &'static str,
}

static SCRIPT: RwLock<ScriptPaths> = RwLock::new(ScriptPaths {
    filename: "",
    script_name: "",
    document_root: "",
});

/// Sets the script paths for all later requests. Each call leaks the new strings: a worker process sets them once.
pub(crate) fn set_script(filename: &Path) {
    let document_root = filename
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let script_name = filename
        .file_name()
        .map_or_else(|| "/".to_string(), |f| format!("/{}", f.to_string_lossy()));
    *SCRIPT.write().unwrap_or_else(PoisonError::into_inner) = ScriptPaths {
        filename: filename.to_string_lossy().into_owned().leak(),
        script_name: script_name.leak(),
        document_root: document_root.leak(),
    };
}

pub(crate) fn script() -> ScriptPaths {
    *SCRIPT.read().unwrap_or_else(PoisonError::into_inner)
}

/// # Safety
/// Aliases per-thread `SG(server_context)`; sound only while a worker services one request at a time, and the reference must not be held across another `ctx()` call.
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

/// Also clears the `SG(request_info)` pointers into `job.ctx`, which a panic can recycle before `rapira_request_teardown` runs.
pub(crate) fn unbind_server_context() {
    unsafe {
        let sg = &mut *rapira_sg();
        sg.server_context = null_mut();
        let ri = &mut sg.request_info;
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
    if ctx.c.is_none() {
        return;
    }
    let sg = unsafe { &mut *rapira_sg() };
    sg.request_info.proto_num = match ctx.req.protocol.as_str() {
        "HTTP/1.0" => 1000,
        "HTTP/1.1" => 1001,
        p if p.starts_with("HTTP/2.0") => 2000,
        p if p.starts_with("HTTP/3.0") => 3000,
        _ => 1001,
    };
}

/// Resets `http_response_code` because the engine keeps the previous request's status (reset commented out in main/SAPI.c:435-437).
pub(crate) unsafe fn populate_request_context(ctx: &mut Context) {
    let Some(reqc) = ctx.c.as_ref() else { return };
    let sg = unsafe { &mut *rapira_sg() };
    sg.sapi_headers.http_response_code = 200;
    let ri: &mut sapi_request_info = &mut sg.request_info;
    ri.request_method = reqc.method.as_ptr();
    ri.query_string = reqc.query.as_ptr() as *mut c_char;
    ri.request_uri = reqc.uri.as_ptr() as *mut c_char;
    ri.path_translated = reqc.script.as_ptr() as *mut c_char;
    ri.content_type = reqc.ctype.as_ref().map_or(null(), |s| s.as_ptr());
    ri.content_length = ctx.req.content_length;

    unsafe {
        php_handle_auth_data(
            reqc.authorization
                .as_ref()
                .map_or(null(), |auth| auth.as_ptr()),
        )
    };
}
