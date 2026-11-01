//! The admission checks before the middleware chain: Host, the field names of the CGI view and the declared body length.

use http::Version;
use http::header::{CONTENT_LENGTH, HOST, HeaderMap, HeaderName};

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

/// The CGI view maps `-` and `.` to `_`, so `X_Forwarded_For` and `X.Forwarded.For` would land on the `HTTP_X_FORWARDED_FOR` of `X-Forwarded-For`. nginx and Apache drop such names too.
pub(crate) fn is_safe_field_name(name: &str) -> bool {
    name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The authority of the request: the first Host value; an empty one counts as missing. HTTP/1.1 requires one. https://www.rfc-editor.org/rfc/rfc9112#section-3.2
fn authority(headers: &HeaderMap, http11: bool) -> Result<Option<Vec<u8>>, &'static str> {
    match headers.get(HOST).filter(|v| !v.is_empty()) {
        Some(v) => Ok(Some(v.as_bytes().to_vec())),
        None if http11 => Err("HTTP/1.1 request without a Host field"),
        None => Ok(None),
    }
}

/// Removes the field names that alias a CGI variable. Only the classic and worker modes build the CGI view.
pub fn drop_unsafe_field_names(headers: &mut HeaderMap) {
    let mut unsafe_names: Vec<HeaderName> = Vec::new();
    for name in headers.keys() {
        if !is_safe_field_name(name.as_str()) {
            unsafe_names.push(name.clone());
        }
    }
    for name in &unsafe_names {
        headers.remove(name);
    }
}

/// Admission checks in dispatch order; Ok carries the authority for the PHP request.
pub fn check_request(
    parts: &mut http::request::Parts,
    superglobals: bool,
    max_body_size: usize,
) -> Result<Option<Vec<u8>>, Rejection> {
    let authority = authority(&parts.headers, parts.version == Version::HTTP_11)
        .map_err(|reason| Rejection::new(http::StatusCode::BAD_REQUEST, reason))?;
    if superglobals {
        drop_unsafe_field_names(&mut parts.headers);
    }
    let declared = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
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
    use http::Method;

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

    /// RFC 9112 §3.2 requires a Host on HTTP/1.1; the plugin reads the first non-empty value.
    #[test]
    fn authority_follows_the_host_rules() {
        struct Case {
            name: &'static str,
            headers: &'static [(&'static str, &'static str)],
            http11: bool,
            /// None is a 400.
            want: Option<Option<&'static [u8]>>,
        }
        let cases = [
            Case {
                name: "HTTP/1.1 with a Host",
                headers: &[("host", "a.example")],
                http11: true,
                want: Some(Some(b"a.example")),
            },
            Case {
                name: "HTTP/1.1 without a Host",
                headers: &[],
                http11: true,
                want: None,
            },
            Case {
                name: "HTTP/1.1 with an empty Host",
                headers: &[("host", "")],
                http11: true,
                want: None,
            },
            Case {
                name: "HTTP/1.0 without a Host",
                headers: &[],
                http11: false,
                want: Some(None),
            },
            Case {
                name: "HTTP/1.0 with an empty Host",
                headers: &[("host", "")],
                http11: false,
                want: Some(None),
            },
            Case {
                name: "the first of two Host lines wins",
                headers: &[("host", "a"), ("host", "b")],
                http11: true,
                want: Some(Some(b"a")),
            },
        ];
        for case in cases {
            let got = authority(&map(case.headers), case.http11).ok();
            let want = case.want.map(|a| a.map(<[u8]>::to_vec));
            assert_eq!(got, want, "{}", case.name);
        }
    }

    #[test]
    fn only_alphanumerics_and_dash_are_safe_field_names() {
        struct Case {
            name: &'static str,
            field: &'static str,
            safe: bool,
        }
        let cases = [
            Case {
                name: "dashes",
                field: "x-forwarded-for",
                safe: true,
            },
            Case {
                name: "mixed case",
                field: "Sec-Ch-Ua-Mobile",
                safe: true,
            },
            Case {
                name: "underscore",
                field: "x_forwarded_for",
                safe: false,
            },
            Case {
                name: "dot",
                field: "x.forwarded.for",
                safe: false,
            },
            Case {
                name: "tilde",
                field: "x~foo",
                safe: false,
            },
            Case {
                name: "dollar",
                field: "x$foo",
                safe: false,
            },
        ];
        for case in cases {
            assert_eq!(is_safe_field_name(case.field), case.safe, "{}", case.name);
        }
    }

    #[test]
    fn drop_removes_only_the_unsafe_names() {
        let mut headers = map(&[
            ("x-forwarded-for", "203.0.113.7"),
            ("x_forwarded_for", "1.2.3.4"),
            ("x.forwarded.for", "5.6.7.8"),
        ]);
        drop_unsafe_field_names(&mut headers);
        let names: Vec<&str> = headers.keys().map(HeaderName::as_str).collect();
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
    fn check_request_admits_or_refuses() {
        struct Case {
            name: &'static str,
            version: Version,
            headers: &'static [(&'static str, &'static str)],
            superglobals: bool,
            /// None is admitted; Some is the refusal status.
            want_status: Option<u16>,
            /// The field names after the checks, for an admitted request.
            want_names: &'static [&'static str],
        }
        let cases = [
            Case {
                name: "declared length over the cap is 413",
                version: Version::HTTP_11,
                headers: &[("host", "e2e"), ("content-length", "2048")],
                superglobals: true,
                want_status: Some(413),
                want_names: &[],
            },
            Case {
                name: "declared length at the cap passes",
                version: Version::HTTP_11,
                headers: &[("host", "e2e"), ("content-length", "1024")],
                superglobals: true,
                want_status: None,
                want_names: &["host", "content-length"],
            },
            Case {
                name: "an unsafe name is dropped for the CGI view",
                version: Version::HTTP_11,
                headers: &[("host", "e2e"), ("x_forwarded_for", "1.2.3.4")],
                superglobals: true,
                want_status: None,
                want_names: &["host"],
            },
            Case {
                name: "an unsafe name stays without the CGI view",
                version: Version::HTTP_11,
                headers: &[("host", "e2e"), ("x_forwarded_for", "1.2.3.4")],
                superglobals: false,
                want_status: None,
                want_names: &["host", "x_forwarded_for"],
            },
            Case {
                name: "HTTP/1.1 without a Host is 400",
                version: Version::HTTP_11,
                headers: &[],
                superglobals: true,
                want_status: Some(400),
                want_names: &[],
            },
        ];
        for case in cases {
            let mut p = parts(Method::POST, case.version, case.headers);
            let got = check_request(&mut p, case.superglobals, 1024);
            assert_eq!(
                got.as_ref().err().map(|r| r.status.as_u16()),
                case.want_status,
                "{}",
                case.name
            );
            if got.is_ok() {
                let names: Vec<&str> = p.headers.keys().map(HeaderName::as_str).collect();
                assert_eq!(names, case.want_names, "{}", case.name);
            }
        }
    }
}
