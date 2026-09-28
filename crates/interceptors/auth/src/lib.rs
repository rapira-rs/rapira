mod config;

pub use config::{Section, Settings, resolve};

use anyhow::{Context, bail};
use connectrpc::{ConnectError, Interceptor, RequestHead};
use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use http::{HeaderMap, HeaderValue};

/// The gRPC health service. Kubernetes gRPC probes cannot send metadata, so it needs no token.
const HEALTH: &str = "/grpc.health.v1.Health/";

/// Accepts a call that carries one of the configured bearer tokens. `grpc.health.v1.Health` needs no token.
pub struct Auth {
    tokens: Vec<Box<[u8]>>,
}

impl Auth {
    pub fn load(settings: &Settings) -> anyhow::Result<Auth> {
        let path = settings.tokens_file.display();
        let text = std::fs::read_to_string(&settings.tokens_file)
            .with_context(|| format!("reading grpc.auth.tokens_file `{path}`"))?;
        let tokens = parse(&text).with_context(|| format!("grpc.auth.tokens_file `{path}`"))?;
        Ok(Auth { tokens })
    }

    /// True for exactly one `authorization` value `Bearer <token>` with a configured token.
    fn accepts(&self, headers: &HeaderMap) -> bool {
        let mut values = headers.get_all(AUTHORIZATION).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return false;
        };
        let Some(token) = bearer_token(value.as_bytes()) else {
            return false;
        };
        // Every token is compared, so the time does not show which token matched.
        self.tokens
            .iter()
            .fold(false, |found, t| found | same(t, token))
    }
}

#[connectrpc::async_trait]
impl Interceptor for Auth {
    async fn intercept_head(&self, head: &mut RequestHead<'_>) -> Result<(), ConnectError> {
        if head.path().starts_with(HEALTH) || self.accepts(head.headers()) {
            return Ok(());
        }
        // A 401 must carry a challenge: https://www.rfc-editor.org/rfc/rfc9110#section-15.5.2
        let mut challenge = HeaderMap::new();
        challenge.insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        Err(
            ConnectError::unauthenticated("missing or invalid bearer token")
                .with_headers(challenge),
        )
    }
}

/// The token of `Bearer 1*SP b64token`. https://www.rfc-editor.org/rfc/rfc6750#section-2.1
fn bearer_token(value: &[u8]) -> Option<&[u8]> {
    let (scheme, rest) = value.split_at_checked(6)?;
    if !scheme.eq_ignore_ascii_case(b"Bearer") {
        return None;
    }
    let spaces = rest.iter().take_while(|&&b| b == b' ').count();
    let token = &rest[spaces..];
    (spaces > 0 && is_b64token(token)).then_some(token)
}

/// `1*( ALPHA / DIGIT / "-" / "." / "_" / "~" / "+" / "/" ) *"="`. https://www.rfc-editor.org/rfc/rfc6750#section-2.1
fn is_b64token(token: &[u8]) -> bool {
    let body = token.len() - token.iter().rev().take_while(|&&b| b == b'=').count();
    body > 0
        && token[..body]
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
}

/// Compares every byte when the lengths are equal.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// One token per line. Blank lines and `#` lines are skipped. An error names the line, never its text.
fn parse(text: &str) -> anyhow::Result<Vec<Box<[u8]>>> {
    let mut tokens = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !is_b64token(line.as_bytes()) {
            bail!("line {} is not a valid bearer token", i + 1);
        }
        tokens.push(line.as_bytes().into());
    }
    if tokens.is_empty() {
        bail!("the file has no token");
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;
    use http::header::AUTHORIZATION;

    use super::*;

    /// Expected values: the `b64token` grammar of RFC 6750 §2.1, and the file format of the spec (one token per line, trimmed, blank lines and `#` lines skipped, at least one token).
    #[test]
    fn tokens_file_parses() {
        struct Case {
            name: &'static str,
            text: &'static str,
            expected: Result<&'static [&'static str], &'static str>,
        }
        let cases = [
            Case {
                name: "one token",
                text: "alpha-token\n",
                expected: Ok(&["alpha-token"]),
            },
            Case {
                name: "comments and blank lines",
                text: "# ci\n\nalpha\n  # rotated 2026-09\nbeta\n",
                expected: Ok(&["alpha", "beta"]),
            },
            Case {
                name: "surrounding whitespace",
                text: "  alpha \t\n",
                expected: Ok(&["alpha"]),
            },
            Case {
                name: "every b64token character and padding",
                text: "aZ09-._~+/==\n",
                expected: Ok(&["aZ09-._~+/=="]),
            },
            Case {
                name: "crlf line endings",
                text: "alpha\r\nbeta\r\n",
                expected: Ok(&["alpha", "beta"]),
            },
            Case {
                name: "no final newline",
                text: "alpha",
                expected: Ok(&["alpha"]),
            },
            Case {
                name: "space inside a token",
                text: "alpha\nnot a token\n",
                expected: Err("line 2 is not a valid bearer token"),
            },
            Case {
                name: "padding before other characters",
                text: "ab=c\n",
                expected: Err("line 1 is not a valid bearer token"),
            },
            Case {
                name: "padding only",
                text: "==\n",
                expected: Err("line 1 is not a valid bearer token"),
            },
            Case {
                name: "utf-8 bom",
                text: "\u{feff}alpha\n",
                expected: Err("line 1 is not a valid bearer token"),
            },
            Case {
                name: "comments only",
                text: "# nothing yet\n\n",
                expected: Err("the file has no token"),
            },
            Case {
                name: "empty file",
                text: "",
                expected: Err("the file has no token"),
            },
        ];
        for case in cases {
            let got = parse(case.text);
            match (got, case.expected) {
                (Ok(tokens), Ok(want)) => {
                    let want: Vec<Box<[u8]>> = want.iter().map(|t| t.as_bytes().into()).collect();
                    assert_eq!(tokens, want, "{}", case.name);
                }
                (Err(e), Err(want)) => {
                    assert_eq!(e.to_string(), want, "{}", case.name);
                    assert!(
                        !e.to_string().contains("not a token"),
                        "{}: the error shows the line",
                        case.name
                    );
                }
                (got, want) => panic!("{}: got {got:?}, want {want:?}", case.name),
            }
        }
    }

    /// Expected values: RFC 6750 §2.1 (`credentials = "Bearer" 1*SP b64token`) and RFC 9110 §11.1 (the scheme is case-insensitive). The spec accepts exactly one `authorization` value.
    #[test]
    fn authorization_is_checked() {
        struct Case {
            name: &'static str,
            values: &'static [&'static [u8]],
            accepted: bool,
        }
        let cases = [
            Case {
                name: "valid token",
                values: &[b"Bearer alpha"],
                accepted: true,
            },
            Case {
                name: "second token",
                values: &[b"Bearer beta=="],
                accepted: true,
            },
            Case {
                name: "lower-case scheme",
                values: &[b"bearer alpha"],
                accepted: true,
            },
            Case {
                name: "upper-case scheme",
                values: &[b"BEARER alpha"],
                accepted: true,
            },
            Case {
                name: "several spaces",
                values: &[b"Bearer   alpha"],
                accepted: true,
            },
            Case {
                name: "no header",
                values: &[],
                accepted: false,
            },
            Case {
                name: "wrong token",
                values: &[b"Bearer gamma"],
                accepted: false,
            },
            Case {
                name: "prefix of a token",
                values: &[b"Bearer alph"],
                accepted: false,
            },
            Case {
                name: "token with a suffix",
                values: &[b"Bearer alphaa"],
                accepted: false,
            },
            Case {
                name: "other scheme",
                values: &[b"Basic alpha"],
                accepted: false,
            },
            Case {
                name: "scheme only",
                values: &[b"Bearer"],
                accepted: false,
            },
            Case {
                name: "scheme and a space",
                values: &[b"Bearer "],
                accepted: false,
            },
            Case {
                name: "no space",
                values: &[b"Beareralpha"],
                accepted: false,
            },
            Case {
                name: "tab after the scheme",
                values: &[b"Bearer\talpha"],
                accepted: false,
            },
            Case {
                name: "trailing space",
                values: &[b"Bearer alpha "],
                accepted: false,
            },
            Case {
                name: "token only",
                values: &[b"alpha"],
                accepted: false,
            },
            Case {
                name: "non-ascii value",
                values: &[b"Bearer alph\xe9"],
                accepted: false,
            },
            Case {
                name: "two values",
                values: &[b"Bearer alpha", b"Bearer alpha"],
                accepted: false,
            },
        ];
        let auth = Auth {
            tokens: vec![b"alpha".as_slice().into(), b"beta==".as_slice().into()],
        };
        for case in cases {
            let mut headers = HeaderMap::new();
            for value in case.values {
                headers.append(
                    AUTHORIZATION,
                    HeaderValue::from_bytes(value).expect("a header value"),
                );
            }
            assert_eq!(auth.accepts(&headers), case.accepted, "{}", case.name);
        }
    }
}
