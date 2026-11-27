//! The head table walk, the content-length split and the trailer denylist.

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
    pub(super) headers: ResponseHeaders,
    pub(super) declared_cl: Option<u64>,
}

/// Takes content-length out as the declared length; the HTTP writer drops the hop-by-hop fields.
pub(super) fn split_framing(mut headers: ResponseHeaders) -> Result<SplitHead, &'static CStr> {
    let declared_cl = {
        let mut lines = headers.get_all(&http::header::CONTENT_LENGTH).iter();
        let first = lines.next();
        if lines.next().is_some() {
            return Err(c"content-length may not repeat");
        }
        first.and_then(|v| crate::check::parse_content_length(v.as_bytes().trim_ascii()))
    };
    headers.remove(&http::header::CONTENT_LENGTH);
    Ok(SplitHead {
        headers,
        declared_cl,
    })
}

/// A name outside the RFC 9110 `tchar` set or a value byte outside the field-value set is an error. `HeaderName::from_bytes` and `HeaderValue::from_bytes` apply these sets, as on the classic path.
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.5
/// # Safety
/// `ht` is NULL or a live array. ZPP owns the entries.
pub(super) unsafe fn walk_head_table(ht: *mut HashTable) -> Result<ResponseHeaders, &'static CStr> {
    let mut headers = ResponseHeaders::new();
    if ht.is_null() {
        return Ok(headers);
    }
    unsafe {
        for zend::Entry { key, val: list } in zend::entries(ht) {
            let zend::Key::Str(str_key) = key else {
                return Err(c"header name is not representable on the wire");
            };
            let Ok(name) = HeaderName::from_bytes(zend::zstr_bytes(str_key)) else {
                return Err(c"header name is not representable on the wire");
            };
            if zend::zval_type(list) != IS_ARRAY {
                return Err(c"each header entry must be a list of strings");
            }
            for zend::Entry { val: item, .. } in zend::entries((*list).value.arr) {
                if zend::zval_type(item) != IS_STRING {
                    return Err(c"header value is not representable on the wire");
                }
                let Ok(value) = HeaderValue::from_bytes(zend::zstr_bytes((*item).value.str_))
                else {
                    return Err(c"header value is not representable on the wire");
                };
                headers.append(name.clone(), value);
            }
        }
    }
    Ok(headers)
}
