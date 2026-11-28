use std::borrow::Cow;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use http::{Extensions, StatusCode, header};
use http_body::Body as _;
use http_body_util::BodyExt;
use rapira_http_middleware::MiddlewareChain;
use rapira_sapi::types::{Authority, Chunks, Request, RequestUri};

use super::headers::*;
use super::respond::*;
use super::*;
use crate::responder::{ResponseSlot, Start, build_php_response, refusal};

fn base_req() -> Request {
    Request {
        method: Cow::Borrowed("GET"),
        uri: RequestUri::Static("/"),
        target: None,
        authority: Authority::None,
        https: false,
        protocol: "HTTP/1.1",
        remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        headers: HeaderMap::new(),
        body: Chunks::default(),
        received_at: 0.0,
    }
}

fn hdrs(lines: &[(&'static str, &'static str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for &(k, v) in lines {
        h.append(HeaderName::from_static(k), HeaderValue::from_static(v));
    }
    h
}

struct Captured {
    status: StatusCode,
    headers: http::HeaderMap,
    exact_len: Option<u64>,
    is_end_stream: bool,
    body: Vec<u8>,
    aborted: bool,
}

struct Attached {
    rt: tokio::runtime::Runtime,
    state: ExchangeState,
    slot: Arc<ResponseSlot>,
    is_head: bool,
}

fn try_poll_slot(slot: &ResponseSlot) -> Option<Start> {
    let mut cx = Context::from_waker(Waker::noop());
    match slot.poll_recv(&mut cx) {
        Poll::Ready(Ok(start)) => Some(start),
        _ => None,
    }
}

impl Attached {
    fn try_recv_start(&mut self) -> Option<Start> {
        try_poll_slot(&self.slot)
    }

    fn build_and_collect(&mut self) -> Captured {
        let start = self.try_recv_start().expect("expected Start message");
        let res = build_php_response(
            &MiddlewareChain::default(),
            self.is_head,
            false,
            &Extensions::new(),
            start,
        )
        .unwrap_or_else(refusal);
        let (parts, mut body) = res.into_parts();
        let exact_len = body.size_hint().exact();
        let is_end_stream = body.is_end_stream();
        let mut out = Vec::new();
        let mut aborted = false;
        self.rt.block_on(async {
            while let Some(frame) = body.frame().await {
                match frame {
                    Ok(f) => {
                        if let Ok(data) = f.into_data() {
                            out.extend_from_slice(&data);
                        }
                    }
                    Err(_) => {
                        aborted = true;
                        break;
                    }
                }
            }
        });
        Captured {
            status: parts.status,
            headers: parts.headers,
            exact_len,
            is_end_stream,
            body: out,
            aborted,
        }
    }
}

fn state_of(req: Request) -> Attached {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let is_head = req.method == "HEAD";
    let (responder, slot) = HttpResponder::pair(is_head);
    Attached {
        rt,
        state: ExchangeState::new(req, None, responder),
        slot,
        is_head,
    }
}

fn state() -> Attached {
    state_of(base_req())
}

/// A 304 head drops the accepted body chunks, like 204 and HEAD.
#[test]
fn seal_drops_the_body_for_304() {
    let mut a = state();
    assert_eq!(
        write_head_core(&mut a.state, 304, HeaderMap::new()),
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut a.state, b"gone", true) };
    assert_eq!(v, Verb::Ok);
    let cap = a.build_and_collect();
    assert_eq!(cap.status, StatusCode::NOT_MODIFIED);
    assert!(cap.is_end_stream);
    assert!(cap.body.is_empty());
}

/// The contract says that an empty chunk without eos does nothing. No head commits.
#[test]
fn empty_non_eos_chunk_commits_nothing() {
    let mut a = state();
    let v = unsafe { write_body_core(&mut a.state, b"", false) };
    assert_eq!(v, Verb::Ok);
    assert!(a.try_recv_start().is_none());
    assert_eq!(
        write_head_core(&mut a.state, 404, HeaderMap::new()),
        Verb::Ok,
        "the head slot must still be open"
    );
    let v = unsafe { write_body_core(&mut a.state, b"x", true) };
    assert_eq!(v, Verb::Ok);
    let cap = a.build_and_collect();
    assert_eq!(cap.status, StatusCode::NOT_FOUND);
    assert_eq!(cap.body, b"x");
}

/// `uri_abs` takes the host of `$uri` from the authority.
#[test]
fn uri_abs_spells_the_uri() {
    struct Case {
        name: &'static str,
        https: bool,
        authority: Option<&'static [u8]>,
        uri: &'static str,
        server: Addr,
        want_uri: &'static str,
    }
    let v6 = |ip: &str, port, scope| {
        Addr::Inet(std::net::SocketAddrV6::new(ip.parse().unwrap(), port, 0, scope).into())
    };
    let cases = [
        Case {
            name: "ipv4 server, no authority",
            https: false,
            authority: None,
            uri: "/a?b=1",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            want_uri: "http://127.0.0.1:8080/a?b=1",
        },
        Case {
            name: "ipv4 zero address and port",
            https: false,
            authority: None,
            uri: "/",
            server: Addr::Inet(([0, 0, 0, 0], 0).into()),
            want_uri: "http://0.0.0.0:0/",
        },
        Case {
            name: "ipv4 widest octets and port",
            https: true,
            authority: None,
            uri: "/",
            server: Addr::Inet(([255, 255, 255, 255], 65535).into()),
            want_uri: "https://255.255.255.255:65535/",
        },
        Case {
            name: "ipv6 loopback server",
            https: true,
            authority: None,
            uri: "/x",
            server: v6("::1", 443, 0),
            want_uri: "https://[::1]:443/x",
        },
        Case {
            name: "ipv6 scope id stays in the uri only",
            https: false,
            authority: None,
            uri: "/",
            server: v6("fe80::1", 80, 3),
            want_uri: "http://[fe80::1%3]:80/",
        },
        Case {
            name: "ipv4-mapped ipv6",
            https: false,
            authority: None,
            uri: "/",
            server: v6("::ffff:1.2.3.4", 1, 0),
            want_uri: "http://[::ffff:1.2.3.4]:1/",
        },
        Case {
            name: "authority wins over the server socket",
            https: false,
            authority: Some(b"example.com:8443"),
            uri: "/x",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            want_uri: "http://example.com:8443/x",
        },
        Case {
            name: "invalid utf-8 authority is replaced",
            https: false,
            authority: Some(b"h\xff"),
            uri: "/",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            want_uri: "http://h\u{fffd}/",
        },
        Case {
            name: "asterisk-form target collapses to the root",
            https: false,
            authority: Some(b"h"),
            uri: "*",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            want_uri: "http://h/",
        },
    ];
    for c in cases {
        let mut req = base_req();
        req.https = c.https;
        req.authority = match c.authority {
            Some(a) => Authority::Explicit(Bytes::from_static(a)),
            None => Authority::None,
        };
        req.uri = RequestUri::Static(c.uri);
        req.server = c.server;
        assert_eq!(uri_abs(&req), c.want_uri, "{}", c.name);
    }
}

/// A one-shot write carries exact size_hint; a streamed write leaves size_hint unbounded for chunked framing.
#[test]
fn head_frame_length_follows_the_write_shape() {
    let mut a = state();
    assert_eq!(
        unsafe { write_body_core(&mut a.state, b"abc", true) },
        Verb::Ok
    );
    let cap = a.build_and_collect();
    assert_eq!(cap.exact_len, Some(3));
    assert_eq!(cap.body, b"abc");

    let mut b = state();
    {
        let _enter = b.rt.enter();
        assert_eq!(
            unsafe { write_body_core(&mut b.state, b"abc", false) },
            Verb::Ok
        );
        unsafe { seal(&mut b.state) };
    }
    let cap = b.build_and_collect();
    assert_eq!(cap.exact_len, None);
    assert_eq!(cap.body, b"abc");
    assert!(!cap.aborted);
}

/// A write past the declared length sends the prefix that fits and seals without truncation.
#[test]
fn content_length_exceeded_sends_the_prefix_and_seals() {
    let mut a = state();
    assert_eq!(
        write_head_core(&mut a.state, 200, hdrs(&[("content-length", "5")])),
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut a.state, b"0123456789", true) };
    assert_eq!(v, Verb::ContentLengthExceeded);

    let cap = a.build_and_collect();
    assert_eq!(cap.exact_len, Some(5));
    assert_eq!(cap.body, b"01234");
    assert!(!cap.aborted);

    let v = unsafe { write_body_core(&mut a.state, b"x", true) };
    assert_eq!(v, Verb::Finalized, "nothing written after it");
}

/// A repeated content-length in the head table is a `\ValueError`.
#[test]
fn repeated_content_length_is_a_bad_field() {
    let mut a = state();
    let v = write_head_core(
        &mut a.state,
        200,
        hdrs(&[("content-length", "5"), ("content-length", "7")]),
    );
    assert!(matches!(v, Verb::BadField(_)));
    assert_eq!(
        unsafe { write_trailers_core(&mut a.state) },
        Verb::HeadNotWritten,
        "a rejected head commits nothing"
    );
}

/// An interim head puts nothing on the wire and leaves the final-head slot open.
#[test]
fn interim_head_sends_nothing_and_leaves_the_final_head_open() {
    let mut a = state();
    let fields = hdrs(&[("link", "</a.css>; rel=preload")]);
    assert_eq!(write_head_core(&mut a.state, 103, fields), Verb::Ok);
    assert!(
        a.try_recv_start().is_none(),
        "an interim head sends nothing"
    );
    assert_eq!(
        write_head_core(&mut a.state, 200, HeaderMap::new()),
        Verb::Ok,
        "the final-head slot stays open"
    );
}

/// When the client is gone, the state discards the unit once, and the unit stays discarded.
#[test]
fn gone_client_discards_once_and_stays_discarded() {
    let mut a = state();
    a.slot.cancel_if_incomplete();
    let v = unsafe { write_body_core(&mut a.state, b"x", false) };
    assert_eq!(v, Verb::Discarded);
    let v = unsafe { write_body_core(&mut a.state, b"y", true) };
    assert_eq!(v, Verb::Discarded, "sticky across repeat writes");
    assert!(a.state.is_finalized());
    assert!(a.state.cancelled());
}

/// `flush()` emits the implicit 200 once; a repeat flush puts nothing new on the wire.
#[test]
fn flush_emits_the_implicit_head_once() {
    let mut a = state();
    assert_eq!(flush_core(&mut a.state), Verb::Ok);
    assert!(matches!(a.try_recv_start(), Some(Start::Stream { .. })));
    assert_eq!(flush_core(&mut a.state), Verb::Ok);
    assert!(a.try_recv_start().is_none(), "a repeat flush is a no-op");
}

/// A committed 101 is bodiless, so the state accepts chunks and drops them, serving 502 on HTTP/1.1.
#[test]
fn a_101_head_drops_body_chunks() {
    let mut a = state();
    assert_eq!(
        write_head_core(&mut a.state, 101, HeaderMap::new()),
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut a.state, b"upgrade", true) };
    assert_eq!(v, Verb::Ok);
    let cap = a.build_and_collect();
    assert_eq!(cap.status, StatusCode::BAD_GATEWAY);
    assert!(cap.body.is_empty());
}

/// `writeTrailers()` ends the response cleanly.
#[test]
fn trailers_finalize_with_a_committed_head() {
    let mut a = state();
    assert_eq!(
        unsafe { write_trailers_core(&mut a.state) },
        Verb::HeadNotWritten,
        "nothing here commits a head"
    );
    assert_eq!(
        write_head_core(&mut a.state, 200, HeaderMap::new()),
        Verb::Ok
    );
    assert_eq!(unsafe { write_trailers_core(&mut a.state) }, Verb::Ok);
    let cap = a.build_and_collect();
    assert_eq!(cap.status, StatusCode::OK);
    assert_eq!(cap.exact_len, Some(0));
    assert!(cap.body.is_empty());
    assert_eq!(
        unsafe { write_trailers_core(&mut a.state) },
        Verb::Finalized
    );
}

/// The forbidden set covers the RFC 9110 §6.5.1 categories case-insensitively; unknown extension fields pass.
#[test]
fn trailer_denylist_matches_the_categories() {
    struct Case {
        name: &'static str,
        field: &'static str,
        forbidden: bool,
    }
    let cases = [
        Case {
            name: "mixed-case framing field",
            field: "Content-Length",
            forbidden: true,
        },
        Case {
            name: "lowercase authentication field",
            field: "authorization",
            forbidden: true,
        },
        Case {
            name: "extension field",
            field: "x-checksum",
            forbidden: false,
        },
        Case {
            name: "field that a trailer may carry",
            field: "server-timing",
            forbidden: false,
        },
    ];
    for case in cases {
        assert_eq!(
            forbidden_trailer(case.field),
            case.forbidden,
            "{}",
            case.name
        );
    }
}

/// A lost unit returns 500 when no head was sent, and truncates after a head was sent.
#[test]
fn a_lost_unit_fails_or_truncates_the_reply() {
    let mut a = state();
    a.state.lost();
    assert!(matches!(a.try_recv_start(), Some(Start::Error(500))));

    let mut b = state();
    assert_eq!(
        write_head_core(&mut b.state, 200, HeaderMap::new()),
        Verb::Ok
    );
    b.state.lost();
    assert!(matches!(b.try_recv_start(), Some(Start::Error(500))));

    let mut c = state();
    {
        let _enter = c.rt.enter();
        assert_eq!(
            unsafe { write_body_core(&mut c.state, b"abc", false) },
            Verb::Ok
        );
        c.state.lost();
    }
    let cap = c.build_and_collect();
    assert_eq!(cap.body, b"abc");
    assert!(cap.aborted, "lost mid-stream must abort the body");
}

/// A shed unit returns 503.
#[test]
fn a_shed_unit_gets_the_503_seal() {
    let Attached { state, slot, .. } = state();
    Box::new(state).shed();
    assert!(matches!(try_poll_slot(&slot), Some(Start::Error(503))));
}

#[test]
fn truncated_bodiless_response_omits_transfer_encoding() {
    use rapira_sapi::CgiResponder;

    struct Case {
        name: &'static str,
        method: &'static str,
        status: u16,
        want_status: StatusCode,
    }
    let cases = [
        Case {
            name: "truncated HEAD without content-length",
            method: "HEAD",
            status: 200,
            want_status: StatusCode::OK,
        },
        Case {
            name: "truncated 204",
            method: "GET",
            status: 204,
            want_status: StatusCode::NO_CONTENT,
        },
        Case {
            name: "truncated 304",
            method: "GET",
            status: 304,
            want_status: StatusCode::NOT_MODIFIED,
        },
        Case {
            name: "truncated 101 mapped to 502",
            method: "GET",
            status: 101,
            want_status: StatusCode::BAD_GATEWAY,
        },
    ];
    for case in cases {
        let mut req = base_req();
        req.method = Cow::Borrowed(case.method);
        let mut a = state_of(req);
        let responder = a.state.responder.take().expect("responder");
        Box::new(responder).respond(
            rapira_sapi::types::ResponseHead::new(case.status),
            b"partial".to_vec(),
            true,
        );
        let cap = a.build_and_collect();
        assert_eq!(cap.status, case.want_status, "{}", case.name);
        assert!(
            !cap.headers.contains_key(header::TRANSFER_ENCODING),
            "{}",
            case.name
        );
        assert_eq!(
            cap.headers
                .get(header::CONNECTION)
                .map(HeaderValue::as_bytes),
            Some(&b"close"[..]),
            "{}",
            case.name
        );
        assert!(cap.is_end_stream, "{}", case.name);
        assert!(cap.body.is_empty(), "{}", case.name);
        assert!(!cap.aborted, "{}", case.name);
    }
}

/// A one-shot response shorter than its declared Content-Length preserves the declared length and aborts the stream.
#[test]
fn declared_content_length_underrun_preserves_length_and_aborts() {
    let mut a = state();
    assert_eq!(
        write_head_core(&mut a.state, 200, hdrs(&[("content-length", "10")])),
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut a.state, b"abc", true) };
    assert_eq!(v, Verb::Ok);
    let cap = a.build_and_collect();
    assert_eq!(cap.status, StatusCode::OK);
    assert_eq!(cap.exact_len, Some(10));
    assert_eq!(cap.body, b"abc");
    assert!(cap.aborted);
}
