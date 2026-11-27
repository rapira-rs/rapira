use std::{
    ffi::c_char, fmt::NumBuffer, io::Write, net::IpAddr, os::unix::ffi::OsStrExt, sync::OnceLock,
};

use crate::context::{Permanent, permanent};
use crate::{
    add_assoc_zval_ex, object_init_ex, rapira_array_init, rapira_ce_inet_address,
    rapira_ce_unix_address, types::Addr, zend, zend_hash_update, zval,
};

/// The number of the current cycle of this PHP thread. A plugin that closes something at the cycle end reopens it when the number changes.
pub fn cycle() -> u64 {
    crate::worker::current_cycle()
}

/// Writes the text of `ip` into `buf` and returns it, the same bytes as its `Display`.
/// 39 bytes hold every `Display` of an `Ipv6Addr`: eight groups of four hex digits and seven colons, or "::ffff:" and a dotted IPv4 (22 bytes). The std padding path of that `Display` uses the same bound. A dotted IPv4 has at most 15 bytes.
pub fn ip_text(ip: IpAddr, buf: &mut [u8; 39]) -> &str {
    let len = match ip {
        IpAddr::V4(v4) => {
            let mut len = 0;
            for (i, octet) in v4.octets().into_iter().enumerate() {
                if i > 0 {
                    buf[len] = b'.';
                    len += 1;
                }
                let mut digits = NumBuffer::new();
                let digits = octet.format_into(&mut digits).as_bytes();
                buf[len..len + digits.len()].copy_from_slice(digits);
                len += digits.len();
            }
            len
        }
        IpAddr::V6(v6) => {
            let mut rest = &mut buf[..];
            write!(rest, "{v6}").expect("39 bytes hold the Display of an Ipv6Addr");
            let free = rest.len();
            buf.len() - free
        }
    };
    std::str::from_utf8(&buf[..len]).expect("an IP text is ASCII")
}

macro_rules! header_keys {
    ($($field:ident: $text:literal,)*) => {
        /// Pre-interned permanent Zend string keys for common HTTP and multipart header names.
        pub struct HeaderKeys {
            $($field: Permanent,)*
        }

        impl HeaderKeys {
            fn intern() -> Self {
                Self {
                    $($field: permanent($text),)*
                }
            }

            #[inline]
            fn find(&self, name: &str) -> Option<Permanent> {
                match name.as_bytes() {
                    $($text => Some(self.$field),)*
                    _ => None,
                }
            }
        }
    };
}

header_keys! {
    // Multipart part headers in common wire spellings.
    content_disposition_title: b"Content-Disposition",
    content_disposition: b"content-disposition",
    content_type_title: b"Content-Type",
    content_type: b"content-type",
    content_transfer_encoding_title: b"Content-Transfer-Encoding",
    content_transfer_encoding: b"content-transfer-encoding",
    // Common HTTP request headers (lowercase in http::HeaderMap).
    host: b"host",
    user_agent: b"user-agent",
    accept: b"accept",
    accept_language: b"accept-language",
    accept_encoding: b"accept-encoding",
    referer: b"referer",
    cookie: b"cookie",
    connection: b"connection",
    content_length: b"content-length",
    authorization: b"authorization",
    origin: b"origin",
    cache_control: b"cache-control",
    pragma: b"pragma",
    dnt: b"dnt",
    sec_gpc: b"sec-gpc",
    sec_fetch_dest: b"sec-fetch-dest",
    sec_fetch_mode: b"sec-fetch-mode",
    sec_fetch_site: b"sec-fetch-site",
    sec_fetch_user: b"sec-fetch-user",
    upgrade_insecure_requests: b"upgrade-insecure-requests",
    x_forwarded_for: b"x-forwarded-for",
    x_forwarded_proto: b"x-forwarded-proto",
    x_forwarded_host: b"x-forwarded-host",
    x_real_ip: b"x-real-ip",
    x_requested_with: b"x-requested-with",
    if_none_match: b"if-none-match",
    if_modified_since: b"if-modified-since",
}

static HEADER_KEYS: OnceLock<HeaderKeys> = OnceLock::new();

/// Returns the process-wide pre-interned header key table, initializing it once on first use.
#[inline]
pub fn header_keys() -> &'static HeaderKeys {
    HEADER_KEYS.get_or_init(HeaderKeys::intern)
}

/// Inserts a list of header values under `name` using a pre-loaded `HeaderKeys` table.
/// # Safety
/// `dst` is a live array, and the engine is active on this thread.
#[inline]
pub unsafe fn add_list_with_keys<'v>(
    keys: &HeaderKeys,
    dst: *mut zval,
    name: &str,
    values: impl Iterator<Item = &'v [u8]>,
) {
    unsafe {
        let mut list: zval = std::mem::zeroed();
        rapira_array_init(&mut list, values.size_hint().0 as u32);
        for v in values {
            crate::add_next_index_stringl(&mut list, zend::ptr_or_empty(v), v.len());
        }
        if let Some(perm) = keys.find(name) {
            zend_hash_update((*dst).value.arr, perm.as_ptr(), &mut list);
        } else {
            add_assoc_zval_ex(dst, header_key(name), name.len(), &mut list);
        }
    }
}

/// Inserts a list of header values under `name`. Common names use pre-interned permanent keys to avoid hashing and allocation.
/// # Safety
/// `dst` is a live array, and the engine is active on this thread.
pub unsafe fn add_list<'v>(dst: *mut zval, name: &str, values: impl Iterator<Item = &'v [u8]>) {
    unsafe { add_list_with_keys(header_keys(), dst, name, values) }
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
/// `dst` is writable, and the engine is active on this thread.
pub unsafe fn build_address(dst: *mut zval, addr: &Addr) {
    unsafe {
        match addr {
            Addr::Inet(sa) => {
                let mut buf = [0; _];
                let ip = ip_text(sa.ip(), &mut buf);
                let ce = rapira_ce_inet_address;
                let [ip_slot, port_slot] =
                    *INET_SLOTS.get_or_init(|| [c"ip", c"port"].map(|n| zend::prop_offset(ce, n)));
                let _ = object_init_ex(dst, ce);
                let o = (*dst).value.obj;
                zend::slot_stringl(o, ip_slot, ip.as_bytes());
                zend::slot_long(o, port_slot, i64::from(sa.port()));
            }
            Addr::Unix(path) => {
                let path = path.as_deref().map(|p| p.as_os_str().as_bytes());
                let ce = rapira_ce_unix_address;
                let path_slot = *UNIX_SLOT.get_or_init(|| zend::prop_offset(ce, c"path"));
                let _ = object_init_ex(dst, ce);
                zend::slot_str_or_null((*dst).value.obj, path_slot, path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, SocketAddrV6};

    /// The ip text of an inet address is the `Display` of its ip, with no port and no scope id.
    #[test]
    fn ip_text_is_the_display_of_the_ip() {
        struct Case {
            name: &'static str,
            addr: SocketAddr,
            want: &'static str,
        }
        let v6 = |segments: [u16; 8], scope| {
            SocketAddr::V6(SocketAddrV6::new(segments.into(), 0, 0, scope))
        };
        let cases = [
            Case {
                name: "ipv4 client",
                addr: ([10, 0, 0, 1], 5000).into(),
                want: "10.0.0.1",
            },
            Case {
                name: "ipv4 zero address",
                addr: ([0, 0, 0, 0], 0).into(),
                want: "0.0.0.0",
            },
            Case {
                name: "ipv4 octets of three and one digits",
                addr: ([192, 168, 100, 9], 65535).into(),
                want: "192.168.100.9",
            },
            Case {
                name: "ipv4 loopback",
                addr: ([127, 0, 0, 1], 0).into(),
                want: "127.0.0.1",
            },
            Case {
                name: "ipv6 with a compressed zero run",
                addr: v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 0),
                want: "2001:db8::1",
            },
            Case {
                name: "ipv6 scope id stays out of the ip",
                addr: v6([0xfe80, 0, 0, 0, 0, 0, 0, 1], 3),
                want: "fe80::1",
            },
            Case {
                name: "ipv4-mapped ipv6",
                addr: v6([0, 0, 0, 0, 0, 0xffff, 0x0102, 0x0304], 0),
                want: "::ffff:1.2.3.4",
            },
            Case {
                name: "longest ipv6 text fills the buffer",
                addr: v6([0xffff; 8], 0),
                want: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            },
        ];
        for c in cases {
            assert_eq!(ip_text(c.addr.ip(), &mut [0; _]), c.want, "{}", c.name);
        }
    }
}
