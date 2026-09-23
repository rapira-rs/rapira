use http::header::{CONTENT_LENGTH, HOST, HeaderMap, HeaderName};
use http::{Method, Version};

use crate::UnsafeFieldNames;

#[derive(Debug)]
pub(crate) struct Rejection {
    pub status: http::StatusCode,
    pub reason: String,
}

impl Rejection {
    fn new(status: http::StatusCode, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }
}

pub(crate) fn is_safe_field_name(name: &str) -> bool {
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

pub(crate) fn authority(
    headers: &HeaderMap,
    http11: bool,
) -> Result<Option<Vec<u8>>, &'static str> {
    let mut lines = headers.get_all(HOST).iter();
    let first = lines.next();
    if lines.next().is_some() {
        return Err("request carries more than one Host field line");
    }
    match first {
        Some(v) if !v.as_bytes().is_empty() => Ok(Some(v.as_bytes().to_vec())),
        Some(_) if http11 => Err("HTTP/1.1 request with an empty Host field value"),
        None if http11 => Err("HTTP/1.1 request without a Host field"),
        _ => Ok(None),
    }
}

pub(crate) fn apply_field_name_policy(
    headers: &mut HeaderMap,
    policy: UnsafeFieldNames,
    superglobals: bool,
) -> Result<(), Rejection> {
    if policy == UnsafeFieldNames::Drop && !superglobals {
        return Ok(());
    }
    let unsafe_names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| !is_safe_field_name(name.as_str()))
        .cloned()
        .collect();
    if unsafe_names.is_empty() {
        return Ok(());
    }

    match policy {
        UnsafeFieldNames::Drop => {
            for name in &unsafe_names {
                headers.remove(name);
            }
            // One record per request. The client controls the count and the names.
            tracing::warn!(
                target: "http",
                "dropped {} request header(s) aliasing a CGI variable, e.g. {} \
                 (unsafe_field_names = \"drop\"; use \"reject\" to answer 400 instead)",
                unsafe_names.len(),
                unsafe_names[0]
            );
        }
        UnsafeFieldNames::Reject => {
            return Err(Rejection::new(
                http::StatusCode::BAD_REQUEST,
                format!(
                    "{} field name(s) alias a CGI variable, e.g. {}",
                    unsafe_names.len(),
                    unsafe_names[0]
                ),
            ));
        }
    }

    Ok(())
}

/// Admission checks in dispatch order; Ok carries the resolved authority for the PHP request.
pub(crate) fn check_request(
    parts: &mut http::request::Parts,
    unsafe_field_names: UnsafeFieldNames,
    superglobals: bool,
    max_body_size: usize,
) -> Result<Option<Vec<u8>>, Rejection> {
    if parts.method == Method::CONNECT {
        return Err(Rejection::new(
            http::StatusCode::NOT_IMPLEMENTED,
            "CONNECT is not supported",
        ));
    }
    let authority = authority(&parts.headers, parts.version == Version::HTTP_11)
        .map_err(|reason| Rejection::new(http::StatusCode::BAD_REQUEST, reason))?;

    let authority = match parts.uri.authority() {
        Some(a) => {
            let a = a.as_str();
            let host_port = a.rsplit_once('@').map_or(a, |(_, hp)| hp);
            // Rewrite Host to the effective authority, so HTTP_HOST cannot disagree.
            // https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
            let v = http::HeaderValue::from_str(host_port)
                .expect("uri authority bytes are valid header bytes");
            parts.headers.insert(HOST, v);
            Some(host_port.as_bytes().to_vec())
        }
        None => authority,
    };

    apply_field_name_policy(&mut parts.headers, unsafe_field_names, superglobals)?;

    let declared = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    if declared.is_some_and(|len| len > max_body_size as u64) {
        return Err(Rejection::new(
            http::StatusCode::PAYLOAD_TOO_LARGE,
            "declared content-length exceeds max_body_size",
        ));
    }
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.append(
                HeaderName::try_from(*k).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    /// RFC 9112 §3.2: repeated/missing/empty Host on 1.1 is a 400; a bare 1.0 request named no authority.
    #[test]
    fn authority_follows_the_host_rules() {
        assert_eq!(
            authority(&map(&[("host", "a.example")]), true)
                .unwrap()
                .as_deref(),
            Some(&b"a.example"[..])
        );
        assert!(authority(&map(&[]), true).is_err());
        assert!(authority(&map(&[("host", "")]), true).is_err());
        assert!(authority(&map(&[("host", "a"), ("host", "b")]), false).is_err());
        assert_eq!(authority(&map(&[]), false).unwrap(), None);
        assert_eq!(authority(&map(&[("host", "")]), false).unwrap(), None);
    }

    /// Field names surviving `policy`, in map order.
    fn surviving(
        policy: UnsafeFieldNames,
        pairs: &[(&str, &str)],
    ) -> Result<Vec<String>, Rejection> {
        let mut headers = map(pairs);
        apply_field_name_policy(&mut headers, policy, true)?;
        Ok(headers.keys().map(|n| n.as_str().to_owned()).collect())
    }

    /// Drop protects the $_SERVER mapping; a dispatcher pool has none, so names pass as received.
    #[test]
    fn drop_is_inert_without_superglobals() {
        let mut headers = map(&[("x_forwarded_for", "1.2.3.4")]);
        apply_field_name_policy(&mut headers, UnsafeFieldNames::Drop, false).unwrap();
        assert!(headers.get("x_forwarded_for").is_some());
    }

    #[test]
    fn only_alphanumerics_and_dash_are_safe_field_names() {
        assert!(is_safe_field_name("x-forwarded-for"));
        assert!(is_safe_field_name("Sec-Ch-Ua-Mobile"));
        assert!(!is_safe_field_name("x_forwarded_for"));
        assert!(!is_safe_field_name("x.forwarded.for"));
        assert!(!is_safe_field_name("x~foo"));
        assert!(!is_safe_field_name("x$foo"));
    }

    #[test]
    fn drop_removes_only_the_unsafe_names() {
        let names = surviving(
            UnsafeFieldNames::Drop,
            &[
                ("x-forwarded-for", "203.0.113.7"),
                ("x_forwarded_for", "1.2.3.4"),
                ("x.forwarded.for", "5.6.7.8"),
            ],
        )
        .unwrap();
        assert_eq!(names, ["x-forwarded-for"]);
    }

    #[test]
    fn reject_answers_400_only_when_a_name_is_unsafe() {
        let err = surviving(UnsafeFieldNames::Reject, &[("x_forwarded_for", "1.2.3.4")])
            .err()
            .unwrap();
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);
        let names = surviving(UnsafeFieldNames::Reject, &[("x-forwarded-for", "1.2.3.4")]).unwrap();
        assert_eq!(names, ["x-forwarded-for"]);
    }

    fn parts(method: Method, version: Version, pairs: &[(&str, &str)]) -> http::request::Parts {
        let mut req = http::Request::new(());
        *req.method_mut() = method;
        *req.version_mut() = version;
        *req.headers_mut() = map(pairs);
        req.into_parts().0
    }

    #[test]
    fn connect_is_refused_with_501() {
        let mut p = parts(Method::CONNECT, Version::HTTP_11, &[("host", "e2e")]);
        let err = check_request(&mut p, UnsafeFieldNames::Drop, true, 1024)
            .err()
            .unwrap();
        assert_eq!(err.status, http::StatusCode::NOT_IMPLEMENTED);
    }

    /// RFC 9112 §3.2.2: the absolute-form target authority replaces Host. Userinfo is stripped.
    /// The Host field line is rewritten too, so HTTP_HOST agrees with the authority.
    /// https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
    #[test]
    fn absolute_form_authority_overrides_host() {
        let mut p = parts(
            Method::GET,
            Version::HTTP_11,
            &[("host", "spoofed.example")],
        );
        p.uri = "http://target.example/admin?x=1".parse().unwrap();
        let authority = check_request(&mut p, UnsafeFieldNames::Drop, true, 1024).unwrap();
        assert_eq!(authority.as_deref(), Some(&b"target.example"[..]));
        assert_eq!(p.headers.get("host").unwrap(), "target.example");

        let mut p = parts(
            Method::GET,
            Version::HTTP_11,
            &[("host", "spoofed.example")],
        );
        p.uri = "http://u:pw@target.example:8080/x".parse().unwrap();
        let authority = check_request(&mut p, UnsafeFieldNames::Drop, true, 1024).unwrap();
        assert_eq!(authority.as_deref(), Some(&b"target.example:8080"[..]));
        assert_eq!(p.headers.get("host").unwrap(), "target.example:8080");
    }

    /// RFC 9112 §3.2: the Host 400 rules stay in force for an absolute-form target.
    #[test]
    fn absolute_form_keeps_the_host_rules() {
        let mut p = parts(Method::GET, Version::HTTP_11, &[]);
        p.uri = "http://target.example/".parse().unwrap();
        let err = check_request(&mut p, UnsafeFieldNames::Drop, true, 1024)
            .err()
            .unwrap();
        assert_eq!(err.status, http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn declared_length_over_the_cap_is_413() {
        let mut p = parts(
            Method::POST,
            Version::HTTP_11,
            &[("host", "e2e"), ("content-length", "2048")],
        );
        let err = check_request(&mut p, UnsafeFieldNames::Drop, true, 1024)
            .err()
            .unwrap();
        assert_eq!(err.status, http::StatusCode::PAYLOAD_TOO_LARGE);

        let mut p = parts(
            Method::POST,
            Version::HTTP_11,
            &[("host", "e2e"), ("content-length", "1024")],
        );
        assert!(check_request(&mut p, UnsafeFieldNames::Drop, true, 1024).is_ok());
    }
}
