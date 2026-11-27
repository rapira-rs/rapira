//! getRequest() builds the Rapira\Http\Request object once per exchange.

use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

use rapira_http_middleware::RequestHeaders;
use rapira_sapi::exchange::HeaderKeys;

use super::*;

unsafe fn emit_headers<'a>(
    keys: &HeaderKeys,
    dst: *mut zval,
    headers: impl ExactSizeIterator<Item = HeaderRef<'a>>,
) {
    unsafe {
        rapira_array_init(dst, headers.len() as u32);
        for h in headers {
            add_list_with_keys(keys, dst, h.name(), h.values());
        }
    }
}

/// Emits request headers grouped by lowercase field name in first-occurrence order.
unsafe fn emit_request_headers(keys: &HeaderKeys, dst: *mut zval, headers: &RequestHeaders) {
    unsafe {
        rapira_array_init(dst, headers.keys_len() as u32);
        // http 1.5.0 groups values by identical HeaderName reference in first-insertion order.
        // https://docs.rs/http/1.5.0/src/http/header/map.rs.html#2323-2356
        let mut fields = headers.iter().peekable();
        while let Some((name, first)) = fields.next() {
            let rest = std::iter::from_fn(|| {
                fields
                    .next_if(|&(n, _)| std::ptr::eq(n, name))
                    .map(|(_, v)| v)
            });
            let values = std::iter::once(first)
                .chain(rest)
                .map(HeaderValue::as_bytes);
            add_list_with_keys(keys, dst, name.as_str(), values);
        }
    }
}

static UPLOADED_FILE_SLOTS: OnceLock<[u32; 6]> = OnceLock::new();
static FORM_FIELD_SLOTS: OnceLock<[u32; 3]> = OnceLock::new();
static MULTIPART_SLOTS: OnceLock<[u32; 2]> = OnceLock::new();

unsafe fn build_file(keys: &HeaderKeys, dst: *mut zval, body: &MultipartBody, f: &UploadedFile) {
    unsafe {
        let ce = rapira_ce_http_uploaded_file;
        let [
            name_slot,
            client_filename_slot,
            client_media_type_slot,
            headers_slot,
            tmp_path_slot,
            size_slot,
        ] = *UPLOADED_FILE_SLOTS.get_or_init(|| {
            [
                c"name",
                c"clientFilename",
                c"clientMediaType",
                c"headers",
                c"tmpPath",
                c"size",
            ]
            .map(|n| zend::prop_offset(ce, n))
        });
        let mut headers: zval = std::mem::zeroed();
        emit_headers(keys, &mut headers, body.file_headers(f));
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::slot_stringl(o, name_slot, body.file_name(f));
        zend::slot_stringl(o, client_filename_slot, body.file_client_filename(f));
        zend::slot_str_or_null(o, client_media_type_slot, body.file_client_media_type(f));
        zend::slot_init(o, headers_slot, &headers);
        zend::slot_stringl(o, tmp_path_slot, f.file.path.as_os_str().as_bytes());
        zend::slot_long(o, size_slot, f.size as i64);
    }
}

/// A property write on a fresh object of a plugin class with a matching type throws nothing; a Zend OOM is a bailout.
unsafe fn build_multipart(keys: &HeaderKeys, dst: *mut zval, body: &MultipartBody) {
    unsafe {
        let mut fields: zval = std::mem::zeroed();
        rapira_array_init(&mut fields, body.fields.len() as u32);
        let mut files: zval = std::mem::zeroed();
        rapira_array_init(&mut files, body.files.len() as u32);

        if !body.fields.is_empty() {
            let ce = rapira_ce_http_form_field;
            let [name_slot, value_slot, headers_slot] = *FORM_FIELD_SLOTS
                .get_or_init(|| [c"name", c"value", c"headers"].map(|n| zend::prop_offset(ce, n)));
            for f in &body.fields {
                let mut headers: zval = std::mem::zeroed();
                emit_headers(keys, &mut headers, body.field_headers(f));
                let mut part: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut part, ce);
                let o = part.value.obj;
                zend::slot_stringl(o, name_slot, body.field_name(f));
                zend::slot_stringl(o, value_slot, body.field_value(f));
                zend::slot_init(o, headers_slot, &headers);
                let _ = add_next_index_object(&mut fields, o);
            }
        }

        for f in &body.files {
            let mut part: zval = std::mem::zeroed();
            build_file(keys, &mut part, body, f);
            let _ = add_next_index_object(&mut files, part.value.obj);
        }

        let ce = rapira_ce_http_multipart;
        let [fields_slot, files_slot] = *MULTIPART_SLOTS
            .get_or_init(|| [c"fields", c"files"].map(|n| zend::prop_offset(ce, n)));
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::slot_init(o, fields_slot, &fields);
        zend::slot_init(o, files_slot, &files);
    }
}

/// Formats `$uri` into a stack buffer when it fits, avoiding a heap `String` per request.
fn uri_abs_inline<'a>(
    req: &Request,
    authority: Option<&[u8]>,
    buf: &'a mut [u8; 256],
) -> Option<&'a [u8]> {
    let mut len = 0usize;
    let ok = write_uri_abs(req, authority, |src| {
        let end = len + src.len();
        if end > buf.len() {
            return false;
        }
        buf[len..end].copy_from_slice(src);
        len = end;
        true
    });
    ok.then_some(&buf[..len])
}

/// Builds the Request into `memo`, the request memo of the exchange object. The C shell keeps the memo check. False means a throw is pending; a caught panic returns false without one, and the C shell then throws.
/// # Safety
/// `held` is the held word of a live exchange that receive() attached; `memo` is writable and IS_UNDEF.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_build_request(
    held: *const HeldPtr,
    memo: *mut zval,
) -> bool {
    guard(false, || unsafe { build_request_impl(held, memo) })
}

/// Slot offsets of the `Request` properties in declaration order; the class is fixed after MINIT.
static REQUEST_SLOTS: OnceLock<[u32; 11]> = OnceLock::new();

/// No write here throws. A Zend OOM bails out through this frame, so the state keeps the owned `$uri`.
unsafe fn build_request_impl(held: *const HeldPtr, memo: *mut zval) -> bool {
    unsafe {
        let ce: *mut zend_class_entry = rapira_ce_http_request;
        let st = &mut *(*held).state::<ExchangeState>();
        let req = &st.req;
        let authority = req.authority_bytes();
        let mut uri_buf = [0u8; 256];
        let uri_bytes: &[u8] = match uri_abs_inline(req, authority, &mut uri_buf) {
            Some(inline) => inline,
            None => st.uri_abs.get_or_insert_with(|| uri_abs(req)).as_bytes(),
        };

        let keys = header_keys();
        let mut headers: zval = std::mem::zeroed();
        emit_request_headers(keys, &mut headers, &req.headers);
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &req.remote);
        let mut server: zval = std::mem::zeroed();
        build_address(&mut server, &req.server);
        let mut mp: zval = std::mem::zeroed();
        if let Some(body) = &st.multipart {
            build_multipart(keys, &mut mp, body);
        }

        let [
            method_slot,
            uri_slot,
            target_slot,
            authority_slot,
            protocol_slot,
            headers_slot,
            body_slot,
            remote_slot,
            server_slot,
            tls_slot,
            received_at_slot,
        ] = *REQUEST_SLOTS.get_or_init(|| {
            [
                c"method",
                c"uri",
                c"target",
                c"authority",
                c"protocol",
                c"headers",
                c"body",
                c"remote",
                c"server",
                c"tls",
                c"receivedAt",
            ]
            .map(|n| zend::prop_offset(ce, n))
        });
        let mut reqz: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut reqz, ce);
        let o = reqz.value.obj;
        zend::slot_stringl(o, method_slot, req.method.as_bytes());
        zend::slot_stringl(o, uri_slot, uri_bytes);
        let target = req.target.as_deref().unwrap_or(req.uri.as_str().as_bytes());
        zend::slot_stringl(o, target_slot, target);
        zend::slot_str_or_null(o, authority_slot, authority);
        zend::slot_stringl(o, protocol_slot, protocol_php(req.protocol).as_bytes());
        zend::slot_init(o, headers_slot, &headers);
        match &st.multipart {
            None => zend::slot_string_from_chunks(o, body_slot, &req.body),
            Some(_) => zend::slot_init(o, body_slot, &mp),
        }
        zend::slot_init(o, remote_slot, &remote);
        zend::slot_init(o, server_slot, &server);
        zend::slot_null(o, tls_slot);
        zend::slot_double(o, received_at_slot, req.received_at);

        *memo = reqz;
        st.req.body = Default::default();
        true
    }
}
