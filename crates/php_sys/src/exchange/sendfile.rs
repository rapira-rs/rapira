use std::path::PathBuf;
use std::sync::OnceLock;

use super::respond::{Verb, discard_unit, emit_head, seal, send_frame, throw_verb};
use super::*;

static SENDFILE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The first call sets the root for the process; a later call changes nothing.
pub fn set_sendfile_root(root: PathBuf) {
    let canonical = std::fs::canonicalize(&root).unwrap_or_else(|e| {
        tracing::warn!(
            target: "rapira",
            "sendfile root {} cannot be canonicalized ({e}); sendFile() will reject every path",
            root.display()
        );
        root
    });
    let _ = SENDFILE_ROOT.set(canonical);
}

fn open_send_file(
    path: &[u8],
    offset: u64,
    length: Option<u64>,
) -> Result<(std::fs::File, u64), &'static CStr> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    let canonical = std::fs::canonicalize(path).map_err(|_| c"no readable file at the path")?;
    let Some(root) = SENDFILE_ROOT.get() else {
        return Err(c"no sendfile root is configured");
    };
    if !canonical.starts_with(root) {
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
    Ok((file, len))
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
    if st.host_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    let (file, len) = match open_send_file(path, offset, length) {
        Ok(opened) => opened,
        Err(msg) => return Verb::FileNotSendable(msg),
    };
    if let Some(cl) = st.declared_cl
        && st.sent_body + len > cl
    {
        let fit = cl - st.sent_body;
        if unsafe { emit_head(st, Some(cl)) }.is_ok() && fit > 0 && !st.bodiless {
            let _ = unsafe {
                send_frame(
                    st,
                    Frame::File {
                        file,
                        offset,
                        len: fit,
                    },
                )
            };
        }
        st.sent_body = cl;
        unsafe {
            seal(st, /*truncated=*/ false, Vec::new())
        };
        return Verb::ContentLengthExceeded;
    }
    let finalizing = (eos && st.sent_body == 0).then_some(len);
    if unsafe { emit_head(st, finalizing) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len;
    if len > 0
        && !st.bodiless
        && unsafe { send_frame(st, Frame::File { file, offset, len }) }.is_err()
    {
        discard_unit(st);
        return Verb::Discarded;
    }
    if eos {
        unsafe {
            seal(st, /*truncated=*/ false, Vec::new())
        };
    }
    Verb::Ok
}

/// # Safety
/// `job` from receive; `path` points at `path_len` readable bytes (ZPP-owned); engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_send_file(
    job: *mut c_void,
    path: *const c_char,
    path_len: usize,
    offset: i64,
    length: i64,
    length_is_null: bool,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        if offset < 0 {
            crate::zend_argument_value_error(2, c"must be greater than or equal to 0".as_ptr());
            return false;
        }
        if !length_is_null && length < 1 {
            crate::zend_argument_value_error(3, c"must be greater than or equal to 1".as_ptr());
            return false;
        }
        let st = &mut *job.cast::<ExchangeState>();
        let path = std::slice::from_raw_parts(path.cast::<u8>(), path_len);
        let length = (!length_is_null).then_some(length as u64);
        match send_file_core(st, path, offset as u64, length, eos) {
            Verb::Ok | Verb::Interim => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}
