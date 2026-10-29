use http::header::{CONTENT_LENGTH, HOST, HeaderMap, HeaderName};
use http::{Method, Version};

use crate::UnsafeFieldNames;

/// A refusal before dispatch: PHP never saw the request.
#[derive(Debug)]
pub struct Rejection {
    pub status: http::StatusCode,
    pub reason: String,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status.as_u16(), self.reason)
    }
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

/// RFC 9110 §7.2: Host = uri-host [ ":" port ]. The host is a reg-name or a bracketed IP literal (RFC 3986 §3.2.2).
/// The check uses character classes and does not parse the IP literal grammar. https://www.rfc-editor.org/rfc/rfc3986#section-3.2.2
fn is_valid_host(v: &[u8]) -> bool {
    // unreserved, sub-delims and the '%' of pct-encoded
    let reg_name = |b: &u8| b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=%".contains(b);
    let host_len = match v.strip_prefix(b"[") {
        // IPv6address and IPvFuture also use ':'.
        Some(rest) => match rest.iter().position(|&b| b == b']') {
            Some(end) if end > 0 && rest[..end].iter().all(|b| reg_name(b) || *b == b':') => {
                end + 2
            }
            _ => return false,
        },
        None => v.iter().take_while(|b| reg_name(b)).count(),
    };
    // RFC 9110 §4.2.1: an empty host is invalid. RFC 3986 §3.2.3: port = *DIGIT.
    host_len > 0
        && match &v[host_len..] {
            [] => true,
            [b':', port @ ..] => port.iter().all(u8::is_ascii_digit),
            _ => false,
        }
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
        Some(v) if is_valid_host(v.as_bytes()) => Ok(Some(v.as_bytes().to_vec())),
        Some(v) if !v.as_bytes().is_empty() => {
            Err("Host field value is not uri-host [ \":\" port ]")
        }
        Some(_) if http11 => Err("HTTP/1.1 request with an empty Host field value"),
        None if http11 => Err("HTTP/1.1 request without a Host field"),
        _ => Ok(None),
    }
}

pub fn apply_field_name_policy(
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
pub fn check_request(
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
            if !is_valid_host(host_port.as_bytes()) {
                return Err(Rejection::new(
                    http::StatusCode::BAD_REQUEST,
                    "request-target authority is not uri-host [ \":\" port ]",
                ));
            }
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

    /// RFC 9112 §3.2: a repeated or invalid Host, or a missing or empty Host on 1.1, is a 400; a bare 1.0 request named no authority.
    /// A valid value is uri-host [ ":" port ] (RFC 9110 §7.2), with RFC 3986 §3.2.2 host and §3.2.3 port = *DIGIT.
    /// RFC 9110 §4.2.1: an empty host is invalid. https://www.rfc-editor.org/rfc/rfc9110#section-7.2
    #[test]
    fn authority_follows_the_host_rules() {
        struct Case {
            name: &'static str,
            fields: &'static [&'static [u8]],
            http11: bool,
            /// Err(()) is a rejection; the reason text is not compared.
            want: Result<Option<&'static [u8]>, ()>,
        }
        let cases = [
            Case {
                name: "reg-name",
                fields: &[b"a.example"],
                http11: true,
                want: Ok(Some(b"a.example")),
            },
            Case {
                name: "missing on 1.1",
                fields: &[],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "empty on 1.1",
                fields: &[b""],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "repeated on 1.0",
                fields: &[b"a", b"b"],
                http11: false,
                want: Err(()),
            },
            Case {
                name: "missing on 1.0",
                fields: &[],
                http11: false,
                want: Ok(None),
            },
            Case {
                name: "empty on 1.0",
                fields: &[b""],
                http11: false,
                want: Ok(None),
            },
            Case {
                name: "reg-name with port",
                fields: &[b"good.example:8080"],
                http11: true,
                want: Ok(Some(b"good.example:8080")),
            },
            Case {
                name: "ipv4 with port on 1.0",
                fields: &[b"127.0.0.1:8080"],
                http11: false,
                want: Ok(Some(b"127.0.0.1:8080")),
            },
            Case {
                name: "ipv6 literal with port",
                fields: &[b"[::1]:8080"],
                http11: true,
                want: Ok(Some(b"[::1]:8080")),
            },
            Case {
                name: "empty port",
                fields: &[b"h:"],
                http11: true,
                want: Ok(Some(b"h:")),
            },
            Case {
                name: "pct-encoded reg-name",
                fields: &[b"a%41"],
                http11: true,
                want: Ok(Some(b"a%41")),
            },
            Case {
                name: "path and query delimiters on 1.0",
                fields: &[b"x/admin?"],
                http11: false,
                want: Err(()),
            },
            Case {
                name: "path and query delimiters on 1.1",
                fields: &[b"x/admin?"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "userinfo",
                fields: &[b"a@b"],
                http11: false,
                want: Err(()),
            },
            Case {
                name: "fragment delimiter",
                fields: &[b"x#"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "backslash",
                fields: &[b"x\\admin"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "space",
                fields: &[b"a b"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "non-digit port",
                fields: &[b"h:8o"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "empty host with port",
                fields: &[b":80"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "unclosed ipv6 literal",
                fields: &[b"[::1"],
                http11: true,
                want: Err(()),
            },
            Case {
                name: "obs-text byte",
                fields: &[b"h\x80"],
                http11: true,
                want: Err(()),
            },
        ];
        for case in &cases {
            let mut headers = HeaderMap::new();
            for v in case.fields {
                headers.append(HOST, http::HeaderValue::from_bytes(v).unwrap());
            }
            let got = authority(&headers, case.http11);
            assert_eq!(
                got.as_ref().map(Option::as_deref).map_err(|_| ()),
                case.want,
                "{}",
                case.name
            );
        }
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
    /// The target authority replaces Host, so it follows the Host rule uri-host [ ":" port ] too (RFC 9110 §7.2).
    #[test]
    fn absolute_form_keeps_the_host_rules() {
        struct Case {
            name: &'static str,
            fields: &'static [(&'static str, &'static str)],
            target: &'static str,
        }
        let cases = [
            Case {
                name: "missing Host",
                fields: &[],
                target: "http://target.example/",
            },
            Case {
                name: "invalid Host",
                fields: &[("host", "x/admin?")],
                target: "http://ok.example/public",
            },
            Case {
                name: "non-digit port in the target",
                fields: &[("host", "ok.example")],
                target: "http://b:xyz/public",
            },
        ];
        for case in &cases {
            let mut p = parts(Method::GET, Version::HTTP_11, case.fields);
            p.uri = case.target.parse().unwrap();
            let status =
                check_request(&mut p, UnsafeFieldNames::Drop, true, 1024).map_err(|r| r.status);
            assert_eq!(
                status.err(),
                Some(http::StatusCode::BAD_REQUEST),
                "{}",
                case.name
            );
        }
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
