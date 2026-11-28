use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use http::{Extensions, StatusCode};
use http_body::Body as _;
use hyper::rt::{Read, ReadBuf, Write};
use rapira_http_middleware::{MiddlewareChain, ResponseHead};
use tokio::sync::mpsc;

use super::*;
use crate::responder::{Chunk, HttpResponder, ResponseSlot, Start, build_php_response};

fn poll_read_once<S: AsyncRead + Unpin>(
    io: &mut ShardIo<S>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<usize>> {
    let mut storage = [0u8; 64];
    let mut rb = ReadBuf::new(&mut storage);
    match Pin::new(io).poll_read(cx, rb.unfilled()) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
        Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn shard_io_keepalive_and_write_timeouts() {
    enum Scenario {
        ReadHeadTimesOut,
        InRequestExemptFromKeepalive,
        KeepaliveSlackDefersToExactDeadline,
        WriteStallTimesOutAndCancelsPending,
        FlushSuccessMarksDoneAndWriteErrCancels,
    }
    struct Case {
        name: &'static str,
        scenario: Scenario,
    }
    let cases = [
        Case {
            name: "idle ReadHead times out after keepalive_timeout",
            scenario: Scenario::ReadHeadTimesOut,
        },
        Case {
            name: "InRequest poll_read does not fire keepalive timeout",
            scenario: Scenario::InRequestExemptFromKeepalive,
        },
        Case {
            name: "busy keepalive within slack re-arms exact deadline without premature timeout",
            scenario: Scenario::KeepaliveSlackDefersToExactDeadline,
        },
        Case {
            name: "write stall past write_timeout returns TimedOut and cancels armed slot",
            scenario: Scenario::WriteStallTimesOutAndCancelsPending,
        },
        Case {
            name: "flush marks armed slot done while closed-peer write error cancels armed slot",
            scenario: Scenario::FlushSuccessMarksDoneAndWriteErrCancels,
        },
    ];

    let mut cx = Context::from_waker(Waker::noop());
    for case in cases {
        let phase = ConnPhase::new();
        let (server, peer) = tokio::io::duplex(16);
        let mut io = ShardIo::new(
            server,
            Rc::clone(&phase),
            Duration::from_secs(1),
            Duration::from_millis(200),
        );

        match case.scenario {
            Scenario::ReadHeadTimesOut => {
                let _peer = peer;
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
                tokio::time::advance(Duration::from_millis(999)).await;
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
                tokio::time::advance(Duration::from_millis(2)).await;
                match poll_read_once(&mut io, &mut cx) {
                    Poll::Ready(Err(e)) => {
                        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{}", case.name)
                    }
                    other => panic!("{}: expected TimedOut, got {other:?}", case.name),
                }
            }
            Scenario::InRequestExemptFromKeepalive => {
                let _peer = peer;
                phase.enter_request();
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
                tokio::time::advance(Duration::from_secs(5)).await;
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
            }
            Scenario::KeepaliveSlackDefersToExactDeadline => {
                let _peer = peer;
                // Arm initial timer at t=0 (deadline t=1000ms, slack=250ms).
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
                tokio::time::advance(Duration::from_millis(100)).await;
                phase.enter_request();
                phase.leave_request();
                // At t=100ms, target is t=1100ms (within 250ms slack of t=1000ms), so sleep keeps t=1000ms.
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
                // At t=1000ms, initial sleep expires; ShardIo must re-arm to exact t=1100ms and stay Pending.
                tokio::time::advance(Duration::from_millis(900)).await;
                assert!(
                    poll_read_once(&mut io, &mut cx).is_pending(),
                    "{}",
                    case.name
                );
                // At t=1100ms, exact deadline expires.
                tokio::time::advance(Duration::from_millis(100)).await;
                match poll_read_once(&mut io, &mut cx) {
                    Poll::Ready(Err(e)) => {
                        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{}", case.name)
                    }
                    other => panic!("{}: expected TimedOut, got {other:?}", case.name),
                }
            }
            Scenario::WriteStallTimesOutAndCancelsPending => {
                let _peer = peer;
                let slot = ResponseSlot::new();
                phase.arm_complete(Arc::clone(&slot));
                // Fill the 16-byte duplex buffer first.
                let first = Pin::new(&mut io).poll_write(&mut cx, &[b'a'; 16]);
                assert!(matches!(first, Poll::Ready(Ok(16))), "{}", case.name);
                // Next write stalls and arms write_timeout (200ms).
                assert!(
                    Pin::new(&mut io).poll_write(&mut cx, b"more").is_pending(),
                    "{}",
                    case.name
                );
                tokio::time::advance(Duration::from_millis(200)).await;
                match Pin::new(&mut io).poll_write(&mut cx, b"more") {
                    Poll::Ready(Err(e)) => {
                        assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{}", case.name)
                    }
                    other => panic!("{}: expected TimedOut, got {other:?}", case.name),
                }
                assert!(slot.is_cancelled(), "{}", case.name);
            }
            Scenario::FlushSuccessMarksDoneAndWriteErrCancels => {
                let slot_ok = ResponseSlot::new();
                phase.arm_complete(Arc::clone(&slot_ok));
                assert!(
                    matches!(Pin::new(&mut io).poll_flush(&mut cx), Poll::Ready(Ok(()))),
                    "{}",
                    case.name
                );
                slot_ok.cancel_if_incomplete();
                assert!(
                    !slot_ok.is_cancelled(),
                    "{}: flushed slot must be marked done",
                    case.name
                );

                // Drop peer so write errors with BrokenPipe and cancels the newly armed slot.
                drop(peer);
                let slot_err = ResponseSlot::new();
                phase.arm_complete(Arc::clone(&slot_err));
                assert!(
                    matches!(
                        Pin::new(&mut io).poll_write(&mut cx, b"x"),
                        Poll::Ready(Err(_))
                    ),
                    "{}",
                    case.name
                );
                assert!(slot_err.is_cancelled(), "{}", case.name);
            }
        }
    }
}

fn drain_track(mut body: TrackBody<ResponseBody>) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    let mut pendings = 0usize;
    loop {
        match Pin::new(&mut body).poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(_))) => {
                // Mirror hyper HTTP/1.1: stop polling once `is_end_stream()` flips true.
                if body.is_end_stream() {
                    return false;
                }
            }
            Poll::Ready(Some(Err(_))) => return true,
            Poll::Ready(None) => return false,
            Poll::Pending => {
                pendings += 1;
                assert!(pendings <= 4, "TrackBody stalled");
            }
        }
    }
}

#[test]
fn track_body_and_request_scope_lifecycle() {
    enum Kind {
        Empty,
        Once,
        StreamExactLen,
        StreamEndClean,
        StreamAbort,
        Refuse500,
        DropMidWaitStart,
    }
    struct Case {
        name: &'static str,
        kind: Kind,
        want_aborted: bool,
        want_armed_on_phase: bool,
        want_cancelled_without_flush: bool,
    }
    let cases = [
        Case {
            name: "Empty body arms ConnPhase at construction",
            kind: Kind::Empty,
            want_aborted: false,
            want_armed_on_phase: true,
            want_cancelled_without_flush: false,
        },
        Case {
            name: "Once body arms ConnPhase on final frame without extra poll",
            kind: Kind::Once,
            want_aborted: false,
            want_armed_on_phase: true,
            want_cancelled_without_flush: false,
        },
        Case {
            name: "Stream reaching declared_len arms ConnPhase on final frame",
            kind: Kind::StreamExactLen,
            want_aborted: false,
            want_armed_on_phase: true,
            want_cancelled_without_flush: false,
        },
        Case {
            name: "Stream ending on clean Chunk::End arms ConnPhase",
            kind: Kind::StreamEndClean,
            want_aborted: false,
            want_armed_on_phase: true,
            want_cancelled_without_flush: false,
        },
        Case {
            name: "Stream abort cancels slot immediately and never arms ConnPhase",
            kind: Kind::StreamAbort,
            want_aborted: true,
            want_armed_on_phase: false,
            want_cancelled_without_flush: true,
        },
        Case {
            name: "RequestScope::refuse marks slot done without arming ConnPhase",
            kind: Kind::Refuse500,
            want_aborted: false,
            want_armed_on_phase: false,
            want_cancelled_without_flush: false,
        },
        Case {
            name: "dropping RequestScope while awaiting Start closes receiver and cancels slot",
            kind: Kind::DropMidWaitStart,
            want_aborted: false,
            want_armed_on_phase: false,
            want_cancelled_without_flush: true,
        },
    ];

    let chain = MiddlewareChain::default();
    let ext = Extensions::new();
    for case in cases {
        let phase = ConnPhase::new();
        let inflight = Rc::new(LocalInflight::default());
        let (mut tx, slot) = HttpResponder::pair(false);
        let mut scope = RequestScope::new(Rc::clone(&phase), Rc::clone(&inflight));
        assert_eq!(inflight.local.get(), 1, "{}", case.name);
        assert_eq!(phase.current().0, IoPhase::InRequest, "{}", case.name);
        scope.attach_slot(Arc::clone(&slot));

        if matches!(case.kind, Kind::DropMidWaitStart) {
            drop(scope);
            assert_eq!(inflight.local.get(), 0, "{}", case.name);
            assert_eq!(phase.current().0, IoPhase::ReadHead, "{}", case.name);
            assert!(slot.is_cancelled(), "{}", case.name);
            let mut head = ResponseHead::new(200);
            head.known_len = Some(4);
            assert!(
                tx.write_complete(head, b"late", false).is_err(),
                "{}: receiver must be closed on mid-wait drop",
                case.name
            );
            continue;
        }

        let tracked = match case.kind {
            Kind::Empty => {
                let res = build_php_response(
                    &chain,
                    false,
                    false,
                    &ext,
                    Start::Complete {
                        head: ResponseHead::new(204),
                        body: Bytes::new(),
                        truncated: false,
                    },
                )
                .unwrap();
                scope.track(res)
            }
            Kind::Once => {
                let mut head = ResponseHead::new(200);
                head.known_len = Some(5);
                let res = build_php_response(
                    &chain,
                    false,
                    false,
                    &ext,
                    Start::Complete {
                        head,
                        body: Bytes::from_static(b"hello"),
                        truncated: false,
                    },
                )
                .unwrap();
                scope.track(res)
            }
            Kind::StreamExactLen | Kind::StreamEndClean | Kind::StreamAbort => {
                let (chunk_tx, chunk_rx) = mpsc::channel(4);
                chunk_tx
                    .try_send(Chunk::Data(Bytes::from_static(b"hello")))
                    .unwrap();
                let (cl, truncated) = match case.kind {
                    Kind::StreamExactLen => (Some(5), false),
                    Kind::StreamEndClean => (None, false),
                    Kind::StreamAbort => (None, true),
                    _ => unreachable!(),
                };
                chunk_tx.try_send(Chunk::End { truncated }).unwrap();
                let mut head = ResponseHead::new(200);
                head.known_len = cl;
                let res = build_php_response(
                    &chain,
                    false,
                    false,
                    &ext,
                    Start::Stream { head, rx: chunk_rx },
                )
                .unwrap();
                scope.track(res)
            }
            Kind::Refuse500 => scope.refuse(StatusCode::INTERNAL_SERVER_ERROR),
            Kind::DropMidWaitStart => unreachable!(),
        };

        assert_eq!(inflight.local.get(), 1, "{}", case.name);
        let aborted = drain_track(tracked.into_body());
        assert_eq!(aborted, case.want_aborted, "{}", case.name);
        assert_eq!(inflight.local.get(), 0, "{}", case.name);
        assert_eq!(phase.current().0, IoPhase::ReadHead, "{}", case.name);

        if case.want_armed_on_phase {
            assert!(
                !slot.is_cancelled(),
                "{}: armed slot waits for socket flush or error",
                case.name
            );
            phase.cancel_pending();
            assert!(
                slot.is_cancelled(),
                "{}: cancel_pending must cancel armed slot",
                case.name
            );
        } else {
            phase.on_flushed();
            phase.cancel_pending();
            assert_eq!(
                slot.is_cancelled(),
                case.want_cancelled_without_flush,
                "{}",
                case.name
            );
        }
    }
}
