//! This module implements sendFile(), with the root check and direct kernel sendfile(2).

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use super::respond::{Verb, closed, discard_unit, emit_head, seal, throw_verb};
use super::*;

/// The real path of the root for the app instance of `cycle`.
struct CycleRoot {
    cycle: u64,
    root: PathBuf,
}

#[derive(Default)]
struct SendfileRoot {
    /// Configured root installed by `Server::worker_init`, with symlinks unresolved.
    configured: Option<PathBuf>,
    /// Canonicalized root for the current app cycle.
    resolved: Option<CycleRoot>,
}

thread_local! {
    static SENDFILE_ROOT: RefCell<SendfileRoot> = const { RefCell::new(SendfileRoot { configured: None, resolved: None }) };
}

/// Sets the sendFile() root on the calling PHP worker thread.
pub(crate) fn set_sendfile_root(root: PathBuf) {
    SENDFILE_ROOT.with_borrow_mut(|state| {
        state.configured = Some(root);
        state.resolved = None;
    });
}

/// Returns true when `canonical` is under the root. The first call of an app instance resolves the root. After a symlink deploy and a reload, the root therefore resolves into the new release.
fn under_root(canonical: &Path) -> bool {
    let cycle = rapira_sapi::exchange::cycle();
    SENDFILE_ROOT.with_borrow_mut(|state| {
        if state.resolved.as_ref().is_none_or(|r| r.cycle != cycle) {
            let root = state
                .configured
                .as_ref()
                .expect("set in Server::worker_init");
            let real = std::fs::canonicalize(root).unwrap_or_else(|e| {
                tracing::warn!(
                    target: "rapira",
                    "sendfile root {} cannot be canonicalized ({e}); sendFile() will reject every path",
                    root.display()
                );
                root.clone()
            });
            state.resolved = Some(CycleRoot { cycle, root: real });
        }
        state
            .resolved
            .as_ref()
            .is_some_and(|r| canonical.starts_with(&r.root))
    })
}

struct FileSlice {
    file: std::fs::File,
    /// The slice length in bytes, checked against the file size.
    len: u64,
}

/// `cwd` is the cwd of PHP for a relative `path`. None resolves `path` against the process cwd.
fn open_send_file(
    path: &[u8],
    cwd: Option<&CStr>,
    offset: u64,
    length: Option<u64>,
) -> Result<FileSlice, &'static CStr> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let path = Path::new(OsStr::from_bytes(path));
    let canonical = match cwd {
        Some(cwd) => std::fs::canonicalize(Path::new(OsStr::from_bytes(cwd.to_bytes())).join(path)),
        None => std::fs::canonicalize(path),
    }
    .map_err(|_| c"no readable file at the path")?;
    if !under_root(&canonical) {
        return Err(c"the path is outside the configured sendfile root");
    }
    let file = std::fs::File::open(&canonical).map_err(|_| c"no readable file at the path")?;
    let meta = file
        .metadata()
        .map_err(|_| c"no readable file at the path")?;
    if !meta.is_file() {
        return Err(c"not a regular file");
    }
    let size = meta.len();
    if offset > size {
        return Err(c"the requested slice runs past the end of the file");
    }
    let len = match length {
        Some(l) => {
            if offset + l > size {
                return Err(c"the requested slice runs past the end of the file");
            }
            l
        }
        None => size - offset,
    };
    Ok(FileSlice { file, len })
}

/// # Safety
/// The engine is active on this thread.
pub(super) unsafe fn send_file_core(
    st: &mut ExchangeState,
    path: &[u8],
    cwd: Option<&CStr>,
    offset: u64,
    length: Option<u64>,
    eos: bool,
) -> Verb {
    if let Some(verb) = closed(st) {
        return verb;
    }
    let FileSlice { file, len } = match open_send_file(path, cwd, offset, length) {
        Ok(opened) => opened,
        Err(msg) => return Verb::FileNotSendable(msg),
    };
    // The slice length is known here, so the call writes nothing when the slice goes past the declared length.
    if st.declared_cl.is_some_and(|cl| st.sent_body + len > cl) {
        return Verb::ContentLengthExceeded;
    }
    let finalizing = (eos && st.sent_body == 0).then_some(len);
    if emit_head(st, finalizing).is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len;
    if len > 0 && !st.bodiless {
        let Some(responder) = st.responder.as_mut() else {
            discard_unit(st);
            return Verb::Discarded;
        };
        if unsafe { responder.send_file(file, offset, len) }.is_err() {
            discard_unit(st);
            return Verb::Discarded;
        }
    }
    if eos {
        unsafe { seal(st) };
    }
    Verb::Ok
}

/// # Safety
/// `held` is the slot of a live exchange. `path` points at `path_len` readable bytes that ZPP owns. `cwd` is null or a NUL-terminated string that stays valid for the call. `offset >= 0`, and `length >= 1` unless `length_is_null`. The engine is active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_send_file(
    held: *const HeldPtr,
    path: *const c_char,
    path_len: usize,
    cwd: *const c_char,
    offset: i64,
    length: i64,
    length_is_null: bool,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *(*held).state::<ExchangeState>();
        let path = std::slice::from_raw_parts(path.cast::<u8>(), path_len);
        let cwd = (!cwd.is_null()).then(|| CStr::from_ptr(cwd));
        let length = (!length_is_null).then_some(length as u64);
        throw_verb(send_file_core(st, path, cwd, offset as u64, length, eos))
    })
}
