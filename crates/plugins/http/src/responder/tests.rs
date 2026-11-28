use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use bytes::Bytes;
use http::header::{ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use http::{Extensions, HeaderMap, Method, StatusCode, Uri, Version};
use http_body::Body as _;
use rapira_http_middleware::{
    BodyEncoder, BodyStream, Middleware, MiddlewareChain, Peer, PeerAddr, Phases, RequestCtx,
    ResponseHead, ShortResponse,
};
use tokio::sync::mpsc;

use super::slot::Chunk;
use super::*;

struct CountWaker(AtomicUsize);

impl Wake for CountWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn count_waker() -> (Arc<CountWaker>, Waker) {
    let counter = Arc::new(CountWaker(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    (counter, waker)
}

fn drain_body(mut body: ResponseBody) -> (Vec<Bytes>, bool, usize) {
    let (_cnt, waker) = count_waker();
    let mut cx = Context::from_waker(&waker);
    let mut chunks = Vec::new();
    let mut pendings = 0usize;
    loop {
        match Pin::new(&mut body).poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    chunks.push(data);
                }
            }
            Poll::Ready(Some(Err(_))) => return (chunks, true, pendings),
            Poll::Ready(None) => return (chunks, false, pendings),
            Poll::Pending => {
                pendings += 1;
                assert!(pendings <= 4, "body stalled in drain_body");
            }
        }
    }
}

#[test]
fn response_slot_interleavings() {
    enum Action {
        SendBeforePoll,
        PollThenSend,
        PollTwiceWithNewWakerThenSend,
        CloseSenderBeforePoll,
        PollThenCloseSender,
        CloseReceiverBeforeSend,
        PollThenCloseReceiverBeforeSend,
        SendThenCloseReceiver,
    }
    struct Case {
        name: &'static str,
        action: Action,
        want_wakes: usize,
    }
    let cases = [
        Case {
            name: "send before poll resolves without waking",
            action: Action::SendBeforePoll,
            want_wakes: 0,
        },
        Case {
            name: "poll before send registers waker and wakes once",
            action: Action::PollThenSend,
            want_wakes: 1,
        },
        Case {
            name: "second poll replaces waker and wakes only the latest",
            action: Action::PollTwiceWithNewWakerThenSend,
            want_wakes: 1,
        },
        Case {
            name: "close sender before poll returns Err without waking",
            action: Action::CloseSenderBeforePoll,
            want_wakes: 0,
        },
        Case {
            name: "close sender after poll wakes receiver and returns Err",
            action: Action::PollThenCloseSender,
            want_wakes: 1,
        },
        Case {
            name: "close receiver before send returns Err to sender",
            action: Action::CloseReceiverBeforeSend,
            want_wakes: 0,
        },
        Case {
            name: "poll then drop receiver before send returns Err without waking",
            action: Action::PollThenCloseReceiverBeforeSend,
            want_wakes: 0,
        },
        Case {
            name: "send then drop receiver reclaims payload cleanly",
            action: Action::SendThenCloseReceiver,
            want_wakes: 0,
        },
    ];

    for case in cases {
        let (mut tx, slot) = HttpResponder::pair(false);
        let (w1_cnt, w1) = count_waker();
        let mut cx1 = Context::from_waker(&w1);
        match case.action {
            Action::SendBeforePoll => {
                tx.send_error(200);
                let res = slot.poll_recv(&mut cx1);
                assert!(
                    matches!(res, Poll::Ready(Ok(Start::Error(200)))),
                    "{}",
                    case.name
                );
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
            }
            Action::PollThenSend => {
                assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                tx.send_error(201);
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
                let res = slot.poll_recv(&mut cx1);
                assert!(
                    matches!(res, Poll::Ready(Ok(Start::Error(201)))),
                    "{}",
                    case.name
                );
            }
            Action::PollTwiceWithNewWakerThenSend => {
                assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                let (w2_cnt, w2) = count_waker();
                let mut cx2 = Context::from_waker(&w2);
                assert!(slot.poll_recv(&mut cx2).is_pending(), "{}", case.name);
                tx.send_error(202);
                assert_eq!(w1_cnt.0.load(Ordering::Relaxed), 0, "{}", case.name);
                assert_eq!(
                    w2_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
                let res = slot.poll_recv(&mut cx2);
                assert!(
                    matches!(res, Poll::Ready(Ok(Start::Error(202)))),
                    "{}",
                    case.name
                );
            }
            Action::CloseSenderBeforePoll => {
                drop(tx);
                let res = slot.poll_recv(&mut cx1);
                assert!(matches!(res, Poll::Ready(Err(()))), "{}", case.name);
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
            }
            Action::PollThenCloseSender => {
                assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                drop(tx);
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
                let res = slot.poll_recv(&mut cx1);
                assert!(matches!(res, Poll::Ready(Err(()))), "{}", case.name);
            }
            Action::CloseReceiverBeforeSend => {
                slot.close_receiver();
                assert!(tx.send_start(Start::Error(200)).is_err(), "{}", case.name);
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
            }
            Action::PollThenCloseReceiverBeforeSend => {
                assert!(slot.poll_recv(&mut cx1).is_pending(), "{}", case.name);
                slot.close_receiver();
                assert!(tx.send_start(Start::Error(200)).is_err(), "{}", case.name);
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
            }
            Action::SendThenCloseReceiver => {
                assert!(tx.send_start(Start::Error(200)).is_ok(), "{}", case.name);
                slot.close_receiver();
                assert_eq!(
                    w1_cnt.0.load(Ordering::Relaxed),
                    case.want_wakes,
                    "{}",
                    case.name
                );
            }
        }
    }
}

#[test]
fn response_slot_cancel_respects_mark_done() {
    struct Case {
        name: &'static str,
        mark_done_first: bool,
        want_cancelled: bool,
    }
    let cases = [
        Case {
            name: "cancel before mark_done sets cancelled",
            mark_done_first: false,
            want_cancelled: true,
        },
        Case {
            name: "cancel after mark_done does not set cancelled",
            mark_done_first: true,
            want_cancelled: false,
        },
    ];
    for case in cases {
        let slot = ResponseSlot::new();
        if case.mark_done_first {
            slot.mark_done();
        }
        slot.cancel_if_incomplete();
        assert_eq!(slot.is_cancelled(), case.want_cancelled, "{}", case.name);
    }
}

#[test]
fn response_slot_cross_thread_stress() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    for i in 0..1000u16 {
        let (mut tx, slot) = HttpResponder::pair(false);
        let status = 200 + (i % 100);
        let handle = std::thread::spawn(move || {
            tx.send_error(status);
        });
        let (w_cnt, waker) = count_waker();
        let mut cx = Context::from_waker(&waker);
        let got = loop {
            match slot.poll_recv(&mut cx) {
                Poll::Ready(res) => break res,
                Poll::Pending => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "response slot timed out on iteration {i}"
                    );
                    std::thread::yield_now();
                }
            }
        };
        handle.join().unwrap();
        assert!(matches!(got, Ok(Start::Error(code)) if code == status));
        assert!(w_cnt.0.load(Ordering::Relaxed) <= 1);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn compressed_declared_under_run_aborts() {
    struct Case {
        name: &'static str,
        stream: bool,
        declared_len: u64,
        body_len: usize,
        want_abort: bool,
    }
    let cases = [
        Case {
            name: "one-shot compressed body shorter than content-length aborts",
            stream: false,
            declared_len: 100,
            body_len: 50,
            want_abort: true,
        },
        Case {
            name: "streamed compressed body shorter than content-length aborts",
            stream: true,
            declared_len: 100,
            body_len: 50,
            want_abort: true,
        },
        Case {
            name: "one-shot compressed body matching content-length completes",
            stream: false,
            declared_len: 100,
            body_len: 100,
            want_abort: false,
        },
        Case {
            name: "streamed compressed body matching content-length completes",
            stream: true,
            declared_len: 100,
            body_len: 100,
            want_abort: false,
        },
    ];

    let compress = Arc::new(rapira_compress::Compress::new(
        rapira_compress::resolve(rapira_compress::Section {
            min_size: Some(16),
            ..Default::default()
        })
        .unwrap(),
    ));
    let chain = MiddlewareChain::new(vec![compress.clone() as Arc<dyn Middleware>]);

    let mut req_headers = HeaderMap::new();
    req_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
    let mut ctx = RequestCtx {
        method: Method::GET,
        uri: Uri::from_static("/"),
        version: Version::HTTP_11,
        headers: req_headers,
        peer: Peer {
            remote: PeerAddr::Inet(([127, 0, 0, 1], 40000).into()),
            server: PeerAddr::Inet(([127, 0, 0, 1], 8000).into()),
            https: false,
            received_at: 0.0,
        },
        extensions: Extensions::new(),
    };
    let _ = compress.on_request(&mut ctx);

    for case in cases {
        let mut head = ResponseHead::new(200);
        head.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        head.known_len = Some(case.declared_len);
        let start = if case.stream {
            let (tx, rx) = mpsc::channel(4);
            tx.try_send(Chunk::Data(Bytes::from(vec![b'x'; case.body_len])))
                .unwrap();
            tx.try_send(Chunk::End { truncated: false }).unwrap();
            Start::Stream { head, rx }
        } else {
            Start::Complete {
                head,
                body: Bytes::from(vec![b'x'; case.body_len]),
                truncated: false,
            }
        };
        let resp = build_php_response(&chain, false, false, &ctx.extensions, start)
            .await
            .unwrap();
        let (_chunks, aborted, _pendings) = drain_body(resp.into_body());
        assert_eq!(aborted, case.want_abort, "{}", case.name);
    }
}

struct VecStream(Vec<io::Result<Bytes>>);

impl BodyStream for VecStream {
    fn poll_next_chunk(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Bytes>>> {
        if self.0.is_empty() {
            Poll::Ready(None)
        } else {
            Poll::Ready(Some(self.0.remove(0)))
        }
    }
}

struct FailingEncoder {
    fail_write: bool,
    fail_finish: bool,
}

impl BodyEncoder for FailingEncoder {
    fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        if self.fail_write {
            return Err(io::Error::other("encode write boom"));
        }
        out.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(&mut self, _out: &mut Vec<u8>) -> io::Result<()> {
        if self.fail_finish {
            return Err(io::Error::other("encode finish boom"));
        }
        Ok(())
    }
}

struct FailingEncoderMw {
    fail_write: bool,
    fail_finish: bool,
}

impl Middleware for FailingEncoderMw {
    fn phases(&self) -> Phases {
        Phases::RESPONSE_HEAD
    }

    fn on_response_head(
        &self,
        _is_head: bool,
        _extensions: &Extensions,
        _head: &mut ResponseHead,
    ) -> Option<Box<dyn BodyEncoder>> {
        Some(Box::new(FailingEncoder {
            fail_write: self.fail_write,
            fail_finish: self.fail_finish,
        }))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unified_normalizer_and_stream_invariants() {
    enum SourceKind {
        ShortFull,
        ShortStreamUnderRun,
        PhpComplete,
        PhpTruncated,
        PhpStream,
    }
    struct Case {
        name: &'static str,
        kind: SourceKind,
        status: u16,
        is_head: bool,
        declared_len: Option<u64>,
        fail_write: bool,
        fail_finish: bool,
        want_err: Option<StatusCode>,
        want_status: StatusCode,
        want_cl: Option<&'static str>,
        want_body: &'static [u8],
        want_abort: bool,
        want_pending_before_abort: bool,
    }
    let cases = [
        Case {
            name: "short middleware response with status <200 normalizes to 502",
            kind: SourceKind::ShortFull,
            status: 103,
            is_head: false,
            declared_len: Some(5),
            fail_write: false,
            fail_finish: false,
            want_err: None,
            want_status: StatusCode::BAD_GATEWAY,
            want_cl: None,
            want_body: b"",
            want_abort: false,
            want_pending_before_abort: false,
        },
        Case {
            name: "HEAD response keeps known Content-Length without body on 200",
            kind: SourceKind::ShortFull,
            status: 200,
            is_head: true,
            declared_len: Some(42),
            fail_write: false,
            fail_finish: false,
            want_err: None,
            want_status: StatusCode::OK,
            want_cl: Some("42"),
            want_body: b"",
            want_abort: false,
            want_pending_before_abort: false,
        },
        Case {
            name: "HEAD 304 never emits Content-Length",
            kind: SourceKind::ShortFull,
            status: 304,
            is_head: true,
            declared_len: Some(42),
            fail_write: false,
            fail_finish: false,
            want_err: None,
            want_status: StatusCode::NOT_MODIFIED,
            want_cl: None,
            want_body: b"",
            want_abort: false,
            want_pending_before_abort: false,
        },
        Case {
            name: "middleware stream shorter than known_len yields pending before abort",
            kind: SourceKind::ShortStreamUnderRun,
            status: 200,
            is_head: false,
            declared_len: Some(10),
            fail_write: false,
            fail_finish: false,
            want_err: None,
            want_status: StatusCode::OK,
            want_cl: None,
            want_body: b"hello",
            want_abort: true,
            want_pending_before_abort: true,
        },
        Case {
            name: "one-shot encoder failure returns Err(500) refusal status",
            kind: SourceKind::PhpComplete,
            status: 200,
            is_head: false,
            declared_len: Some(5),
            fail_write: false,
            fail_finish: true,
            want_err: Some(StatusCode::INTERNAL_SERVER_ERROR),
            want_status: StatusCode::INTERNAL_SERVER_ERROR,
            want_cl: None,
            want_body: b"",
            want_abort: false,
            want_pending_before_abort: false,
        },
        Case {
            name: "truncated pre-head encoder write failure returns Err(500) refusal status",
            kind: SourceKind::PhpTruncated,
            status: 200,
            is_head: false,
            declared_len: None,
            fail_write: true,
            fail_finish: false,
            want_err: Some(StatusCode::INTERNAL_SERVER_ERROR),
            want_status: StatusCode::INTERNAL_SERVER_ERROR,
            want_cl: None,
            want_body: b"",
            want_abort: false,
            want_pending_before_abort: false,
        },
        Case {
            name: "stream finish encoder failure yields pending before abort",
            kind: SourceKind::PhpStream,
            status: 200,
            is_head: false,
            declared_len: Some(5),
            fail_write: false,
            fail_finish: true,
            want_err: None,
            want_status: StatusCode::OK,
            want_cl: None,
            want_body: b"",
            want_abort: true,
            want_pending_before_abort: true,
        },
    ];

    for case in cases {
        let chain = if case.fail_write || case.fail_finish {
            MiddlewareChain::new(vec![Arc::new(FailingEncoderMw {
                fail_write: case.fail_write,
                fail_finish: case.fail_finish,
            })])
        } else {
            MiddlewareChain::default()
        };
        let ext = Extensions::new();

        let res = match case.kind {
            SourceKind::ShortFull => {
                let mut head = ResponseHead::new(case.status);
                head.known_len = case.declared_len;
                build_short_response(
                    &chain,
                    0,
                    case.is_head,
                    false,
                    &ext,
                    ShortResponse::full(head, Bytes::from_static(b"hello")),
                )
                .await
            }
            SourceKind::ShortStreamUnderRun => {
                let mut head = ResponseHead::new(case.status);
                head.known_len = case.declared_len;
                let short = ShortResponse {
                    head,
                    body: Bytes::from_static(b"hel"),
                    rest: Some(Box::new(VecStream(vec![Ok(Bytes::from_static(b"lo"))]))),
                };
                build_short_response(&chain, 0, case.is_head, false, &ext, short).await
            }
            SourceKind::PhpComplete | SourceKind::PhpTruncated => {
                let mut head = ResponseHead::new(case.status);
                head.known_len = case.declared_len;
                build_php_response(
                    &chain,
                    case.is_head,
                    false,
                    &ext,
                    Start::Complete {
                        head,
                        body: Bytes::from_static(b"hello"),
                        truncated: matches!(case.kind, SourceKind::PhpTruncated),
                    },
                )
                .await
            }
            SourceKind::PhpStream => {
                let (tx, rx) = mpsc::channel(4);
                tx.try_send(Chunk::Data(Bytes::from_static(b"hello")))
                    .unwrap();
                tx.try_send(Chunk::End { truncated: false }).unwrap();
                let mut head = ResponseHead::new(case.status);
                head.known_len = case.declared_len;
                build_php_response(
                    &chain,
                    case.is_head,
                    false,
                    &ext,
                    Start::Stream { head, rx },
                )
                .await
            }
        };

        if let Some(want_err) = case.want_err {
            assert_eq!(res.err(), Some(want_err), "{}", case.name);
            continue;
        }
        let resp = res.expect(case.name);
        assert_eq!(resp.status(), case.want_status, "{}", case.name);
        assert_eq!(
            resp.headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            case.want_cl,
            "{}",
            case.name
        );

        let (chunks, aborted, pendings) = drain_body(resp.into_body());
        let body: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(body, case.want_body, "{}", case.name);
        assert_eq!(aborted, case.want_abort, "{}", case.name);
        if case.want_pending_before_abort {
            assert!(
                pendings >= 1,
                "{}: expected flush yield before abort",
                case.name
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn complete_response_encoding_offloads_large_bodies() {
    use std::sync::Mutex;
    use std::thread::ThreadId;

    struct ThreadRecordingEncoder {
        seen: Arc<Mutex<Option<ThreadId>>>,
    }

    impl BodyEncoder for ThreadRecordingEncoder {
        fn write(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
            *self.seen.lock().unwrap() = Some(std::thread::current().id());
            out.extend_from_slice(chunk);
            Ok(())
        }

        fn finish(&mut self, _out: &mut Vec<u8>) -> io::Result<()> {
            Ok(())
        }
    }

    struct ThreadRecordingMw {
        seen: Arc<Mutex<Option<ThreadId>>>,
    }

    impl Middleware for ThreadRecordingMw {
        fn phases(&self) -> Phases {
            Phases::RESPONSE_HEAD
        }

        fn on_response_head(
            &self,
            _is_head: bool,
            _extensions: &Extensions,
            _head: &mut ResponseHead,
        ) -> Option<Box<dyn BodyEncoder>> {
            Some(Box::new(ThreadRecordingEncoder {
                seen: Arc::clone(&self.seen),
            }))
        }
    }

    struct Case {
        name: &'static str,
        size: usize,
        want_offloaded: bool,
    }
    let cases = [
        Case {
            name: "body at inline threshold encodes on shard thread",
            size: INLINE_ENCODE_MAX,
            want_offloaded: false,
        },
        Case {
            name: "body above inline threshold encodes on blocking pool thread",
            size: INLINE_ENCODE_MAX + 1,
            want_offloaded: true,
        },
    ];

    let shard_tid = std::thread::current().id();
    for case in cases {
        let seen = Arc::new(Mutex::new(None));
        let chain = MiddlewareChain::new(vec![Arc::new(ThreadRecordingMw {
            seen: Arc::clone(&seen),
        })]);
        let payload = vec![b'a'; case.size];
        let resp = build_php_response(
            &chain,
            false,
            false,
            &Extensions::new(),
            Start::Complete {
                head: ResponseHead::new(200),
                body: Bytes::from(payload.clone()),
                truncated: false,
            },
        )
        .await
        .expect(case.name);
        let (chunks, aborted, _) = drain_body(resp.into_body());
        assert!(!aborted, "{}", case.name);
        let got: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        assert_eq!(got, payload, "{}", case.name);
        let ran_on = seen.lock().unwrap().expect("encoder ran");
        assert_eq!(ran_on != shard_tid, case.want_offloaded, "{}", case.name);
    }
}
