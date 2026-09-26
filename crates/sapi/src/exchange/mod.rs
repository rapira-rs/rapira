pub(crate) use std::{
    cell::Cell,
    ffi::{CStr, c_char, c_int, c_void},
    path::Path,
    time::Duration,
};

pub(crate) use bytes::Bytes;
pub(crate) use http::header::{HeaderMap, HeaderName, HeaderValue};

pub(crate) use crate::work::{DispatcherClasses, Held, release};
pub(crate) use crate::{
    HashPosition, HashTable, IS_STRING, RAPIRA_MODE_DISPATCHER, add_assoc_zval_ex,
    add_next_index_object,
    callbacks::guard,
    object_init_ex, rapira_array_init, rapira_ce_closed_exception, rapira_ce_inet_address,
    rapira_ce_no_dispatcher_error, rapira_ce_timeout_exception, rapira_ce_unix_address,
    rapira_dispatcher_info_obj, rapira_receive_timed, rapira_receive_untimed,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth, pull_job_try, pull_job_wait},
    types::Addr,
    zend, zend_class_entry, zend_hash_get_current_data_ex, zend_hash_internal_pointer_reset_ex,
    zend_hash_move_forward_ex, zend_object, zval, zval_add_ref, zval_ptr_dtor,
};

mod grpc;
mod receive;

pub(crate) use grpc::{GrpcState, grpc_call_from, is_binary, printable};

thread_local! {
    /// The dispatcher classes of the plugin this PHP thread serves; None outside dispatcher mode.
    static CLASSES: Cell<Option<DispatcherClasses>> = const { Cell::new(None) };
}

pub(crate) fn set_classes(classes: Option<DispatcherClasses>) {
    CLASSES.set(classes);
}

fn classes() -> DispatcherClasses {
    CLASSES
        .get()
        .expect("a dispatcher-mode worker started with no DispatcherClasses")
}

/// Clears the cycle slot if it still points at `ptr`.
pub(crate) fn forget_held(ptr: *const ()) {
    update(|c| {
        if c.unit.is_some_and(|u| std::ptr::addr_eq(u, ptr)) {
            c.unit = None;
        }
    });
}

#[derive(Clone, Copy)]
struct CycleState {
    /// The Box pointer of the unit handed out last, so paths where free_obj never runs (bailout) can still reclaim it.
    unit: Option<*mut dyn Held>,
    closed_seen: bool,
    served: bool,
    /// A unit was handed out this cycle: a fatal after that is an app failure, not a boot failure.
    received: bool,
}

const CYCLE_IDLE: CycleState = CycleState {
    unit: None,
    closed_seen: false,
    served: false,
    received: false,
};

thread_local! {
    static CYCLE: Cell<CycleState> = const { Cell::new(CYCLE_IDLE) };
}

fn update(f: impl FnOnce(&mut CycleState)) {
    let mut c = CYCLE.get();
    f(&mut c);
    CYCLE.set(c);
}

pub(crate) fn cycle_reset() {
    reclaim_current();
    CYCLE.set(CYCLE_IDLE);
}

/// Reclaim a unit free_obj never saw (shutdown bailout / allocation bailout).
pub(crate) fn reclaim_current() {
    if let Some(ptr) = CYCLE.get().unit {
        // SAFETY: the pointer came from Box::into_raw in receive, and rapira_rs_exchange_drop clears the unit before it reclaims.
        drop(unsafe { release(ptr) });
    }
}

pub(crate) fn closed_seen() -> bool {
    CYCLE.get().closed_seen
}

pub(crate) fn note_closed() {
    update(|c| c.closed_seen = true);
}

pub(crate) fn note_received() {
    update(|c| c.received = true);
}

pub fn note_served() {
    update(|c| c.served = true);
}

pub(crate) fn served_any() -> bool {
    CYCLE.get().served
}

pub(crate) fn received_any() -> bool {
    CYCLE.get().received
}

pub enum AddrOwned {
    Inet {
        ip: String,
        port: u16,
    },
    /// None = unnamed endpoint.
    Unix(Option<Vec<u8>>),
}

pub fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

impl AddrOwned {
    pub fn new(a: &Addr) -> Self {
        match a {
            Addr::Inet(sa) => Self::Inet {
                ip: sa.ip().to_string(),
                port: sa.port(),
            },
            Addr::Unix(p) => Self::Unix(p.as_deref().map(path_bytes).filter(|b| !b.is_empty())),
        }
    }
}

/// add_assoc_zval_ex moves the list ref in: the hash-update family never addrefs.
/// # Safety
/// `dst` a live array; engine active on this thread.
pub unsafe fn add_list<'v>(
    dst: *mut zval,
    key: *const c_char,
    key_len: usize,
    values: impl Iterator<Item = &'v [u8]>,
) {
    unsafe {
        let mut list: zval = std::mem::zeroed();
        rapira_array_init(&mut list, values.size_hint().0 as u32);
        for v in values {
            zend::list_push_stringl(&mut list, v);
        }
        add_assoc_zval_ex(dst, key, key_len, &mut list);
    }
}

/// The key pointer of a header name for add_assoc_zval_ex.
/// The symtable prefilter in add_assoc_zval_ex reads the byte after a leading `-`. For the name "-" that byte is past the name, so a NUL-terminated copy replaces it.
pub fn header_key(name: &str) -> *const c_char {
    if name == "-" {
        c"-".as_ptr()
    } else {
        name.as_ptr().cast()
    }
}

/// # Safety
/// `dst` writable; engine active on this thread.
pub unsafe fn build_address(dst: *mut zval, addr: &AddrOwned) {
    unsafe {
        match addr {
            AddrOwned::Inet { ip, port } => {
                let ce = rapira_ce_inet_address;
                let _ = object_init_ex(dst, ce);
                let o = (*dst).value.obj;
                zend::prop_stringl(ce, o, c"ip", ip.as_bytes());
                zend::prop_long(ce, o, c"port", i64::from(*port));
            }
            AddrOwned::Unix(path) => {
                let ce = rapira_ce_unix_address;
                let _ = object_init_ex(dst, ce);
                zend::prop_str_or_null(ce, (*dst).value.obj, c"path", path.as_deref());
            }
        }
    }
}

/// `fn $name(obj) -> *mut $t` recovers the enclosing C struct: the C fields sit before `std` (the layouts in rapira_sapi.h, rapira_http.h and rapira_grpc.h).
#[macro_export]
macro_rules! container_of {
    ($vis:vis $name:ident, $t:ty) => {
        $vis unsafe fn $name(obj: *mut $crate::zend_object) -> *mut $t {
            unsafe { obj.byte_sub(std::mem::offset_of!($t, std)).cast() }
        }
    };
}
pub(crate) use container_of;

container_of!(info_from, rapira_dispatcher_info_obj);
