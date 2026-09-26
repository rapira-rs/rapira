use super::headers::*;
use super::respond::*;
use super::sendfile::*;
use super::*;
use crate::types::{Context, Request};
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

fn recv_sealed(rx: &mut tokio::sync::mpsc::Receiver<crate::types::Frame>) -> Sealed {
    let (mut status, mut body, mut saw_frames) = (None, Vec::new(), false);
    while let Ok(frame) = rx.try_recv() {
        saw_frames = true;
        match frame {
            crate::types::Frame::Interim(_) | crate::types::Frame::File { .. } => {}
            crate::types::Frame::Head { head, .. } => status = Some(head.status),
            crate::types::Frame::Chunk(b) => body.extend_from_slice(&b),
            crate::types::Frame::End { truncated, .. } => {
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
    tokio::sync::mpsc::Receiver<crate::types::Frame>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let job = Box::new(Job {
        ctx: Context::new(req, tx, /*superglobals=*/ false),
    });
    (ExchangeState::new(job), rx)
}

fn state() -> (
    ExchangeState,
    tokio::sync::mpsc::Receiver<crate::types::Frame>,
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

/// A one-shot write carries its computed length on the Head frame; a streamed write leaves framing to the front.
#[test]
fn head_frame_length_follows_the_write_shape() {
    use crate::types::Frame;
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
    assert_eq!(content_length, None, "streaming: the front frames");
}

/// Over-declared length sends the fitting prefix and seals untruncated, so later writes see Finalized.
#[test]
fn content_length_exceeded_sends_the_prefix_and_seals() {
    use crate::types::Frame;
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

/// An interim head emits at once with its fields as written (the front frames the wire) and leaves the final-head slot open.
#[test]
fn interim_head_emits_its_fields_and_leaves_the_final_head_open() {
    use crate::types::Frame;
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
    use crate::types::Frame;
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
    use crate::types::Frame;
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

/// sendFile validation, one test fn: the root is process-global state.
#[test]
fn send_file_validation_table() {
    use crate::types::Frame;
    let dir = std::env::temp_dir();
    set_sendfile_root(dir.clone());
    let path = dir.join(format!("rapira-sf-{}", std::process::id()));
    std::fs::write(&path, b"abcdefghijklmnopqrstuvwxyz").unwrap();
    let pb = path_bytes(&path);
    let link_out = dir.join(format!("rapira-sf-out-{}", std::process::id()));
    std::fs::remove_file(&link_out).ok();
    std::os::unix::fs::symlink("/etc/hosts", &link_out).unwrap();

    let (mut st, _rx) = state();
    for (name, path, offset, length) in [
        ("missing", b"/definitely/not/here".to_vec(), 0, None),
        ("directory", path_bytes(&dir), 0, None),
        ("offset past end", pb.clone(), 27, None),
        ("slice past end", pb.clone(), 20, Some(10)),
        ("outside the root", b"/etc/hosts".to_vec(), 0, None),
        ("escaping symlink", path_bytes(&link_out), 0, None),
    ] {
        let v = unsafe { send_file_core(&mut st, &path, offset, length, true) };
        assert!(matches!(v, Verb::FileNotSendable(_)), "{name}");
    }
    assert_eq!(st.stage, Stage::Open);

    let (mut st, mut rx) = state();
    let v = unsafe { send_file_core(&mut st, &pb, 2, Some(3), true) };
    assert_eq!(v, Verb::Ok);
    let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
        panic!("head first");
    };
    assert_eq!(
        content_length,
        Some(3),
        "the slice length is known up front"
    );
    let Ok(Frame::File { offset, len, .. }) = rx.try_recv() else {
        panic!("the file rides its own frame");
    };
    assert_eq!((offset, len), (2, 3));
    assert!(matches!(
        rx.try_recv(),
        Ok(Frame::End {
            truncated: false,
            ..
        })
    ));
    assert_eq!(st.stage, Stage::Finalized);

    let link_in = dir.join(format!("rapira-sf-in-{}", std::process::id()));
    std::fs::remove_file(&link_in).ok();
    std::os::unix::fs::symlink(&path, &link_in).unwrap();
    let (mut st, mut rx) = state();
    let v = unsafe { send_file_core(&mut st, &path_bytes(&link_in), 0, None, true) };
    assert_eq!(v, Verb::Ok, "intra-root symlinks stay sendable");
    assert!(matches!(rx.try_recv(), Ok(Frame::Head { .. })));

    std::fs::remove_file(&link_in).ok();
    std::fs::remove_file(&link_out).ok();
    std::fs::remove_file(&path).ok();
}

/// Trailers end the response on the End frame: a headless call is HeadNotWritten, a repeat call Finalized.
#[test]
fn trailers_finalize_with_a_committed_head() {
    use crate::types::Frame;
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

/// Sealing unlinks the spool files, so uploads are gone once the exchange finalizes.
#[test]
fn seal_unlinks_the_spool_files() {
    let (mut st, mut _rx) = state();
    let dir = std::env::temp_dir();
    let path = dir.join(format!("rapira-test-spool-{}", std::process::id()));
    std::fs::write(&path, b"payload").unwrap();
    st.body = BodyState::Multipart {
        fields: Vec::new(),
        files: vec![FilePart {
            upload: crate::types::UploadedFile {
                name: b"f".to_vec(),
                client_filename: b"a.bin".to_vec(),
                client_media_type: None,
                headers: Vec::new(),
                file: crate::types::SpooledFile { path: path.clone() },
                size: 7,
            },
            path: path_bytes(&path),
            headers: Grouped::new(&[]),
        }],
    };
    assert!(path.exists());
    unsafe { seal(&mut st, false, HeaderMap::new()) };
    assert!(!path.exists(), "seal must unlink the spooled file");
}
