use std::{
    ffi::c_char,
    fmt::NumBuffer,
    net::{IpAddr, Ipv4Addr},
    path::Path,
    sync::OnceLock,
};

use crate::{
    IS_OBJECT, add_assoc_zval_ex, callbacks::guard, object_init_ex, rapira_array_init,
    rapira_ce_inet_address, rapira_ce_tls, rapira_ce_unix_address, types::Addr, zend, zend_object,
    zend_string, zend_zval_value_name, zval,
};

/// Checks the `Rapira\InetAddress|Rapira\UnixAddress` union that arginfo cannot enforce (internal arg types are debug-only).
/// # Safety
/// `zv` a live zval; engine active on this thread.
pub unsafe fn address_arg(zv: *mut zval, num: u32) -> bool {
    unsafe {
        if zend::zval_type(zv) == IS_OBJECT {
            let ce = (*(*zv).value.obj).ce;
            if zend::instanceof(ce, rapira_ce_inet_address)
                || zend::instanceof(ce, rapira_ce_unix_address)
            {
                return true;
            }
        }
        crate::zend_argument_type_error(
            num,
            c"must be of type Rapira\\InetAddress|Rapira\\UnixAddress, %s given".as_ptr(),
            zend_zval_value_name(zv),
        );
        false
    }
}

/// # Safety
/// `obj` is under construction; strings/zvals are ZPP-owned for the call, and all ctors below share this contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_inet_address(
    obj: *mut zend_object,
    ip: *mut zend_string,
    port: i64,
) -> bool {
    guard(false, || unsafe {
        let ce = rapira_ce_inet_address;
        zend::prop_zstr(ce, obj, c"ip", ip);
        zend::prop_long(ce, obj, c"port", port);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`; `path` nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_unix_address(
    obj: *mut zend_object,
    path: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        zend::prop_zstr_or_null(rapira_ce_unix_address, obj, c"path", path);
        !zend::exception_pending()
    })
}

/// # Safety
/// As `rapira_rs_ctor_inet_address`; the five cert/negotiation strings are nullable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_ctor_tls(
    obj: *mut zend_object,
    version: *mut zend_string,
    cipher: *mut zend_string,
    negotiated: *mut zend_string,
    server_name: *mut zend_string,
    serial: *mut zend_string,
    org: *mut zend_string,
    fingerprint: *mut zend_string,
) -> bool {
    guard(false, || unsafe {
        let ce = rapira_ce_tls;
        zend::prop_zstr(ce, obj, c"version", version);
        zend::prop_zstr(ce, obj, c"cipher", cipher);
        zend::prop_zstr_or_null(ce, obj, c"negotiatedProtocol", negotiated);
        zend::prop_zstr_or_null(ce, obj, c"requestedServerName", server_name);
        zend::prop_zstr_or_null(ce, obj, c"certSerial", serial);
        zend::prop_zstr_or_null(ce, obj, c"certOrganization", org);
        zend::prop_zstr_or_null(ce, obj, c"certFingerprint", fingerprint);
        !zend::exception_pending()
    })
}

#[derive(Debug, PartialEq, Eq)]
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

/// Appends the dotted form of `ip`, the same bytes as its `Display`.
pub fn push_ipv4(out: &mut String, ip: Ipv4Addr) {
    for (i, octet) in ip.octets().into_iter().enumerate() {
        if i > 0 {
            out.push('.');
        }
        out.push_str(octet.format_into(&mut NumBuffer::new()));
    }
}

impl AddrOwned {
    pub fn new(a: &Addr) -> Self {
        match a {
            Addr::Inet(sa) => Self::Inet {
                ip: match sa.ip() {
                    IpAddr::V4(v4) => {
                        let mut ip = String::with_capacity(15);
                        push_ipv4(&mut ip, v4);
                        ip
                    }
                    v6 @ IpAddr::V6(_) => v6.to_string(),
                },
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
            crate::add_next_index_stringl(&mut list, zend::ptr_or_empty(v), v.len());
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

/// Slot offsets of `InetAddress` (ip, port) and `UnixAddress` (path); the classes are fixed after MINIT.
static INET_SLOTS: OnceLock<[u32; 2]> = OnceLock::new();
static UNIX_SLOT: OnceLock<u32> = OnceLock::new();

/// # Safety
/// `dst` writable; engine active on this thread.
pub unsafe fn build_address(dst: *mut zval, addr: &AddrOwned) {
    unsafe {
        match addr {
            AddrOwned::Inet { ip, port } => {
                let ce = rapira_ce_inet_address;
                let [ip_slot, port_slot] =
                    *INET_SLOTS.get_or_init(|| [c"ip", c"port"].map(|n| zend::prop_offset(ce, n)));
                let _ = object_init_ex(dst, ce);
                let o = (*dst).value.obj;
                zend::slot_stringl(o, ip_slot, ip.as_bytes());
                zend::slot_long(o, port_slot, i64::from(*port));
            }
            AddrOwned::Unix(path) => {
                let ce = rapira_ce_unix_address;
                let path_slot = *UNIX_SLOT.get_or_init(|| zend::prop_offset(ce, c"path"));
                let _ = object_init_ex(dst, ce);
                zend::slot_str_or_null((*dst).value.obj, path_slot, path.as_deref());
            }
        }
    }
}
