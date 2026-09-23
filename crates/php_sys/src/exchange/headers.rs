use super::*;

/// Fields that may not travel in a trailer section per RFC 9110 §6.5.1; unknown fields pass.
/// https://www.rfc-editor.org/rfc/rfc9110#section-6.5.1
const TRAILER_FORBIDDEN: &[&str] = &[
    "age",
    "authorization",
    "cache-control",
    "connection",
    "content-encoding",
    "content-language",
    "content-length",
    "content-location",
    "content-range",
    "content-type",
    "cookie",
    "date",
    "expect",
    "expires",
    "forwarded",
    "host",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "keep-alive",
    "location",
    "max-forwards",
    "pragma",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "range",
    "retry-after",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "vary",
    "via",
    "warning",
    "www-authenticate",
];

pub(super) fn forbidden_trailer(name: &str) -> bool {
    TRAILER_FORBIDDEN
        .iter()
        .any(|f| name.eq_ignore_ascii_case(f))
}

pub(super) struct SplitHead {
    pub(super) headers: HeaderMap,
    pub(super) declared_cl: Option<u64>,
}

/// Takes content-length out as the declared length; the front drops the hop-by-hop fields.
pub(super) fn split_framing(mut headers: HeaderMap) -> Result<SplitHead, &'static CStr> {
    let mut lines = headers.get_all(http::header::CONTENT_LENGTH).iter();
    let first = lines.next();
    if lines.next().is_some() {
        return Err(c"content-length may not repeat");
    }
    let declared_cl = first.and_then(|v| parse_content_length(v.as_bytes()));
    headers.remove(http::header::CONTENT_LENGTH);
    Ok(SplitHead {
        headers,
        declared_cl,
    })
}

pub(super) fn parse_content_length(v: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(v).ok()?.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A name outside the RFC 9110 `tchar` set or a value byte outside the field-value set is an error. `HeaderName::from_bytes` and `HeaderValue::from_bytes` apply these sets, as on the classic path.
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.5
/// `&raw mut pos`: the pos parameter is *mut on PHP 8.4 and *const on 8.5.
/// # Safety
/// `ht` NULL or a live array; entries stay ZPP-owned.
pub(super) unsafe fn walk_head_table(ht: *mut HashTable) -> Result<HeaderMap, &'static CStr> {
    let mut map = HeaderMap::new();
    if ht.is_null() {
        return Ok(map);
    }
    unsafe {
        let mut pos: HashPosition = 0;
        zend_hash_internal_pointer_reset_ex(ht, &mut pos);
        loop {
            let entry = zend_hash_get_current_data_ex(ht, &raw mut pos);
            if entry.is_null() {
                break;
            }
            let mut str_key: *mut zend_string = std::ptr::null_mut();
            let mut num_key = 0;
            let kt = zend_hash_get_current_key_ex(ht, &mut str_key, &mut num_key, &pos);
            if i64::from(kt) != crate::HASH_KEY_IS_STRING || str_key.is_null() {
                return Err(c"header name is not representable on the wire");
            }
            let Ok(name) = HeaderName::from_bytes(zend::zstr_bytes(str_key)) else {
                return Err(c"header name is not representable on the wire");
            };
            let list = zend::deref(entry);
            if zend::zval_type(list) != IS_ARRAY {
                return Err(c"each header entry must be a list of strings");
            }
            let inner = (*list).value.arr;
            let mut ipos: HashPosition = 0;
            zend_hash_internal_pointer_reset_ex(inner, &mut ipos);
            loop {
                let item = zend_hash_get_current_data_ex(inner, &raw mut ipos);
                if item.is_null() {
                    break;
                }
                let item = zend::deref(item);
                if zend::zval_type(item) != IS_STRING {
                    return Err(c"header value is not representable on the wire");
                }
                let Ok(value) = HeaderValue::from_bytes(zend::zstr_bytes((*item).value.str_))
                else {
                    return Err(c"header value is not representable on the wire");
                };
                map.append(&name, value);
                zend_hash_move_forward_ex(inner, &mut ipos);
            }
            zend_hash_move_forward_ex(ht, &mut pos);
        }
    }
    Ok(map)
}
