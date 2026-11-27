//! Pre-body HTTP request admission checks and CGI header name filtering.

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, HOST, HeaderName};
use http::{HeaderMap, StatusCode, Version};
use rapira_sapi::types::Authority;

use crate::Rejection;

/// Parses a non-empty ASCII decimal `Content-Length` without signs or overflow.
pub fn parse_content_length(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    let mut val: u64 = 0;
    for &b in raw {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(val)
}

/// True when `name` contains only ASCII letters, digits, and `-`, so `HTTP_*` CGI mangling cannot alias another header.
#[inline]
pub fn is_safe_field_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Drops request headers whose names could alias a CGI `HTTP_*` variable.
pub fn drop_unsafe_field_names(headers: &mut HeaderMap) {
    if headers.keys().all(|k| is_safe_field_name(k.as_str())) {
        return;
    }
    let bad: Vec<HeaderName> = headers
        .keys()
        .filter(|k| !is_safe_field_name(k.as_str()))
        .cloned()
        .collect();
    for key in bad {
        headers.remove(&key);
    }
}

/// Validates Host and Content-Length before middleware or body reads, and extracts the request authority.
pub fn check_request(
    parts: &mut http::request::Parts,
    superglobals: bool,
    max_body_size: usize,
) -> Result<Authority, Rejection> {
    let mut hosts = parts.headers.get_all(HOST).iter();
    let first_host = hosts.next();
    if hosts.next().is_some() {
        return Err(Rejection {
            status: StatusCode::BAD_REQUEST,
            reason: "repeated Host header".into(),
        });
    }
    let host_bytes = first_host.map(|v| v.as_bytes().trim_ascii());
    let uri_authority = parts.uri.authority().map(|a| a.as_str().as_bytes());

    let authority = match (host_bytes, uri_authority) {
        (Some(b""), _) => {
            if parts.version == Version::HTTP_11 {
                return Err(Rejection {
                    status: StatusCode::BAD_REQUEST,
                    reason: "empty Host header".into(),
                });
            }
            match uri_authority {
                Some(a) if !a.is_empty() => Authority::Explicit(Bytes::copy_from_slice(a)),
                _ => Authority::None,
            }
        }
        (Some(_), _) => Authority::HostHeader,
        (None, Some(a)) if !a.is_empty() => Authority::Explicit(Bytes::copy_from_slice(a)),
        (None, _) => {
            if parts.version == Version::HTTP_11 {
                return Err(Rejection {
                    status: StatusCode::BAD_REQUEST,
                    reason: "missing Host header".into(),
                });
            }
            Authority::None
        }
    };

    if let Some(cl_val) = parts.headers.get(CONTENT_LENGTH)
        && let Some(cl) = parse_content_length(cl_val.as_bytes().trim_ascii())
        && cl > max_body_size as u64
    {
        return Err(Rejection {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            reason: format!("request body of {cl} bytes exceeds max_body_size ({max_body_size})"),
        });
    }

    if superglobals {
        drop_unsafe_field_names(&mut parts.headers);
    }

    Ok(authority)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    #[test]
    fn check_request_validates_host_and_body_cap() {
        struct Case {
            name: &'static str,
            version: Version,
            uri: &'static str,
            headers: &'static [(&'static str, &'static str)],
            superglobals: bool,
            max_body: usize,
            want_status: Option<u16>,
            want_authority: Option<&'static [u8]>,
        }
        let cases = [
            Case {
                name: "http11 requires host",
                version: Version::HTTP_11,
                uri: "/",
                headers: &[],
                superglobals: false,
                max_body: 1024,
                want_status: Some(400),
                want_authority: None,
            },
            Case {
                name: "http11 rejects empty host",
                version: Version::HTTP_11,
                uri: "/",
                headers: &[("host", "   ")],
                superglobals: false,
                max_body: 1024,
                want_status: Some(400),
                want_authority: None,
            },
            Case {
                name: "repeated host rejected",
                version: Version::HTTP_11,
                uri: "/",
                headers: &[("host", "a"), ("host", "b")],
                superglobals: false,
                max_body: 1024,
                want_status: Some(400),
                want_authority: None,
            },
            Case {
                name: "http10 allows missing host",
                version: Version::HTTP_10,
                uri: "/ok",
                headers: &[],
                superglobals: false,
                max_body: 1024,
                want_status: None,
                want_authority: None,
            },
            Case {
                name: "declared content-length over cap returns 413",
                version: Version::HTTP_11,
                uri: "/",
                headers: &[("host", "example.com"), ("content-length", "2048")],
                superglobals: false,
                max_body: 1024,
                want_status: Some(413),
                want_authority: None,
            },
            Case {
                name: "valid http11 extracts host authority",
                version: Version::HTTP_11,
                uri: "/",
                headers: &[
                    ("host", "example.com:8080"),
                    ("x_bad", "1"),
                    ("x-good", "2"),
                ],
                superglobals: true,
                max_body: 1024,
                want_status: None,
                want_authority: Some(b"example.com:8080"),
            },
        ];

        for case in cases {
            let mut req = http::Request::builder()
                .version(case.version)
                .uri(case.uri)
                .body(())
                .unwrap();
            for &(k, v) in case.headers {
                req.headers_mut().append(
                    HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            let (mut parts, ()) = req.into_parts();
            let res = check_request(&mut parts, case.superglobals, case.max_body);
            match case.want_status {
                Some(code) => {
                    let err = res.expect_err(case.name);
                    assert_eq!(err.status.as_u16(), code, "{}", case.name);
                }
                None => {
                    let auth = res.expect(case.name);
                    let resolved = match &auth {
                        Authority::None => None,
                        Authority::HostHeader => {
                            parts.headers.get(HOST).map(|v| v.as_bytes().trim_ascii())
                        }
                        Authority::Explicit(b) => Some(b.as_ref()),
                    };
                    assert_eq!(resolved, case.want_authority, "{}", case.name);
                    if case.superglobals {
                        assert!(!parts.headers.contains_key("x_bad"), "{}", case.name);
                        assert!(parts.headers.contains_key("x-good"), "{}", case.name);
                    }
                }
            }
        }
    }
}
