use super::headers::*;
use super::respond::*;
use super::*;
use rapira_sapi::types::Request;
use rapira_sapi::zend_string;
use std::path::PathBuf;

fn base_req() -> Request {
    Request {
        method: String::new(),
        uri: "/".into(),
        target: None,
        authority: None,
        https: false,
        protocol: "HTTP/1.1",
        remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server_name: "",
        server_port: 8080,
        headers: HeaderMap::new(),
        content_type: None,
        content_length: 0,
        body: Body::Raw(std::io::Cursor::new(Vec::new())),
        received_at: 0.0,
    }
}

fn map(lines: &[(&'static str, &'static str)]) -> HeaderMap {
    lines
        .iter()
        .map(|&(k, v)| (HeaderName::from_static(k), HeaderValue::from_static(v)))
        .collect()
}

enum Sealed {
    Complete { status: u16, body: Vec<u8> },
    Truncated,
    Nothing,
}

fn recv_sealed(rx: &mut tokio::sync::mpsc::Receiver<rapira_sapi::types::Frame>) -> Sealed {
    let (mut status, mut body, mut saw_frames) = (None, Vec::new(), false);
    while let Ok(frame) = rx.try_recv() {
        saw_frames = true;
        match frame {
            rapira_sapi::types::Frame::File { .. } => {}
            rapira_sapi::types::Frame::Head { head, .. } => status = Some(head.status),
            rapira_sapi::types::Frame::Chunk(b) => body.extend_from_slice(&b),
            rapira_sapi::types::Frame::End { truncated, .. } => {
                return match (truncated, status) {
                    (true, _) => Sealed::Truncated,
                    (false, Some(status)) => Sealed::Complete { status, body },
                    (false, None) => Sealed::Nothing,
                };
            }
        }
    }
    if saw_frames {
        panic!("stream carried frames but no End");
    }
    Sealed::Nothing
}

/// A zend_string in test memory. write_body_core copies a piece below ZERO_COPY_MIN, so no ZendMM call reaches it.
struct Piece(Vec<u64>);

impl Piece {
    fn new(bytes: &[u8]) -> Self {
        let head = std::mem::offset_of!(zend_string, val);
        let mut words = vec![0u64; (head + bytes.len() + 1).div_ceil(8)];
        let s = words.as_mut_ptr().cast::<zend_string>();
        // SAFETY: words holds the header and bytes.len() + 1 bytes from val on.
        unsafe {
            (*s).len = bytes.len();
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                (&raw mut (*s).val).cast::<u8>(),
                bytes.len(),
            );
        }
        Self(words)
    }

    fn ptr(&mut self) -> *mut zend_string {
        self.0.as_mut_ptr().cast()
    }
}

/// An exchange state as the submit makes it.
struct Attached {
    state: ExchangeState,
    /// The reply that `state` sends to.
    reply: tokio::sync::mpsc::Receiver<rapira_sapi::types::Frame>,
}

/// Channel is sized for a full event trio so seal never parks with no reader.
fn state_of(req: Request) -> Attached {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    Attached {
        state: ExchangeState::new(req, tx),
        reply: rx,
    }
}

fn state() -> Attached {
    state_of(base_req())
}

/// A 304 head drops accepted body chunks at seal, like 204 and HEAD.
#[test]
fn seal_drops_the_body_for_304() {
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    assert_eq!(
        unsafe { write_head_core(&mut st, 304, HeaderMap::new()) },
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut st, Piece::new(b"gone").ptr(), true) };
    assert_eq!(v, Verb::Ok);
    let Sealed::Complete { status, body } = recv_sealed(&mut rx) else {
        panic!("must seal cleanly");
    };
    assert_eq!(status, 304);
    assert!(body.is_empty(), "304 carries no body");
}

/// Contract: an empty chunk without eos does nothing - no head commits.
#[test]
fn empty_non_eos_chunk_commits_nothing() {
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    let v = unsafe { write_body_core(&mut st, Piece::new(b"").ptr(), false) };
    assert_eq!(v, Verb::Ok);
    assert_eq!(
        unsafe { write_head_core(&mut st, 404, HeaderMap::new()) },
        Verb::Ok,
        "the head slot must still be open"
    );
    let v = unsafe { write_body_core(&mut st, Piece::new(b"x").ptr(), true) };
    assert_eq!(v, Verb::Ok);
    let Sealed::Complete { status, .. } = recv_sealed(&mut rx) else {
        panic!("must seal cleanly");
    };
    assert_eq!(status, 404);
}

/// The view normalizes the protocol spelling and maps an empty unix path to the unnamed endpoint.
#[test]
fn view_normalizes_protocol_and_empty_unix_path() {
    let mut req = base_req();
    req.remote = Addr::Unix(Some(PathBuf::new()));
    assert_eq!(protocol_php("HTTP/2.0"), "HTTP/2");
    assert!(matches!(
        RequestView::new(&req).remote,
        AddrOwned::Unix(None)
    ));
}

/// The view spells `$uri` and the address ips as the `Display` of the socket addresses: IPv6 in brackets with its scope id, the unix fallback as `server_name:server_port`.
#[test]
fn view_spells_uri_and_addresses() {
    struct Case {
        name: &'static str,
        https: bool,
        authority: Option<&'static [u8]>,
        uri: &'static str,
        server: Addr,
        remote: Addr,
        want_uri: &'static str,
        want_remote: AddrOwned,
    }
    let v6 = |ip: &str, port, scope| {
        Addr::Inet(std::net::SocketAddrV6::new(ip.parse().unwrap(), port, 0, scope).into())
    };
    let inet = |ip: &str| AddrOwned::Inet {
        ip: ip.into(),
        port: 0,
    };
    let cases = [
        Case {
            name: "ipv4 server, no authority",
            https: false,
            authority: None,
            uri: "/a?b=1",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            remote: Addr::Inet(([10, 0, 0, 1], 5000).into()),
            want_uri: "http://127.0.0.1:8080/a?b=1",
            want_remote: AddrOwned::Inet {
                ip: "10.0.0.1".into(),
                port: 5000,
            },
        },
        Case {
            name: "ipv4 zero address and port",
            https: false,
            authority: None,
            uri: "/",
            server: Addr::Inet(([0, 0, 0, 0], 0).into()),
            remote: Addr::Inet(([0, 0, 0, 0], 0).into()),
            want_uri: "http://0.0.0.0:0/",
            want_remote: inet("0.0.0.0"),
        },
        Case {
            name: "ipv4 widest octets and port",
            https: true,
            authority: None,
            uri: "/",
            server: Addr::Inet(([255, 255, 255, 255], 65535).into()),
            remote: Addr::Inet(([192, 168, 100, 9], 65535).into()),
            want_uri: "https://255.255.255.255:65535/",
            want_remote: AddrOwned::Inet {
                ip: "192.168.100.9".into(),
                port: 65535,
            },
        },
        Case {
            name: "ipv6 loopback server",
            https: true,
            authority: None,
            uri: "/x",
            server: v6("::1", 443, 0),
            remote: v6("2001:db8::1", 0, 0),
            want_uri: "https://[::1]:443/x",
            want_remote: inet("2001:db8::1"),
        },
        Case {
            name: "ipv6 scope id stays in the uri only",
            https: false,
            authority: None,
            uri: "/",
            server: v6("fe80::1", 80, 3),
            remote: v6("fe80::1", 0, 3),
            want_uri: "http://[fe80::1%3]:80/",
            want_remote: inet("fe80::1"),
        },
        Case {
            name: "ipv4-mapped ipv6",
            https: false,
            authority: None,
            uri: "/",
            server: v6("::ffff:1.2.3.4", 1, 0),
            remote: v6("::ffff:1.2.3.4", 0, 0),
            want_uri: "http://[::ffff:1.2.3.4]:1/",
            want_remote: inet("::ffff:1.2.3.4"),
        },
        Case {
            name: "authority wins over the server socket",
            https: false,
            authority: Some(b"example.com:8443"),
            uri: "/x",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            remote: Addr::Inet(([127, 0, 0, 1], 0).into()),
            want_uri: "http://example.com:8443/x",
            want_remote: inet("127.0.0.1"),
        },
        Case {
            name: "invalid utf-8 authority is replaced",
            https: false,
            authority: Some(b"h\xff"),
            uri: "/",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            remote: Addr::Inet(([127, 0, 0, 1], 0).into()),
            want_uri: "http://h\u{fffd}/",
            want_remote: inet("127.0.0.1"),
        },
        Case {
            name: "asterisk-form target collapses to the root",
            https: false,
            authority: Some(b"h"),
            uri: "*",
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            remote: Addr::Inet(([127, 0, 0, 1], 0).into()),
            want_uri: "http://h/",
            want_remote: inet("127.0.0.1"),
        },
        Case {
            name: "unix server, no authority",
            https: false,
            authority: None,
            uri: "/p",
            server: Addr::Unix(Some("/run/r.sock".into())),
            remote: Addr::Unix(Some("/run/peer.sock".into())),
            want_uri: "http://localhost:80/p",
            want_remote: AddrOwned::Unix(Some(b"/run/peer.sock".to_vec())),
        },
        Case {
            name: "unnamed unix peer",
            https: false,
            authority: None,
            uri: "/",
            server: Addr::Unix(None),
            remote: Addr::Unix(None),
            want_uri: "http://localhost:80/",
            want_remote: AddrOwned::Unix(None),
        },
    ];
    for c in cases {
        let mut req = base_req();
        req.https = c.https;
        req.authority = c.authority.map(<[u8]>::to_vec);
        req.uri = c.uri.into();
        req.server = c.server;
        req.remote = c.remote;
        req.server_name = "localhost";
        req.server_port = 80;
        let view = RequestView::new(&req);
        assert_eq!(view.uri_abs, c.want_uri, "{}", c.name);
        assert_eq!(view.remote, c.want_remote, "{}", c.name);
    }
}

/// A one-shot write carries its computed length on the Head frame; a streamed write leaves framing to the plugin.
#[test]
fn head_frame_length_follows_the_write_shape() {
    use rapira_sapi::types::Frame;
    struct Case {
        name: &'static str,
        eos: bool,
        want: Option<u64>,
    }
    let cases = [
        Case {
            name: "one-shot write",
            eos: true,
            want: Some(3),
        },
        Case {
            name: "streamed write: the plugin frames",
            eos: false,
            want: None,
        },
    ];
    for case in cases {
        let Attached {
            state: mut st,
            reply: mut rx,
        } = state();
        let v = unsafe { write_body_core(&mut st, Piece::new(b"abc").ptr(), case.eos) };
        assert_eq!(v, Verb::Ok, "{}", case.name);
        let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
            panic!("{}: head first", case.name);
        };
        assert_eq!(content_length, case.want, "{}", case.name);
    }
}

/// Over-declared length sends the fitting prefix and seals untruncated, so later writes see Finalized.
#[test]
fn content_length_exceeded_sends_the_prefix_and_seals() {
    use rapira_sapi::types::Frame;
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    let v = unsafe { write_head_core(&mut st, 200, map(&[("content-length", "5")])) };
    assert_eq!(v, Verb::Ok);
    let v = unsafe { write_body_core(&mut st, Piece::new(b"0123456789").ptr(), true) };
    assert_eq!(v, Verb::ContentLengthExceeded);

    let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
        panic!("head first");
    };
    assert_eq!(content_length, Some(5), "the declared length is honoured");
    let Ok(Frame::Chunk(b)) = rx.try_recv() else {
        panic!("the fitting prefix must be sent");
    };
    assert_eq!(&b[..], b"01234");
    let Ok(Frame::End { truncated, .. }) = rx.try_recv() else {
        panic!("sealed");
    };
    assert!(!truncated, "complete per its declaration");

    let v = unsafe { write_body_core(&mut st, Piece::new(b"x").ptr(), true) };
    assert_eq!(v, Verb::Finalized, "nothing written after it");
}

/// A repeated content-length in the head table is a `\ValueError`.
#[test]
fn repeated_content_length_is_a_bad_field() {
    let Attached {
        state: mut st,
        reply: _rx,
    } = state();
    let v = unsafe {
        write_head_core(
            &mut st,
            200,
            map(&[("content-length", "5"), ("content-length", "7")]),
        )
    };
    assert!(matches!(v, Verb::BadField(_)));
    assert_eq!(st.stage, Stage::Open, "a rejected head commits nothing");
}

/// An interim head puts nothing on the reply stream and leaves the final-head slot open.
#[test]
fn interim_head_sends_nothing_and_leaves_the_final_head_open() {
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    let fields = map(&[("link", "</a.css>; rel=preload")]);
    let v = unsafe { write_head_core(&mut st, 103, fields) };
    assert_eq!(v, Verb::Ok);
    assert!(rx.try_recv().is_err(), "an interim head reaches no frame");
    let v = unsafe { write_head_core(&mut st, 200, HeaderMap::new()) };
    assert_eq!(v, Verb::Ok, "the final-head slot stays open");
}

/// A gone client discards the unit once and the latch stays sticky: finalized plus cancelled.
#[test]
fn gone_client_discards_once_and_stays_discarded() {
    let Attached {
        state: mut st,
        reply: rx,
    } = state();
    drop(rx);
    let v = unsafe { write_body_core(&mut st, Piece::new(b"x").ptr(), false) };
    assert_eq!(v, Verb::Discarded);
    let v = unsafe { write_body_core(&mut st, Piece::new(b"y").ptr(), true) };
    assert_eq!(v, Verb::Discarded, "sticky across repeat writes");

    assert!(st.is_finalized());
    assert!(st.cancelled());
}

/// `flush()` emits the implicit 200 once; a repeat flush puts nothing new on the stream.
#[test]
fn flush_emits_the_implicit_head_once() {
    use rapira_sapi::types::Frame;
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    assert_eq!(unsafe { flush_core(&mut st) }, Verb::Ok);
    let Ok(Frame::Head {
        head,
        content_length,
        ..
    }) = rx.try_recv()
    else {
        panic!("flush must emit the head");
    };
    assert_eq!(head.status, 200);
    assert!(head.headers.is_empty(), "implicit 200 has no fields");
    assert_eq!(content_length, None, "flush costs the computed length");
    assert_eq!(unsafe { flush_core(&mut st) }, Verb::Ok);
    assert!(rx.try_recv().is_err(), "a repeat flush is a no-op");
}

/// A committed 101 is bodiless: chunks are accepted and dropped.
#[test]
fn a_101_head_drops_body_chunks() {
    use rapira_sapi::types::Frame;
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    assert_eq!(
        unsafe { write_head_core(&mut st, 101, HeaderMap::new()) },
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut st, Piece::new(b"upgrade").ptr(), true) };
    assert_eq!(v, Verb::Ok);
    let Ok(Frame::Head { bodiless, .. }) = rx.try_recv() else {
        panic!("head first");
    };
    assert!(bodiless);
    assert!(
        matches!(rx.try_recv(), Ok(Frame::End { .. })),
        "no chunk frames for a 1xx response"
    );
}

/// Trailers end the response on the End frame: a headless call is HeadNotWritten, a repeat call Finalized.
#[test]
fn trailers_finalize_with_a_committed_head() {
    use rapira_sapi::types::Frame;
    let Attached {
        state: mut st,
        reply: mut rx,
    } = state();
    let v = unsafe { write_trailers_core(&mut st, map(&[("x", "y")])) };
    assert_eq!(v, Verb::HeadNotWritten, "nothing here commits a head");

    assert_eq!(
        unsafe { write_head_core(&mut st, 200, HeaderMap::new()) },
        Verb::Ok
    );
    let v = unsafe { write_trailers_core(&mut st, map(&[("x", "y")])) };
    assert_eq!(v, Verb::Ok);
    let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
        panic!("head first");
    };
    assert_eq!(
        content_length,
        Some(0),
        "trailers-only keeps length framing"
    );
    let Ok(Frame::End {
        trailers,
        truncated,
    }) = rx.try_recv()
    else {
        panic!("the trailers ride the End frame");
    };
    assert!(!truncated);
    assert_eq!(trailers, map(&[("x", "y")]));

    let v = unsafe { write_trailers_core(&mut st, HeaderMap::new()) };
    assert_eq!(v, Verb::Finalized);
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

fn describe(frame: rapira_sapi::types::Frame) -> String {
    use rapira_sapi::types::Frame;
    match frame {
        Frame::Head {
            head,
            content_length,
            bodiless,
        } => format!(
            "head {} cl={content_length:?} bodiless={bodiless}",
            head.status
        ),
        Frame::Chunk(b) => format!("chunk {}", String::from_utf8_lossy(&b)),
        Frame::File { .. } => "file".into(),
        Frame::End { truncated, .. } => format!("end truncated={truncated}"),
    }
}

/// The frames of a lost unit, one whose object is gone before the response ended: a 500 head when none was sent, a truncated End after one was.
#[test]
fn a_lost_unit_fails_or_truncates_the_reply() {
    enum Before {
        Nothing,
        /// writeHead(200) with no body: the head is committed, not sent.
        HeadCommitted,
        /// A streamed chunk: the head is sent.
        HeadSent,
    }
    struct Case {
        name: &'static str,
        method: &'static str,
        before: Before,
        want: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "nothing written",
            method: "GET",
            before: Before::Nothing,
            want: &["head 500 cl=Some(0) bodiless=false", "end truncated=false"],
        },
        Case {
            name: "head committed, not sent: the 500 replaces it",
            method: "GET",
            before: Before::HeadCommitted,
            want: &["head 500 cl=Some(0) bodiless=false", "end truncated=false"],
        },
        Case {
            name: "head sent: the stream is cut",
            method: "GET",
            before: Before::HeadSent,
            want: &[
                "head 200 cl=None bodiless=false",
                "chunk abc",
                "end truncated=true",
            ],
        },
        Case {
            name: "HEAD request: a bodiless 500",
            method: "HEAD",
            before: Before::Nothing,
            want: &["head 500 cl=None bodiless=true", "end truncated=false"],
        },
    ];
    for case in cases {
        let mut req = base_req();
        req.method = case.method.into();
        let Attached {
            state: mut st,
            reply: mut rx,
        } = state_of(req);
        let before = match case.before {
            Before::Nothing => Verb::Ok,
            Before::HeadCommitted => unsafe { write_head_core(&mut st, 200, HeaderMap::new()) },
            Before::HeadSent => unsafe {
                write_body_core(&mut st, Piece::new(b"abc").ptr(), false)
            },
        };
        assert_eq!(before, Verb::Ok, "{}", case.name);
        st.lost();
        drop(st);
        let mut got = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            got.push(describe(frame));
        }
        assert_eq!(got, case.want, "{}", case.name);
    }
}

/// The frames of a shed unit, one the worker cannot serve: the 503 head and an End, bodiless for a HEAD request.
#[test]
fn a_shed_unit_gets_the_503_seal() {
    struct Case {
        name: &'static str,
        method: &'static str,
        want: &'static [&'static str],
    }
    let cases = [
        Case {
            name: "GET: a 503 with no body",
            method: "GET",
            want: &["head 503 cl=Some(0) bodiless=false", "end truncated=false"],
        },
        Case {
            name: "HEAD request: a bodiless 503",
            method: "HEAD",
            want: &["head 503 cl=None bodiless=true", "end truncated=false"],
        },
    ];
    for case in cases {
        let mut req = base_req();
        req.method = case.method.into();
        let Attached {
            state,
            reply: mut rx,
        } = state_of(req);
        Box::new(state).shed();
        let mut got = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            got.push(describe(frame));
        }
        assert_eq!(got, case.want, "{}", case.name);
    }
}
