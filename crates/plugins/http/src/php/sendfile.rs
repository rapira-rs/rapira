//! sendFile(): the root check and the file slice frame.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::respond::{Verb, discard_unit, send_body, throw_verb};
use super::*;

/// The configured root, with its symlinks.
static SENDFILE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The real path of the root for the app instance of `cycle`.
struct CycleRoot {
    cycle: u64,
    root: PathBuf,
}

thread_local! {
    /// None until the first sendFile() of the PHP thread.
    static CYCLE_ROOT: RefCell<Option<CycleRoot>> = const { RefCell::new(None) };
}

/// Runs once in the master before the fork; the workers inherit the value. A later call changes nothing.
pub(crate) fn set_sendfile_root(root: PathBuf) {
    let _ = SENDFILE_ROOT.set(root);
}

/// Whether `canonical` is under the root. The first call of an app instance resolves the root, so after a symlink deploy and a reload the root follows the code to the new release.
fn under_root(canonical: &Path) -> bool {
    let cycle = rapira_sapi::exchange::cycle();
    CYCLE_ROOT.with_borrow_mut(|resolved| {
        if resolved.as_ref().is_none_or(|r| r.cycle != cycle) {
            let root = SENDFILE_ROOT.get().expect("set in Server::from_settings");
            let real = std::fs::canonicalize(root).unwrap_or_else(|e| {
                tracing::warn!(
                    target: "rapira",
                    "sendfile root {} cannot be canonicalized ({e}); sendFile() will reject every path",
                    root.display()
                );
                root.clone()
            });
            *resolved = Some(CycleRoot { cycle, root: real });
        }
        resolved
            .as_ref()
            .is_some_and(|r| canonical.starts_with(&r.root))
    })
}

struct FileSlice {
    file: std::fs::File,
    /// The slice length in bytes, checked against the file size.
    len: u64,
}

fn open_send_file(
    path: &[u8],
    offset: u64,
    length: Option<u64>,
) -> Result<FileSlice, &'static CStr> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    let canonical = std::fs::canonicalize(path).map_err(|_| c"no readable file at the path")?;
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
/// As `send_frame`.
pub(super) unsafe fn send_file_core(
    st: &mut ExchangeState,
    path: &[u8],
    offset: u64,
    length: Option<u64>,
    eos: bool,
) -> Verb {
    if st.cancelled() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.settled() {
        return Verb::Finalized;
    }
    let FileSlice { file, len } = match open_send_file(path, offset, length) {
        Ok(opened) => opened,
        Err(msg) => return Verb::FileNotSendable(msg),
    };
    // The slice length is known here, so the call writes nothing when the slice goes past the declared length.
    if st.declared_cl.is_some_and(|cl| st.sent_body + len > cl) {
        return Verb::ContentLengthExceeded;
    }
    unsafe {
        send_body(st, len, eos, |n| Frame::File {
            file,
            offset,
            len: n,
        })
    }
}

/// # Safety
/// `held` the slot of a live exchange; `path` points at `path_len` readable bytes (ZPP-owned); `offset >= 0`; `length >= 1` unless `length_is_null`; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_send_file(
    held: *const HeldPtr,
    path: *const c_char,
    path_len: usize,
    offset: i64,
    length: i64,
    length_is_null: bool,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *(*held).state::<ExchangeState>();
        let path = std::slice::from_raw_parts(path.cast::<u8>(), path_len);
        let length = (!length_is_null).then_some(length as u64);
        throw_verb(send_file_core(st, path, offset as u64, length, eos))
    })
}
