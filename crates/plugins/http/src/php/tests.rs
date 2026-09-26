use super::headers::*;
use super::respond::*;
use super::*;
use rapira_sapi::types::Request;
use std::path::PathBuf;

fn base_req() -> Request {
    Request {
        method: String::new(),
        uri: "/".into(),
        target: None,
        authority: None,
        https: false,
        protocol: String::new(),
        remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
        server_name: String::new(),
        server_port: 8080,
        headers: HeaderMap::new(),
        content_type: None,
        content_length: 0,
        body: Body::Raw(std::io::Cursor::new(Vec::new())),
        received_at: None,
        tls: None,
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
    Truncated { status: Option<u16>, body: Vec<u8> },
    Nothing,
}

fn recv_sealed(rx: &mut tokio::sync::mpsc::Receiver<rapira_sapi::types::Frame>) -> Sealed {
    let (mut status, mut body, mut saw_frames) = (None, Vec::new(), false);
    while let Ok(frame) = rx.try_recv() {
        saw_frames = true;
        match frame {
            rapira_sapi::types::Frame::Interim(_) | rapira_sapi::types::Frame::File { .. } => {}
            rapira_sapi::types::Frame::Head { head, .. } => status = Some(head.status),
            rapira_sapi::types::Frame::Chunk(b) => body.extend_from_slice(&b),
            rapira_sapi::types::Frame::End { truncated, .. } => {
                return match (truncated, status) {
                    (true, status) => Sealed::Truncated { status, body },
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

/// Channel is sized for a full event trio so seal never parks with no reader.
fn state_of(
    req: Request,
) -> (
    ExchangeState,
    tokio::sync::mpsc::Receiver<rapira_sapi::types::Frame>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    (ExchangeState::new(req, tx), rx)
}

fn state() -> (
    ExchangeState,
    tokio::sync::mpsc::Receiver<rapira_sapi::types::Frame>,
) {
    state_of(base_req())
}

/// An unsealed overflow would leave the unit unfinalized and wedge every later receive() on the single-flight check.
#[test]
fn overflow_seals_the_unit_truncated() {
    let (mut st, mut rx) = state();
    let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), MAX_BUFFERED_BODY + 1, false) };
    assert_eq!(v, Verb::Overflow);

    let Sealed::Truncated { status, body } = recv_sealed(&mut rx) else {
        panic!("overflow must seal a truncated stream");
    };
    assert_eq!(status, Some(200));
    assert!(body.is_empty(), "the overflowing chunk is never sent");

    let v = unsafe { write_body_core(&mut st, c"y".as_ptr(), 1, true) };
    assert_eq!(v, Verb::Finalized);
    let job: *const c_void = (&raw const st).cast();
    assert!(unsafe { rapira_rs_exchange_is_finalized(job) });
}

/// A 304 head drops accepted body chunks at seal, like 204 and HEAD.
#[test]
fn seal_drops_the_body_for_304() {
    let (mut st, mut rx) = state();
    assert_eq!(
        unsafe { write_head_core(&mut st, 304, HeaderMap::new()) },
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut st, c"gone".as_ptr(), 4, true) };
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
    let (mut st, mut rx) = state();
    let v = unsafe { write_body_core(&mut st, c"".as_ptr(), 0, false) };
    assert_eq!(v, Verb::Ok);
    assert_eq!(
        unsafe { write_head_core(&mut st, 404, HeaderMap::new()) },
        Verb::Ok,
        "the head slot must still be open"
    );
    let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, true) };
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
    assert_eq!(protocol_php("HTTP/3.0"), "HTTP/3");
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
        req.server_name = "localhost".into();
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
    let (mut st, mut rx) = state();
    let v = unsafe { write_body_core(&mut st, c"abc".as_ptr(), 3, true) };
    assert_eq!(v, Verb::Ok);
    let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
        panic!("head first");
    };
    assert_eq!(content_length, Some(3));

    let (mut st, mut rx) = state();
    let v = unsafe { write_body_core(&mut st, c"abc".as_ptr(), 3, false) };
    assert_eq!(v, Verb::Ok);
    let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
        panic!("head first");
    };
    assert_eq!(content_length, None, "streaming: the plugin frames");
}

/// Over-declared length sends the fitting prefix and seals untruncated, so later writes see Finalized.
#[test]
fn content_length_exceeded_sends_the_prefix_and_seals() {
    use rapira_sapi::types::Frame;
    let (mut st, mut rx) = state();
    let v = unsafe { write_head_core(&mut st, 200, map(&[("content-length", "5")])) };
    assert_eq!(v, Verb::Ok);
    let v = unsafe { write_body_core(&mut st, c"0123456789".as_ptr(), 10, true) };
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

    let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, true) };
    assert_eq!(v, Verb::Finalized, "nothing written after it");
}

/// A repeated content-length in the head table is a `\ValueError`.
#[test]
fn repeated_content_length_is_a_bad_field() {
    let (mut st, _rx) = state();
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

/// An interim head emits at once with its fields as written (the plugin frames the wire) and leaves the final-head slot open.
#[test]
fn interim_head_emits_its_fields_and_leaves_the_final_head_open() {
    use rapira_sapi::types::Frame;
    let (mut st, mut rx) = state();
    let fields = map(&[
        ("link", "</a.css>; rel=preload"),
        ("content-length", "5"),
        ("connection", "close"),
    ]);
    let v = unsafe { write_head_core(&mut st, 103, fields.clone()) };
    assert_eq!(v, Verb::Ok);
    let Ok(Frame::Interim(head)) = rx.try_recv() else {
        panic!("interim head must be on the stream");
    };
    assert_eq!(head.status, 103);
    assert_eq!(head.headers, fields);
    let v = unsafe { write_head_core(&mut st, 200, HeaderMap::new()) };
    assert_eq!(v, Verb::Ok, "the final-head slot stays open");
}

/// A gone client discards the unit once and the latch stays sticky: finalized plus cancelled.
#[test]
fn gone_client_discards_once_and_stays_discarded() {
    let (mut st, rx) = state();
    drop(rx);
    let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, false) };
    assert_eq!(v, Verb::Discarded);
    let v = unsafe { write_body_core(&mut st, c"y".as_ptr(), 1, true) };
    assert_eq!(v, Verb::Discarded, "sticky across repeat writes");

    let job: *const c_void = (&raw const st).cast();
    assert!(unsafe { rapira_rs_exchange_is_finalized(job) });
    assert!(unsafe { rapira_rs_exchange_is_cancelled(job) });
}

/// `flush()` emits the implicit 200 once; a repeat flush puts nothing new on the stream.
#[test]
fn flush_emits_the_implicit_head_once() {
    use rapira_sapi::types::Frame;
    let (mut st, mut rx) = state();
    let job: *mut c_void = (&raw mut st).cast();
    assert!(unsafe { rapira_rs_exchange_flush(job) });
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
    assert!(unsafe { rapira_rs_exchange_flush(job) });
    assert!(rx.try_recv().is_err(), "a repeat flush is a no-op");
}

/// A committed 101 is bodiless: chunks are accepted and dropped.
#[test]
fn a_101_head_drops_body_chunks() {
    use rapira_sapi::types::Frame;
    let (mut st, mut rx) = state();
    assert_eq!(
        unsafe { write_head_core(&mut st, 101, HeaderMap::new()) },
        Verb::Ok
    );
    let v = unsafe { write_body_core(&mut st, c"upgrade".as_ptr(), 7, true) };
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
    let (mut st, mut rx) = state();
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

/// The forbidden set covers every RFC 9110 §6.5.1 category; unknown extension fields pass.
#[test]
fn trailer_denylist_matches_the_categories() {
    for name in [
        "Content-Length",
        "connection",
        "host",
        "authorization",
        "cache-control",
        "date",
        "content-type",
    ] {
        assert!(forbidden_trailer(name), "{name}");
    }
    assert!(!forbidden_trailer("x-checksum"));
    assert!(!forbidden_trailer("server-timing"));
}
