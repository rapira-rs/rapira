use super::*;

/// add_assoc_zval_ex moves the list ref in: the hash-update family never addrefs.
unsafe fn emit_headers(dst: *mut zval, g: &Grouped) {
    unsafe {
        rapira_array_init(dst, g.0.len() as u32);
        for (name, values) in &g.0 {
            let mut list: zval = std::mem::zeroed();
            rapira_array_init(&mut list, values.len() as u32);
            for v in values {
                zend::list_push_stringl(&mut list, v);
            }
            add_assoc_zval_ex(dst, name.as_ptr(), name.count_bytes(), &mut list);
        }
    }
}

pub(crate) unsafe fn build_address(dst: *mut zval, addr: &AddrOwned) {
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

pub(crate) unsafe fn build_tls(dst: *mut zval, t: &TlsView) {
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
    ex: *mut rapira_exchange_obj,
    return_value: *mut zval,
) -> bool {
    guard(false, || unsafe { build_request_impl(ex, return_value) })
}

/// A throw during the property writes leaves readonly slots uninitialized: the partial object is released and the memo left unset.
unsafe fn build_request_impl(ex: *mut rapira_exchange_obj, return_value: *mut zval) -> bool {
    unsafe {
        if !zend::is_undef(&(*ex).request) {
            *return_value = (*ex).request;
            zval_add_ref(return_value);
            return true;
        }
        let ce: *mut zend_class_entry = rapira_ce_http_request;
        let st = &*(*ex).job.cast::<ExchangeState>();
        let req = &st.job.ctx.req;

        let mut headers: zval = std::mem::zeroed();
        emit_headers(&mut headers, &st.headers);
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &st.remote);
        let mut server: zval = std::mem::zeroed();
        build_address(&mut server, &st.server);
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

        let mut reqz: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut reqz, ce);
        let o = reqz.value.obj;
        zend::prop_stringl(ce, o, c"method", req.method.as_bytes());
        zend::prop_stringl(ce, o, c"uri", st.uri_abs.as_bytes());
        zend::prop_stringl(ce, o, c"target", &st.target);
        zend::prop_str_or_null(ce, o, c"authority", st.authority.as_deref());
        zend::prop_stringl(ce, o, c"protocol", st.protocol_php.as_bytes());
        zend::prop_zval(ce, o, c"headers", &mut headers);
        zval_ptr_dtor(&mut headers);
        match &st.body {
            BodyState::Raw(v) => zend::prop_stringl(ce, o, c"body", v),
            BodyState::Multipart { .. } => {
                zend::prop_zval(ce, o, c"body", &mut mp);
                zval_ptr_dtor(&mut mp);
            }
        }
        zend::prop_zval(ce, o, c"remote", &mut remote);
        zval_ptr_dtor(&mut remote);
        zend::prop_zval(ce, o, c"server", &mut server);
        zval_ptr_dtor(&mut server);
        if req.tls.is_some() {
            zend::prop_zval(ce, o, c"tls", &mut tls);
        } else {
            zend::prop_null(ce, o, c"tls");
        }
        zval_ptr_dtor(&mut tls);
        zend::prop_double(ce, o, c"receivedAt", req.received_at.unwrap_or(0.0));

        if zend::exception_pending() {
            zval_ptr_dtor(&mut reqz);
            return false;
        }
        (*ex).request = reqz;
        *return_value = reqz;
        zval_add_ref(return_value);
        true
    }
}
