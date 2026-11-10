//! getRequest(): builds the Rapira\Http\Request object once per exchange.

use std::os::unix::ffi::OsStrExt;
use std::sync::OnceLock;

use super::*;

unsafe fn emit_headers(dst: *mut zval, headers: &[HeaderValues]) {
    unsafe {
        rapira_array_init(dst, headers.len() as u32);
        for HeaderValues { name, values } in headers {
            add_list(
                dst,
                header_key(name),
                name.len(),
                values.iter().map(Vec::as_slice),
            );
        }
    }
}

/// One entry per name, with its values in field line order.
unsafe fn emit_header_map(dst: *mut zval, headers: &HeaderMap) {
    unsafe {
        rapira_array_init(dst, headers.keys_len() as u32);
        // shortcut: http 1.5.0 groups values by key reference, recheck on dependency upgrades.
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
            let key = name.as_str();
            add_list(dst, header_key(key), key.len(), values);
        }
    }
}

unsafe fn build_file(dst: *mut zval, f: &UploadedFile) {
    unsafe {
        let ce = rapira_ce_http_uploaded_file;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_stringl(ce, o, c"name", &f.name);
        zend::prop_stringl(ce, o, c"clientFilename", &f.client_filename);
        zend::prop_str_or_null(ce, o, c"clientMediaType", f.client_media_type.as_deref());
        let mut headers: zval = std::mem::zeroed();
        emit_headers(&mut headers, &f.headers);
        zend::prop_zval(ce, o, c"headers", &mut headers);
        zval_ptr_dtor(&mut headers);
        zend::prop_stringl(ce, o, c"tmpPath", f.file.path.as_os_str().as_bytes());
        zend::prop_long(ce, o, c"size", f.size as i64);
    }
}

/// A property write on a fresh object of a plugin class with a matching type throws nothing; a Zend OOM is a bailout.
unsafe fn build_multipart(dst: *mut zval, body: &MultipartBody) {
    unsafe {
        let mut fields: zval = std::mem::zeroed();
        rapira_array_init(&mut fields, body.fields.len() as u32);
        let mut files: zval = std::mem::zeroed();
        rapira_array_init(&mut files, body.files.len() as u32);

        for f in &body.fields {
            let ce = rapira_ce_http_form_field;
            let mut part: zval = std::mem::zeroed();
            let _ = object_init_ex(&mut part, ce);
            let o = part.value.obj;
            zend::prop_stringl(ce, o, c"name", &f.name);
            zend::prop_stringl(ce, o, c"value", &f.value);
            let mut headers: zval = std::mem::zeroed();
            emit_headers(&mut headers, &f.headers);
            zend::prop_zval(ce, o, c"headers", &mut headers);
            zval_ptr_dtor(&mut headers);
            let _ = add_next_index_object(&mut fields, o);
        }

        for f in &body.files {
            let mut part: zval = std::mem::zeroed();
            build_file(&mut part, f);
            let _ = add_next_index_object(&mut files, part.value.obj);
        }

        let ce = rapira_ce_http_multipart;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_zval(ce, o, c"fields", &mut fields);
        zend::prop_zval(ce, o, c"files", &mut files);
        zval_ptr_dtor(&mut fields);
        zval_ptr_dtor(&mut files);
    }
}

/// Builds the Request into `memo`, the request memo of the exchange object. The C shell keeps the memo check. False means a throw is pending; a caught panic returns false without one and the C shell throws instead.
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
        let uri = st.uri_abs.get_or_insert_with(|| uri_abs(req));

        let mut headers: zval = std::mem::zeroed();
        emit_header_map(&mut headers, &req.headers);
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &req.remote);
        let mut server: zval = std::mem::zeroed();
        build_address(&mut server, &req.server);
        let mut mp: zval = std::mem::zeroed();
        if let Some(body) = &st.multipart {
            build_multipart(&mut mp, body);
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
        zend::slot_stringl(o, uri_slot, uri.as_bytes());
        let target = req.target.as_deref().unwrap_or(req.uri.as_bytes());
        zend::slot_stringl(o, target_slot, target);
        zend::slot_str_or_null(o, authority_slot, req.authority.as_deref());
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
        true
    }
}
