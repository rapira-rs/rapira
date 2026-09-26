use std::sync::OnceLock;

use super::*;

unsafe fn emit_headers(dst: *mut zval, g: &Grouped) {
    unsafe {
        rapira_array_init(dst, g.0.len() as u32);
        for (name, values) in &g.0 {
            add_list(
                dst,
                name.as_ptr(),
                name.count_bytes(),
                values.iter().map(Vec::as_slice),
            );
        }
    }
}

/// One entry per name, with its values in field line order.
unsafe fn emit_header_map(dst: *mut zval, headers: &HeaderMap) {
    unsafe {
        rapira_array_init(dst, headers.keys_len() as u32);
        for name in headers.keys() {
            let key = name.as_str();
            let values = headers.get_all(name).iter().map(HeaderValue::as_bytes);
            add_list(dst, header_key(key), key.len(), values);
        }
    }
}

unsafe fn build_tls(dst: *mut zval, t: &Tls) {
    unsafe {
        let ce = rapira_ce_tls;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_stringl(ce, o, c"version", t.version.as_bytes());
        zend::prop_stringl(ce, o, c"cipher", t.cipher.as_bytes());
        zend::prop_str_or_null(
            ce,
            o,
            c"negotiatedProtocol",
            t.alpn.as_deref().map(str::as_bytes),
        );
        zend::prop_str_or_null(
            ce,
            o,
            c"requestedServerName",
            t.server_name.as_deref().map(str::as_bytes),
        );
        match t.cert.as_ref() {
            Some(cert) => {
                zend::prop_stringl(ce, o, c"certSerial", cert.serial.as_bytes());
                zend::prop_str_or_null(
                    ce,
                    o,
                    c"certOrganization",
                    cert.organization.as_deref().map(str::as_bytes),
                );
                zend::prop_stringl(ce, o, c"certFingerprint", cert.fingerprint.as_bytes());
            }
            None => {
                zend::prop_null(ce, o, c"certSerial");
                zend::prop_null(ce, o, c"certOrganization");
                zend::prop_null(ce, o, c"certFingerprint");
            }
        }
    }
}

unsafe fn build_file(dst: *mut zval, p: &FilePart) {
    unsafe {
        let ce = rapira_ce_http_uploaded_file;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_stringl(ce, o, c"name", &p.upload.name);
        zend::prop_stringl(ce, o, c"clientFilename", &p.upload.client_filename);
        zend::prop_str_or_null(
            ce,
            o,
            c"clientMediaType",
            p.upload.client_media_type.as_deref(),
        );
        let mut headers: zval = std::mem::zeroed();
        emit_headers(&mut headers, &p.headers);
        zend::prop_zval(ce, o, c"headers", &mut headers);
        zval_ptr_dtor(&mut headers);
        zend::prop_stringl(ce, o, c"tmpPath", &p.path);
        zend::prop_long(ce, o, c"size", p.upload.size as i64);
    }
}

/// Returns false on a pending exception: property writes must not run with a throw in flight, so the partial graph is released.
unsafe fn build_multipart(
    dst: *mut zval,
    field_parts: &[FieldPart],
    file_parts: &[FilePart],
) -> bool {
    unsafe {
        let mut fields: zval = std::mem::zeroed();
        rapira_array_init(&mut fields, field_parts.len() as u32);
        let mut files: zval = std::mem::zeroed();
        rapira_array_init(&mut files, file_parts.len() as u32);

        for p in field_parts {
            let ce = rapira_ce_http_form_field;
            let mut part: zval = std::mem::zeroed();
            let _ = object_init_ex(&mut part, ce);
            let o = part.value.obj;
            zend::prop_stringl(ce, o, c"name", &p.field.name);
            zend::prop_stringl(ce, o, c"value", &p.field.value);
            let mut headers: zval = std::mem::zeroed();
            emit_headers(&mut headers, &p.headers);
            zend::prop_zval(ce, o, c"headers", &mut headers);
            zval_ptr_dtor(&mut headers);
            if zend::exception_pending() {
                zval_ptr_dtor(&mut part);
                zval_ptr_dtor(&mut fields);
                zval_ptr_dtor(&mut files);
                return false;
            }
            let _ = add_next_index_object(&mut fields, o);
        }

        for p in file_parts {
            let mut part: zval = std::mem::zeroed();
            build_file(&mut part, p);
            if zend::exception_pending() {
                zval_ptr_dtor(&mut part);
                zval_ptr_dtor(&mut fields);
                zval_ptr_dtor(&mut files);
                return false;
            }
            let _ = add_next_index_object(&mut files, part.value.obj);
        }

        let ce = rapira_ce_http_multipart;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_zval(ce, o, c"fields", &mut fields);
        zend::prop_zval(ce, o, c"files", &mut files);
        zval_ptr_dtor(&mut fields);
        zval_ptr_dtor(&mut files);
        if zend::exception_pending() {
            zval_ptr_dtor(dst);
            return false;
        }
        true
    }
}

/// False means a throw is pending; a caught panic returns false without one and the C shell throws instead.
/// # Safety
/// `ex` is a live exchange with a non-null job; `return_value` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_build_request(
    ex: *mut ExchangeObj,
    return_value: *mut zval,
) -> bool {
    guard(false, || unsafe { build_request_impl(ex, return_value) })
}

/// Slot offsets of the `Request` properties in declaration order; the class is fixed after MINIT.
static REQUEST_SLOTS: OnceLock<[u32; 11]> = OnceLock::new();

/// A throw while the property values are built releases them and leaves the memo unset. The slot writes do not throw.
unsafe fn build_request_impl(ex: *mut ExchangeObj, return_value: *mut zval) -> bool {
    unsafe {
        if !zend::is_undef(&(*ex).request) {
            *return_value = (*ex).request;
            zval_add_ref(return_value);
            return true;
        }
        let ce: *mut zend_class_entry = rapira_ce_http_request;
        let st = &mut *(*ex).job.cast::<ExchangeState>();
        let req = &st.ctx.req;
        let view = st.view.get_or_insert_with(|| RequestView::new(req));

        let mut headers: zval = std::mem::zeroed();
        emit_header_map(&mut headers, &req.headers);
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &view.remote);
        let mut server: zval = std::mem::zeroed();
        build_address(&mut server, &view.server);
        let mut tls: zval = std::mem::zeroed();
        if let Some(t) = req.tls.as_ref() {
            build_tls(&mut tls, t);
        }
        let mut mp: zval = std::mem::zeroed();
        if let BodyState::Multipart { fields, files } = &st.body
            && !build_multipart(&mut mp, fields, files)
        {
            zval_ptr_dtor(&mut headers);
            zval_ptr_dtor(&mut remote);
            zval_ptr_dtor(&mut server);
            zval_ptr_dtor(&mut tls);
            return false;
        }
        if zend::exception_pending() {
            zval_ptr_dtor(&mut headers);
            zval_ptr_dtor(&mut remote);
            zval_ptr_dtor(&mut server);
            zval_ptr_dtor(&mut tls);
            zval_ptr_dtor(&mut mp);
            return false;
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
        zend::slot_stringl(o, uri_slot, view.uri_abs.as_bytes());
        let target = req.target.as_deref().unwrap_or(req.uri.as_bytes());
        zend::slot_stringl(o, target_slot, target);
        zend::slot_str_or_null(o, authority_slot, req.authority.as_deref());
        zend::slot_stringl(o, protocol_slot, protocol_php(&req.protocol).as_bytes());
        zend::slot_init(o, headers_slot, &headers);
        match &st.body {
            BodyState::Raw(v) => zend::slot_stringl(o, body_slot, v),
            BodyState::Multipart { .. } => zend::slot_init(o, body_slot, &mp),
        }
        zend::slot_init(o, remote_slot, &remote);
        zend::slot_init(o, server_slot, &server);
        if req.tls.is_some() {
            zend::slot_init(o, tls_slot, &tls);
        } else {
            zend::slot_null(o, tls_slot);
        }
        zend::slot_double(o, received_at_slot, req.received_at.unwrap_or(0.0));

        (*ex).request = reqz;
        *return_value = reqz;
        zval_add_ref(return_value);
        true
    }
}
