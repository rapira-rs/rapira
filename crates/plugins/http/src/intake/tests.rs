use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method, Request as HttpRequest, StatusCode, Uri, Version};
use http_body::{Body, Frame};
use rapira_http_middleware::{
    Middleware, MiddlewareChain, Peer, PeerAddr, Phases, RequestCtx, RequestStep, ResponseHead,
    ShortResponse,
};

use super::*;

fn peer() -> Peer {
    Peer {
        remote: PeerAddr::Inet(([127, 0, 0, 1], 40000).into()),
        server: PeerAddr::Inet(([127, 0, 0, 1], 8000).into()),
        https: false,
        received_at: 1.5,
    }
}

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
        want_cl: Option<u64>,
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
            want_cl: None,
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
            want_cl: None,
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
            want_cl: None,
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
            want_cl: None,
        },
        Case {
            name: "http11 absolute-form without host rejected",
            version: Version::HTTP_11,
            uri: "http://example.test/path",
            headers: &[],
            superglobals: false,
            max_body: 1024,
            want_status: Some(400),
            want_authority: None,
            want_cl: None,
        },
        Case {
            name: "http10 absolute-form without host uses uri authority",
            version: Version::HTTP_10,
            uri: "http://example.test/path",
            headers: &[],
            superglobals: false,
            max_body: 1024,
            want_status: None,
            want_authority: Some(b"example.test"),
            want_cl: None,
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
            want_cl: None,
        },
        Case {
            name: "valid http11 extracts host authority and parsed content-length",
            version: Version::HTTP_11,
            uri: "/",
            headers: &[
                ("host", "example.com:8080"),
                ("content-length", "512"),
                ("x_bad", "1"),
                ("x-good", "2"),
            ],
            superglobals: true,
            max_body: 1024,
            want_status: None,
            want_authority: Some(b"example.com:8080"),
            want_cl: Some(512),
        },
    ];

    for case in cases {
        let mut req = HttpRequest::builder()
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
                let status = res.expect_err(case.name);
                assert_eq!(status.as_u16(), code, "{}", case.name);
            }
            None => {
                let admitted = res.expect(case.name);
                let resolved = match &admitted.authority {
                    Authority::None => None,
                    Authority::HostHeader => {
                        parts.headers.get(HOST).map(|v| v.as_bytes().trim_ascii())
                    }
                    Authority::Explicit(b) => Some(b.as_ref()),
                };
                assert_eq!(resolved, case.want_authority, "{}", case.name);
                assert_eq!(admitted.content_length, case.want_cl, "{}", case.name);
                if case.superglobals {
                    assert!(!parts.headers.contains_key("x_bad"), "{}", case.name);
                    assert!(parts.headers.contains_key("x-good"), "{}", case.name);
                }
            }
        }
    }
}

#[test]
fn headers_keep_the_per_name_wire_order() {
    let mut hdrs = HeaderMap::new();
    hdrs.append(
        HeaderName::from_static("x-probe"),
        HeaderValue::from_static("one"),
    );
    hdrs.append(
        HeaderName::from_static("accept"),
        HeaderValue::from_static("text/*"),
    );
    hdrs.append(
        HeaderName::from_static("x-probe"),
        HeaderValue::from_static("two"),
    );
    let built = build_request(
        Method::GET,
        Uri::from_static("/a/b?x=1"),
        Version::HTTP_11,
        hdrs,
        Authority::Explicit(Bytes::from_static(b"e2e")),
        Chunks::default(),
        peer(),
    );
    let probes: Vec<&[u8]> = built
        .headers
        .get_all("x-probe")
        .iter()
        .map(HeaderValue::as_bytes)
        .collect();
    assert_eq!(probes, [&b"one"[..], &b"two"[..]]);
    assert_eq!(built.protocol, "HTTP/1.1");
    assert_eq!(built.authority_bytes(), Some(&b"e2e"[..]));
    assert_eq!(built.received_at, 1.5);
}

/// PHP gets the origin-form view in `uri`; `target` keeps the absolute form and is None for the other forms. https://www.rfc-editor.org/rfc/rfc9112#section-3.2.2
#[test]
fn the_target_forms_map_to_uri_and_target() {
    struct Case {
        name: &'static str,
        method: &'static str,
        uri: &'static str,
        want_uri: &'static str,
        want_target: Option<&'static [u8]>,
    }
    let cases = [
        Case {
            name: "origin-form keeps the uri and has no target",
            method: "GET",
            uri: "/a/b?x=1",
            want_uri: "/a/b?x=1",
            want_target: None,
        },
        Case {
            name: "absolute-form yields an origin-form uri",
            method: "GET",
            uri: "http://h.example/admin?x=1",
            want_uri: "/admin?x=1",
            want_target: Some(b"http://h.example/admin?x=1"),
        },
        Case {
            name: "absolute-form without a path roots the uri",
            method: "GET",
            uri: "http://h.example?x=1",
            want_uri: "/?x=1",
            want_target: Some(b"http://h.example/?x=1"),
        },
        Case {
            name: "asterisk-form passes through",
            method: "OPTIONS",
            uri: "*",
            want_uri: "*",
            want_target: None,
        },
    ];
    for case in cases {
        let built = build_request(
            Method::from_bytes(case.method.as_bytes()).unwrap(),
            Uri::try_from(case.uri).unwrap(),
            Version::HTTP_11,
            HeaderMap::new(),
            Authority::None,
            Chunks::default(),
            peer(),
        );
        assert_eq!(built.uri.as_str(), case.want_uri, "{}", case.name);
        assert_eq!(built.target.as_deref(), case.want_target, "{}", case.name);
    }
}

enum Step {
    Data(Bytes),
    Trailer,
    Err,
    Stall,
}

struct TestBody {
    steps: VecDeque<Step>,
    polled: Arc<AtomicBool>,
    end_immediately: bool,
}

impl TestBody {
    fn from_steps(steps: Vec<Step>, end_immediately: bool) -> (Self, Arc<AtomicBool>) {
        let polled = Arc::new(AtomicBool::new(false));
        (
            Self {
                steps: steps.into(),
                polled: Arc::clone(&polled),
                end_immediately,
            },
            polled,
        )
    }
}

impl Body for TestBody {
    type Data = Bytes;
    type Error = io::Error;

    fn is_end_stream(&self) -> bool {
        self.end_immediately && self.steps.is_empty()
    }

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.polled.store(true, Ordering::Relaxed);
        if matches!(self.steps.front(), Some(Step::Stall)) {
            return Poll::Pending;
        }
        match self.steps.pop_front() {
            None => Poll::Ready(None),
            Some(Step::Data(b)) => Poll::Ready(Some(Ok(Frame::data(b)))),
            Some(Step::Trailer) => Poll::Ready(Some(Ok(Frame::trailers(HeaderMap::new())))),
            Some(Step::Err) => Poll::Ready(Some(Err(io::Error::other("stream broken")))),
            Some(Step::Stall) => unreachable!(),
        }
    }
}

struct ShortCircuitMw;

impl Middleware for ShortCircuitMw {
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }

    fn on_request<'a>(&'a self, _ctx: &'a mut RequestCtx) -> RequestStep<'a> {
        RequestStep::Respond(ShortResponse::full(
            ResponseHead::new(202),
            Bytes::from_static(b"short"),
        ))
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn prepare_body_limits_coalescing_and_multipart() {
    let dir = std::env::temp_dir().join(format!("rapira-intake-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp upload dir");
    let _spool = TempDir(dir.clone());
    let upload_limits = multipart::Limits {
        dir,
        max_file_size: 4096,
        max_field_size: 4096,
        max_files: 4,
        max_parts: 16,
        max_part_headers: 16,
    };

    const MP_BODY: &[u8] = concat!(
        "--b1\r\n",
        "Content-Disposition: form-data; name=\"field\"\r\n\r\n",
        "val\r\n",
        "--b1\r\n",
        "Content-Disposition: form-data; name=\"up\"; filename=\"a.txt\"\r\n",
        "Content-Type: text/plain\r\n\r\n",
        "0123456789abcdef\r\n",
        "--b1--\r\n",
    )
    .as_bytes();
    const SPOOLED_FILE_LEN: usize = 16;

    enum BodySpec {
        EmptyEndStream,
        SingleFrame,
        SmallCoalesceAndLarge,
        OverMaxStreamed,
        StallTimeout,
        ReadError,
        TrailerIgnored,
        MultipartUpload,
    }
    struct Case {
        name: &'static str,
        headers: &'static [(&'static str, &'static str)],
        use_uploads: bool,
        use_short_mw: bool,
        max_body_size: usize,
        body: BodySpec,
        want_err: Option<StatusCode>,
        want_short: bool,
        want_body_polled: bool,
        want_raw_len: usize,
        want_budget: usize,
        want_multipart_files: usize,
    }

    let cases = [
        Case {
            name: "empty end-stream body skips polling with zero budget",
            headers: &[("host", "example.test")],
            use_uploads: true,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::EmptyEndStream,
            want_err: None,
            want_short: false,
            want_body_polled: false,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
        Case {
            name: "empty body with repeated multipart Content-Type rejects with 400",
            headers: &[
                ("host", "example.test"),
                ("content-type", "multipart/form-data; boundary=a"),
                ("content-type", "text/plain"),
            ],
            use_uploads: true,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::EmptyEndStream,
            want_err: Some(StatusCode::BAD_REQUEST),
            want_short: false,
            want_body_polled: false,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
        Case {
            name: "empty multipart POST without boundary passes through raw",
            headers: &[
                ("host", "example.test"),
                ("content-type", "multipart/form-data"),
            ],
            use_uploads: true,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::EmptyEndStream,
            want_err: None,
            want_short: false,
            want_body_polled: false,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
        Case {
            name: "single frame stays inline with matching budget",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::SingleFrame,
            want_err: None,
            want_short: false,
            want_body_polled: true,
            want_raw_len: 5,
            want_budget: 5,
            want_multipart_files: 0,
        },
        Case {
            name: "small frames coalesce and large frame stays separate",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: false,
            max_body_size: 64 << 10,
            body: BodySpec::SmallCoalesceAndLarge,
            want_err: None,
            want_short: false,
            want_body_polled: true,
            want_raw_len: 10 + COALESCE_MAX,
            want_budget: 10 + COALESCE_MAX,
            want_multipart_files: 0,
        },
        Case {
            name: "streamed body without Content-Length exceeding max_body_size returns 413",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: false,
            max_body_size: 8,
            body: BodySpec::OverMaxStreamed,
            want_err: Some(StatusCode::PAYLOAD_TOO_LARGE),
            want_short: false,
            want_body_polled: true,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
        Case {
            name: "stalled body frame times out with 408",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::StallTimeout,
            want_err: Some(StatusCode::REQUEST_TIMEOUT),
            want_short: false,
            want_body_polled: true,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
        Case {
            name: "body stream read error returns 400",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::ReadError,
            want_err: Some(StatusCode::BAD_REQUEST),
            want_short: false,
            want_body_polled: true,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
        Case {
            name: "trailer frames are ignored while data frames are preserved",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: false,
            max_body_size: 1024,
            body: BodySpec::TrailerIgnored,
            want_err: None,
            want_short: false,
            want_body_polled: true,
            want_raw_len: 3,
            want_budget: 3,
            want_multipart_files: 0,
        },
        Case {
            name: "multipart upload subtracts spooled file bytes from pool budget",
            headers: &[
                ("host", "example.test"),
                ("content-type", "multipart/form-data; boundary=b1"),
            ],
            use_uploads: true,
            use_short_mw: false,
            max_body_size: 4096,
            body: BodySpec::MultipartUpload,
            want_err: None,
            want_short: false,
            want_body_polled: true,
            want_raw_len: 0,
            want_budget: MP_BODY.len() - SPOOLED_FILE_LEN,
            want_multipart_files: 1,
        },
        Case {
            name: "Phase-1 middleware short-circuit returns Short without polling body",
            headers: &[("host", "example.test")],
            use_uploads: false,
            use_short_mw: true,
            max_body_size: 1024,
            body: BodySpec::StallTimeout,
            want_err: None,
            want_short: true,
            want_body_polled: false,
            want_raw_len: 0,
            want_budget: 0,
            want_multipart_files: 0,
        },
    ];

    for case in cases {
        let (body, polled) = match case.body {
            BodySpec::EmptyEndStream => TestBody::from_steps(vec![], true),
            BodySpec::SingleFrame => {
                TestBody::from_steps(vec![Step::Data(Bytes::from_static(b"hello"))], false)
            }
            BodySpec::SmallCoalesceAndLarge => TestBody::from_steps(
                vec![
                    Step::Data(Bytes::from_static(b"12345")),
                    Step::Data(Bytes::from_static(b"67890")),
                    Step::Data(Bytes::from(vec![b'x'; COALESCE_MAX])),
                ],
                false,
            ),
            BodySpec::OverMaxStreamed => TestBody::from_steps(
                vec![
                    Step::Data(Bytes::from_static(b"12345")),
                    Step::Data(Bytes::from_static(b"6789")),
                ],
                false,
            ),
            BodySpec::StallTimeout => TestBody::from_steps(vec![Step::Stall], false),
            BodySpec::ReadError => TestBody::from_steps(vec![Step::Err], false),
            BodySpec::TrailerIgnored => TestBody::from_steps(
                vec![Step::Data(Bytes::from_static(b"abc")), Step::Trailer],
                false,
            ),
            BodySpec::MultipartUpload => {
                TestBody::from_steps(vec![Step::Data(Bytes::from_static(MP_BODY))], false)
            }
        };

        let mut builder = HttpRequest::builder()
            .method(Method::POST)
            .uri("/upload")
            .version(Version::HTTP_11);
        for &(k, v) in case.headers {
            builder = builder.header(k, v);
        }
        let req = builder.body(body).unwrap();
        let chain = if case.use_short_mw {
            MiddlewareChain::new(vec![Arc::new(ShortCircuitMw)])
        } else {
            MiddlewareChain::default()
        };
        let limits = IntakeLimits {
            superglobals: false,
            max_body_size: case.max_body_size,
            body_timeout: Duration::from_millis(50),
            uploads: case.use_uploads.then_some(&upload_limits),
        };

        let out = match admit(req, peer(), &chain, &limits) {
            Ok(Admit::Ready(request)) => Ok(Prepared::Dispatch {
                request,
                multipart: None,
                budget_bytes: 0,
                extensions: Extensions::new(),
            }),
            Ok(Admit::Pending(pending)) => pending.finish(&chain, &limits).await,
            Err(rej) => Err(rej),
        };
        assert_eq!(
            polled.load(Ordering::Relaxed),
            case.want_body_polled,
            "{}",
            case.name
        );

        if let Some(want_status) = case.want_err {
            let status = match out {
                Ok(_) => panic!("{}: expected rejection {want_status}", case.name),
                Err(e) => e,
            };
            assert_eq!(status, want_status, "{}", case.name);
            continue;
        }

        match out.expect(case.name) {
            Prepared::Short { short, .. } => {
                assert!(case.want_short, "{}", case.name);
                assert_eq!(short.head.status, 202, "{}", case.name);
            }
            Prepared::Dispatch {
                request,
                multipart,
                budget_bytes,
                ..
            } => {
                assert!(!case.want_short, "{}", case.name);
                assert_eq!(request.body.len(), case.want_raw_len, "{}", case.name);
                assert_eq!(budget_bytes, case.want_budget, "{}", case.name);
                let got_files = multipart.as_ref().map_or(0, |m| m.files.len());
                assert_eq!(got_files, case.want_multipart_files, "{}", case.name);
            }
        }
    }
}
