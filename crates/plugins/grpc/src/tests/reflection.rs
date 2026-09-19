use super::*;

use std::sync::atomic::AtomicUsize;

use tokio::sync::mpsc;
use tonic::Code;
use tonic::codec::Codec;
use tonic_prost::ProstCodec;

const QUERY: &[u8] = b"\0\0\0\0\x02\x3a\0";
const OVERSIZED: &[u8] = b"\0\0\0\0\x0c\x0a\x08abcdefgh\x3a\0";
const INVALID_FLAG: &[u8] = b"\x02\0\0\0\x02\x3a\0";
const MALFORMED_PROTOBUF: &[u8] = b"\0\0\0\0\x02\x3a\xff";
const HEADER_ONLY: &[u8] = b"\0\0\0\0\x02";
const QUERY_THEN_HEADER: &[u8] = b"\0\0\0\0\x02\x3a\0\0\0\0\0\x02";

fn path(version: &str) -> String {
    format!("/grpc.reflection.{version}.ServerReflection/ServerReflectionInfo")
}

async fn outcome(response: HttpResponse) -> (usize, Code) {
    assert_eq!(response.status(), http::StatusCode::OK);
    assert!(!response.headers().contains_key("grpc-status"));
    let trailer_frames = Arc::new(AtomicUsize::new(0));
    let status_values = Arc::new(AtomicUsize::new(0));
    let frames = Arc::clone(&trailer_frames);
    let values = Arc::clone(&status_values);
    let body = response.into_body().map_frame(move |frame| {
        if let Some(trailers) = frame.trailers_ref() {
            frames.fetch_add(1, Ordering::Relaxed);
            values.fetch_add(
                trailers.get_all("grpc-status").iter().count(),
                Ordering::Relaxed,
            );
        }
        frame
    });
    let mut codec =
        ProstCodec::<pb::ServerReflectionRequest, pb::ServerReflectionResponse>::default();
    let mut stream =
        tonic::Streaming::new_response(codec.decoder(), body, http::StatusCode::OK, None, None);
    let mut replies = 0;
    let code = loop {
        match stream.message().await {
            Ok(Some(reply)) => {
                assert!(matches!(
                    reply.message_response,
                    Some(pb::server_reflection_response::MessageResponse::ListServicesResponse(_))
                ));
                replies += 1;
            }
            Ok(None) => break Code::Ok,
            Err(status) => break status.code(),
        }
    };
    assert!(
        stream.message().await.unwrap().is_none(),
        "terminal status must occur once"
    );
    assert_eq!(trailer_frames.load(Ordering::Relaxed), 1);
    assert_eq!(status_values.load(Ordering::Relaxed), 1);
    (replies, code)
}

#[tokio::test]
async fn request_decode_errors_reach_both_reflection_versions() {
    struct Case {
        name: &'static str,
        version: &'static str,
        invalid: &'static [u8],
        valid_prefix: bool,
        code: Code,
    }
    let cases = [
        Case {
            name: "v1_oversized",
            version: "v1",
            invalid: OVERSIZED,
            valid_prefix: false,
            code: Code::OutOfRange,
        },
        Case {
            name: "v1_invalid_flag",
            version: "v1",
            invalid: INVALID_FLAG,
            valid_prefix: false,
            code: Code::Internal,
        },
        Case {
            name: "v1_malformed_protobuf",
            version: "v1",
            invalid: MALFORMED_PROTOBUF,
            valid_prefix: false,
            code: Code::Internal,
        },
        Case {
            name: "v1_query_then_oversized",
            version: "v1",
            invalid: OVERSIZED,
            valid_prefix: true,
            code: Code::OutOfRange,
        },
        Case {
            name: "v1_query_then_invalid_flag",
            version: "v1",
            invalid: INVALID_FLAG,
            valid_prefix: true,
            code: Code::Internal,
        },
        Case {
            name: "v1_query_then_malformed_protobuf",
            version: "v1",
            invalid: MALFORMED_PROTOBUF,
            valid_prefix: true,
            code: Code::Internal,
        },
        Case {
            name: "v1alpha_oversized",
            version: "v1alpha",
            invalid: OVERSIZED,
            valid_prefix: false,
            code: Code::OutOfRange,
        },
        Case {
            name: "v1alpha_invalid_flag",
            version: "v1alpha",
            invalid: INVALID_FLAG,
            valid_prefix: false,
            code: Code::Internal,
        },
        Case {
            name: "v1alpha_malformed_protobuf",
            version: "v1alpha",
            invalid: MALFORMED_PROTOBUF,
            valid_prefix: false,
            code: Code::Internal,
        },
        Case {
            name: "v1alpha_query_then_oversized",
            version: "v1alpha",
            invalid: OVERSIZED,
            valid_prefix: true,
            code: Code::OutOfRange,
        },
        Case {
            name: "v1alpha_query_then_invalid_flag",
            version: "v1alpha",
            invalid: INVALID_FLAG,
            valid_prefix: true,
            code: Code::Internal,
        },
        Case {
            name: "v1alpha_query_then_malformed_protobuf",
            version: "v1alpha",
            invalid: MALFORMED_PROTOBUF,
            valid_prefix: true,
            code: Code::Internal,
        },
    ];
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let cfg = Config {
            max_request_message_size: 8,
            ..config()
        };
        let mut wire = Vec::new();
        if case.valid_prefix {
            wire.extend_from_slice(QUERY);
        }
        wire.extend_from_slice(case.invalid);
        wire.extend_from_slice(QUERY);
        let response = call(shared(cfg, &backend), request(&path(case.version), wire)).await;
        let (replies, code) = outcome(response).await;
        assert_eq!(replies, usize::from(case.valid_prefix), "{}", case.name);
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
        observed.push((case.name, code));
        expected.push((case.name, case.code));
    }
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn configured_response_errors_survive_input_error_observation() {
    struct Case {
        name: &'static str,
        version: &'static str,
        invalid_tail: bool,
    }
    let cases = [
        Case {
            name: "v1_response_limit",
            version: "v1",
            invalid_tail: false,
        },
        Case {
            name: "v1_response_limit_with_invalid_tail",
            version: "v1",
            invalid_tail: true,
        },
        Case {
            name: "v1alpha_response_limit",
            version: "v1alpha",
            invalid_tail: false,
        },
        Case {
            name: "v1alpha_response_limit_with_invalid_tail",
            version: "v1alpha",
            invalid_tail: true,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let cfg = Config {
            max_request_message_size: 8,
            max_response_message_size: 1,
            ..config()
        };
        let mut wire = QUERY.to_vec();
        if case.invalid_tail {
            wire.extend_from_slice(MALFORMED_PROTOBUF);
        }
        let response = call(shared(cfg, &backend), request(&path(case.version), wire)).await;
        assert_eq!(
            outcome(response).await,
            (0, Code::OutOfRange),
            "{}",
            case.name
        );
    }
}

struct WatchedInput {
    inner: extension_api::Body,
    waiting: Arc<Notify>,
    dropped: Arc<Notify>,
}

impl Body for WatchedInput {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(cx);
        if poll.is_pending() {
            self.waiting.notify_one();
        }
        poll
    }
}

impl Drop for WatchedInput {
    fn drop(&mut self) {
        self.dropped.notify_one();
    }
}

#[tokio::test]
async fn cancelled_reflection_responses_release_parked_request_readers() {
    struct Case {
        name: &'static str,
        version: &'static str,
        query: bool,
    }
    let cases = [
        Case {
            name: "v1_waiting_for_first_query",
            version: "v1",
            query: false,
        },
        Case {
            name: "v1_waiting_for_next_query",
            version: "v1",
            query: true,
        },
        Case {
            name: "v1alpha_waiting_for_first_query",
            version: "v1alpha",
            query: false,
        },
        Case {
            name: "v1alpha_waiting_for_next_query",
            version: "v1alpha",
            query: true,
        },
    ];
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let (send, receive) = mpsc::channel::<Result<Frame<Bytes>, BoxError>>(1);
        let waiting = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let input = WatchedInput {
            inner: StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receive))
                .boxed_unsync(),
            waiting: Arc::clone(&waiting),
            dropped: Arc::clone(&dropped),
        };
        let req = request(&path(case.version), b"".as_slice()).map(|_| input.boxed_unsync());
        let mut response = call(shared(config(), &backend), req).await;
        if case.query {
            send.send(Ok(Frame::data(Bytes::from_static(QUERY))))
                .await
                .unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(1), response.body_mut().frame())
                .await
                .expect(case.name)
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap();
            let message = pb::ServerReflectionResponse::decode(&reply[5..]).unwrap();
            assert!(
                matches!(
                    message.message_response,
                    Some(pb::server_reflection_response::MessageResponse::ListServicesResponse(_))
                ),
                "{}",
                case.name
            );
        }
        tokio::time::timeout(Duration::from_secs(1), waiting.notified())
            .await
            .expect(case.name);
        drop(response);
        let cancelled = tokio::time::timeout(Duration::from_secs(1), dropped.notified())
            .await
            .is_ok();
        drop(send);
        if !cancelled {
            tokio::time::timeout(Duration::from_secs(1), dropped.notified())
                .await
                .expect("closing input must release the reader");
        }
        observed.push((case.name, cancelled));
        expected.push((case.name, true));
    }
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn incomplete_envelopes_fail_at_eof_and_request_trailers() {
    struct Case {
        name: &'static str,
        version: &'static str,
        chunks: &'static [&'static [u8]],
        trailers: bool,
        prior_replies: usize,
    }
    let cases = [
        Case {
            name: "v1_eof_incomplete_prefix",
            version: "v1",
            chunks: &[b"\0", b"\0\0"],
            trailers: false,
            prior_replies: 0,
        },
        Case {
            name: "v1_eof_missing_payload",
            version: "v1",
            chunks: &[b"\0\0", b"\0\0\x02"],
            trailers: false,
            prior_replies: 0,
        },
        Case {
            name: "v1_eof_partial_payload",
            version: "v1",
            chunks: &[HEADER_ONLY, b"\x3a"],
            trailers: false,
            prior_replies: 0,
        },
        Case {
            name: "v1_eof_query_then_incomplete_prefix",
            version: "v1",
            chunks: &[QUERY, b"\0", b"\0"],
            trailers: false,
            prior_replies: 1,
        },
        Case {
            name: "v1_eof_query_then_missing_payload",
            version: "v1",
            chunks: &[QUERY_THEN_HEADER],
            trailers: false,
            prior_replies: 1,
        },
        Case {
            name: "v1_eof_query_then_partial_payload",
            version: "v1",
            chunks: &[b"\0\0\0\0\x02\x3a\0\0\0", b"\0\0\x02\x3a"],
            trailers: false,
            prior_replies: 1,
        },
        Case {
            name: "v1_trailers_incomplete_prefix",
            version: "v1",
            chunks: &[b"\0", b"\0"],
            trailers: true,
            prior_replies: 0,
        },
        Case {
            name: "v1_trailers_missing_payload",
            version: "v1",
            chunks: &[b"\0\0\0", b"\0\x02"],
            trailers: true,
            prior_replies: 0,
        },
        Case {
            name: "v1_trailers_partial_payload",
            version: "v1",
            chunks: &[HEADER_ONLY, b"\x3a"],
            trailers: true,
            prior_replies: 0,
        },
        Case {
            name: "v1_trailers_query_then_incomplete_prefix",
            version: "v1",
            chunks: &[QUERY, b"\0\0"],
            trailers: true,
            prior_replies: 1,
        },
        Case {
            name: "v1_trailers_query_then_missing_payload",
            version: "v1",
            chunks: &[QUERY_THEN_HEADER],
            trailers: true,
            prior_replies: 1,
        },
        Case {
            name: "v1_trailers_query_then_partial_payload",
            version: "v1",
            chunks: &[QUERY, b"\0\0", b"\0\0\x02", b"\x3a"],
            trailers: true,
            prior_replies: 1,
        },
        Case {
            name: "v1alpha_eof_incomplete_prefix",
            version: "v1alpha",
            chunks: &[b"\0", b"\0\0"],
            trailers: false,
            prior_replies: 0,
        },
        Case {
            name: "v1alpha_eof_missing_payload",
            version: "v1alpha",
            chunks: &[b"\0\0", b"\0\0\x02"],
            trailers: false,
            prior_replies: 0,
        },
        Case {
            name: "v1alpha_eof_partial_payload",
            version: "v1alpha",
            chunks: &[HEADER_ONLY, b"\x3a"],
            trailers: false,
            prior_replies: 0,
        },
        Case {
            name: "v1alpha_eof_query_then_incomplete_prefix",
            version: "v1alpha",
            chunks: &[QUERY, b"\0", b"\0"],
            trailers: false,
            prior_replies: 1,
        },
        Case {
            name: "v1alpha_eof_query_then_missing_payload",
            version: "v1alpha",
            chunks: &[QUERY_THEN_HEADER],
            trailers: false,
            prior_replies: 1,
        },
        Case {
            name: "v1alpha_eof_query_then_partial_payload",
            version: "v1alpha",
            chunks: &[b"\0\0\0\0\x02\x3a\0\0\0", b"\0\0\x02\x3a"],
            trailers: false,
            prior_replies: 1,
        },
        Case {
            name: "v1alpha_trailers_incomplete_prefix",
            version: "v1alpha",
            chunks: &[b"\0", b"\0"],
            trailers: true,
            prior_replies: 0,
        },
        Case {
            name: "v1alpha_trailers_missing_payload",
            version: "v1alpha",
            chunks: &[b"\0\0\0", b"\0\x02"],
            trailers: true,
            prior_replies: 0,
        },
        Case {
            name: "v1alpha_trailers_partial_payload",
            version: "v1alpha",
            chunks: &[HEADER_ONLY, b"\x3a"],
            trailers: true,
            prior_replies: 0,
        },
        Case {
            name: "v1alpha_trailers_query_then_incomplete_prefix",
            version: "v1alpha",
            chunks: &[QUERY, b"\0\0"],
            trailers: true,
            prior_replies: 1,
        },
        Case {
            name: "v1alpha_trailers_query_then_missing_payload",
            version: "v1alpha",
            chunks: &[QUERY_THEN_HEADER],
            trailers: true,
            prior_replies: 1,
        },
        Case {
            name: "v1alpha_trailers_query_then_partial_payload",
            version: "v1alpha",
            chunks: &[QUERY, b"\0\0", b"\0\0\x02", b"\x3a"],
            trailers: true,
            prior_replies: 1,
        },
    ];
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut frames: Vec<Result<Frame<Bytes>, BoxError>> = case
            .chunks
            .iter()
            .map(|chunk| Ok(Frame::data(Bytes::from_static(chunk))))
            .collect();
        if case.trailers {
            frames.push(Ok(Frame::trailers(HeaderMap::new())));
        }
        let req = request(&path(case.version), b"".as_slice())
            .map(|_| StreamBody::new(tokio_stream::iter(frames)).boxed_unsync());
        let (replies, code) = outcome(call(shared(config(), &backend), req).await).await;
        observed.push((case.name, replies, code));
        expected.push((case.name, case.prior_replies, Code::Internal));
        assert!(backend.seen.lock().unwrap().is_empty(), "{}", case.name);
    }
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn complete_streams_allow_zero_or_multiple_reflection_queries() {
    struct Case {
        name: &'static str,
        version: &'static str,
        chunks: &'static [&'static [u8]],
        trailers: bool,
        replies: usize,
        code: Code,
    }
    let cases = [
        Case {
            name: "v1_zero_messages",
            version: "v1",
            chunks: &[],
            trailers: false,
            replies: 0,
            code: Code::Ok,
        },
        Case {
            name: "v1_zero_messages_with_trailers",
            version: "v1",
            chunks: &[b""],
            trailers: true,
            replies: 0,
            code: Code::Ok,
        },
        Case {
            name: "v1_empty_protobuf_is_one_message",
            version: "v1",
            chunks: &[b"\0\0\0\0\0"],
            trailers: false,
            replies: 0,
            code: Code::InvalidArgument,
        },
        Case {
            name: "v1_two_queries_in_one_frame",
            version: "v1",
            chunks: &[b"\0\0\0\0\x02\x3a\0\0\0\0\0\x02\x3a\0"],
            trailers: false,
            replies: 2,
            code: Code::Ok,
        },
        Case {
            name: "v1_fragmented_queries_with_trailers",
            version: "v1",
            chunks: &[b"\0\0", b"\0\0\x02\x3a", b"\0", QUERY, b""],
            trailers: true,
            replies: 2,
            code: Code::Ok,
        },
        Case {
            name: "v1alpha_zero_messages",
            version: "v1alpha",
            chunks: &[],
            trailers: false,
            replies: 0,
            code: Code::Ok,
        },
        Case {
            name: "v1alpha_zero_messages_with_trailers",
            version: "v1alpha",
            chunks: &[b""],
            trailers: true,
            replies: 0,
            code: Code::Ok,
        },
        Case {
            name: "v1alpha_empty_protobuf_is_one_message",
            version: "v1alpha",
            chunks: &[b"\0\0\0\0\0"],
            trailers: false,
            replies: 0,
            code: Code::InvalidArgument,
        },
        Case {
            name: "v1alpha_two_queries_in_one_frame",
            version: "v1alpha",
            chunks: &[b"\0\0\0\0\x02\x3a\0\0\0\0\0\x02\x3a\0"],
            trailers: false,
            replies: 2,
            code: Code::Ok,
        },
        Case {
            name: "v1alpha_fragmented_queries_with_trailers",
            version: "v1alpha",
            chunks: &[b"\0\0", b"\0\0\x02\x3a", b"\0", QUERY, b""],
            trailers: true,
            replies: 2,
            code: Code::Ok,
        },
    ];
    for case in cases {
        let backend = Arc::new(TestBackend::default());
        let mut frames: Vec<Result<Frame<Bytes>, BoxError>> = case
            .chunks
            .iter()
            .map(|chunk| Ok(Frame::data(Bytes::from_static(chunk))))
            .collect();
        if case.trailers {
            frames.push(Ok(Frame::trailers(HeaderMap::new())));
        }
        let req = request(&path(case.version), b"".as_slice())
            .map(|_| StreamBody::new(tokio_stream::iter(frames)).boxed_unsync());
        assert_eq!(
            outcome(call(shared(config(), &backend), req).await).await,
            (case.replies, case.code),
            "{}",
            case.name
        );
    }
}
