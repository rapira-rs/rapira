use std::time::Duration;

use connectrpc::ErrorCode;

use super::*;

fn base_call(deadline: Option<Deadline>) -> Call {
    Call {
        method: Arc::from("rapira.test.v1.EchoService/Echo"),
        protocol: Protocol::Grpc,
        metadata: HeaderMap::new(),
        deadline,
        remote: Addr::Inet(([127, 0, 0, 1], 50051).into()),
        message: Bytes::new(),
        received_at: 0.0,
    }
}

fn unary_state(reply: oneshot::Sender<UnaryReply>) -> CallState {
    CallState::new(base_call(None), Reply::Unary(reply))
}

fn stream_state(
    deadline: Option<Deadline>,
) -> (
    CallState,
    oneshot::Receiver<Result<Head, ConnectError>>,
    mpsc::Receiver<Part>,
) {
    let (head_tx, head_rx) = oneshot::channel();
    let (parts_tx, parts_rx) = mpsc::channel(4);
    let st = CallState::new(
        base_call(deadline),
        Reply::Stream(StreamReply::new(head_tx, parts_tx)),
    );
    (st, head_rx, parts_rx)
}

fn try_send_part(tx: &mpsc::Sender<Part>, part: Part, _until: Option<Instant>) -> bool {
    tx.try_send(part).is_ok()
}

/// `from_bytes` takes the obs-text bytes that a client can send.
fn map(lines: &[(&'static str, &'static str)]) -> HeaderMap {
    lines
        .iter()
        .map(|&(k, v)| {
            let value = HeaderValue::from_bytes(v.as_bytes()).expect("a valid field value");
            (HeaderName::from_static(k), value)
        })
        .collect()
}

fn owned(fields: &[(&str, &[&[u8]])]) -> Vec<MetadataField> {
    let mut out = Vec::new();
    for (name, values) in fields {
        out.push(MetadataField {
            name: HeaderName::from_bytes(name.as_bytes()).unwrap(),
            values: values.iter().map(|value| value.to_vec()).collect(),
        });
    }
    out
}

#[test]
fn reserved_names_cover_the_transport() {
    struct Case {
        name: &'static str,
        header: &'static str,
        reserved: bool,
    }
    const CASES: &[Case] = &[
        Case {
            name: "grpc- prefix",
            header: "grpc-timeout",
            reserved: true,
        },
        Case {
            name: "grpc- prefix with -bin",
            header: "grpc-status-details-bin",
            reserved: true,
        },
        Case {
            name: "content- prefix",
            header: "content-type",
            reserved: true,
        },
        Case {
            name: "connect- prefix",
            header: "connect-timeout-ms",
            reserved: true,
        },
        Case {
            name: "trailer- prefix",
            header: "trailer-x",
            reserved: true,
        },
        Case {
            name: "te",
            header: "te",
            reserved: true,
        },
        Case {
            name: "trailer",
            header: "trailer",
            reserved: true,
        },
        Case {
            name: "host",
            header: "host",
            reserved: true,
        },
        Case {
            name: "connection",
            header: "connection",
            reserved: true,
        },
        Case {
            name: "keep-alive",
            header: "keep-alive",
            reserved: true,
        },
        Case {
            name: "proxy-connection",
            header: "proxy-connection",
            reserved: true,
        },
        Case {
            name: "transfer-encoding",
            header: "transfer-encoding",
            reserved: true,
        },
        Case {
            name: "upgrade",
            header: "upgrade",
            reserved: true,
        },
        Case {
            name: "accept-encoding",
            header: "accept-encoding",
            reserved: true,
        },
        Case {
            name: "application name",
            header: "user-agent",
            reserved: false,
        },
        Case {
            name: "x- name",
            header: "x-user",
            reserved: false,
        },
        Case {
            name: "authorization",
            header: "authorization",
            reserved: false,
        },
        Case {
            name: "grpc without the dash",
            header: "grpcx",
            reserved: false,
        },
        Case {
            name: "x-grpc-web",
            header: "x-grpc-web",
            reserved: false,
        },
    ];
    for c in CASES {
        assert_eq!(reserved(c.header), c.reserved, "{}", c.name);
    }
}

#[test]
fn context_metadata_filters_and_decodes() {
    struct Case {
        name: &'static str,
        headers: &'static [(&'static str, &'static str)],
        expected: &'static [(&'static str, &'static [&'static [u8]])],
    }
    // Sources: PROTOCOL-HTTP2 Requests (padded and unpadded -bin values, split on ",", duplicates joined with ","), grpc-go decodeMetadataHeader (an empty -bin value is an empty value), RFC 9110 5.6.1 (whitespace around the ","), RFC 4648 ("AP8" is 00 ff, "AQI" is 01 02).
    const CASES: &[Case] = &[
        Case {
            name: "application key kept",
            headers: &[("user-agent", "t/1")],
            expected: &[("user-agent", &[b"t/1"])],
        },
        Case {
            name: "repeats keep order",
            headers: &[("x-user", "a"), ("x-user", "b")],
            expected: &[("x-user", &[b"a", b"b"])],
        },
        Case {
            name: "unpadded -bin",
            headers: &[("x-trace-bin", "AP8")],
            expected: &[("x-trace-bin", &[b"\x00\xff"])],
        },
        Case {
            name: "padded -bin",
            headers: &[("x-pad-bin", "AP8=")],
            expected: &[("x-pad-bin", &[b"\x00\xff"])],
        },
        Case {
            name: "undecodable -bin dropped",
            headers: &[("x-bad-bin", "!!")],
            expected: &[],
        },
        Case {
            name: "comma-separated -bin values split",
            headers: &[("x-trace-bin", "AP8,AQI")],
            expected: &[("x-trace-bin", &[b"\x00\xff", b"\x01\x02"])],
        },
        Case {
            name: "comma-separated -bin values with a space split",
            headers: &[("x-trace-bin", "AP8, AQI")],
            expected: &[("x-trace-bin", &[b"\x00\xff", b"\x01\x02"])],
        },
        Case {
            name: "empty -bin value kept",
            headers: &[("x-token-bin", "")],
            expected: &[("x-token-bin", &[b""])],
        },
        Case {
            name: "empty -bin value between commas kept",
            headers: &[("x-trace-bin", "AP8,,AQI")],
            expected: &[("x-trace-bin", &[b"\x00\xff", b"", b"\x01\x02"])],
        },
        Case {
            name: "non-ASCII text value dropped",
            headers: &[("x-user", "a"), ("x-user", "\u{e9}")],
            expected: &[("x-user", &[b"a"])],
        },
        Case {
            name: "text value with a tab dropped",
            headers: &[("x-user", "a\tb"), ("x-user", "c")],
            expected: &[("x-user", &[b"c"])],
        },
        Case {
            name: "key with only a non-printable value absent",
            headers: &[("x-user", "\u{e9}")],
            expected: &[],
        },
        Case {
            name: "reserved names dropped",
            headers: &[("grpc-timeout", "1S"), ("te", "trailers")],
            expected: &[],
        },
    ];
    for c in CASES {
        assert_eq!(
            context_metadata(&map(c.headers)),
            owned(c.expected),
            "{}",
            c.name
        );
    }
}

#[test]
fn wire_trims_text_values() {
    struct Case {
        name: &'static str,
        field: (&'static str, &'static [u8]),
        expected: &'static str,
    }
    // Sources: PROTOCOL-HTTP2 Requests ("ASCII-Value should not have leading or trailing whitespace. If it contains leading or trailing whitespace, it may be stripped."), RFC 9113 8.2.1 (a field value must not start or end with whitespace), RFC 4648 ("IA" is one space).
    const CASES: &[Case] = &[
        Case {
            name: "leading space stripped",
            field: ("x-cost", b"  12.30"),
            expected: "12.30",
        },
        Case {
            name: "trailing space stripped",
            field: ("x-note", b"done "),
            expected: "done",
        },
        Case {
            name: "inner space kept",
            field: ("x-note", b"a b"),
            expected: "a b",
        },
        Case {
            name: "only spaces become empty",
            field: ("x-note", b"   "),
            expected: "",
        },
        Case {
            name: "-bin value is encoded, not trimmed",
            field: ("x-b-bin", b" "),
            expected: "IA",
        },
    ];
    for c in CASES {
        let (name, value) = c.field;
        let fields = [MetadataField {
            name: HeaderName::from_static(name),
            values: vec![value.to_vec()],
        }];
        let map = wire(&fields);
        assert_eq!(
            map.get(name).map(HeaderValue::as_bytes),
            Some(c.expected.as_bytes()),
            "{}",
            c.name
        );
    }
}

#[test]
fn error_detail_normalizes_type_url_and_base64() {
    struct Case {
        name: &'static str,
        protocol: Protocol,
        type_url: &'static [u8],
        value: &'static [u8],
        want_url: &'static str,
        want_value: &'static str,
    }
    // RFC 4648: 0a 01 78 is "CgF4", 00 ff is "AP8" (unpadded).
    const CASES: &[Case] = &[
        Case {
            name: "grpc keeps full googleapis type URL",
            protocol: Protocol::Grpc,
            type_url: b"type.googleapis.com/google.rpc.ErrorInfo",
            value: b"\x0a\x01x",
            want_url: "type.googleapis.com/google.rpc.ErrorInfo",
            want_value: "CgF4",
        },
        Case {
            name: "grpc-web keeps full type URL",
            protocol: Protocol::GrpcWeb,
            type_url: b"type.googleapis.com/google.rpc.BadRequest",
            value: b"\x00\xff",
            want_url: "type.googleapis.com/google.rpc.BadRequest",
            want_value: "AP8",
        },
        Case {
            name: "connect strips prefix up to last slash",
            protocol: Protocol::Connect,
            type_url: b"type.googleapis.com/google.rpc.ErrorInfo",
            value: b"\x0a\x01x",
            want_url: "google.rpc.ErrorInfo",
            want_value: "CgF4",
        },
        Case {
            name: "connect keeps bare type name without slash",
            protocol: Protocol::Connect,
            type_url: b"google.rpc.ErrorInfo",
            value: b"",
            want_url: "google.rpc.ErrorInfo",
            want_value: "",
        },
    ];
    for c in CASES {
        let got = normalize_error_detail(c.protocol, c.type_url, c.value);
        assert_eq!(got.type_url, c.want_url, "{}", c.name);
        assert_eq!(got.value.as_deref(), Some(c.want_value), "{}", c.name);
        assert!(got.debug.is_none(), "{}", c.name);
    }
}

enum Setup {
    Open,
    Responded,
    HostClosed,
}

enum Want {
    Stored {
        name: &'static str,
        value: &'static [u8],
    },
    BadField(&'static CStr),
    Finalized,
}

#[test]
fn add_validates_per_contract() {
    struct Case {
        name: &'static str,
        setup: Setup,
        trailer: bool,
        binary: bool,
        key: &'static [u8],
        value: &'static [u8],
        want: Want,
    }
    // Sources: ResponseMetadata.php, PROTOCOL-HTTP2 Header-Name (0-9 a-z _ - .). A bad field is a ValueError before any state check.
    const CASES: &[Case] = &[
        Case {
            name: "text header lower-cases",
            setup: Setup::Open,
            trailer: false,
            binary: false,
            key: b"X-Cost",
            value: b"2.7",
            want: Want::Stored {
                name: "x-cost",
                value: b"2.7",
            },
        },
        Case {
            name: "binary header keeps raw bytes",
            setup: Setup::Open,
            trailer: false,
            binary: true,
            key: b"x-tok-bin",
            value: b"\x00\xff",
            want: Want::Stored {
                name: "x-tok-bin",
                value: b"\x00\xff",
            },
        },
        Case {
            name: "text trailer goes to the trailer half",
            setup: Setup::Open,
            trailer: true,
            binary: false,
            key: b"x-t",
            value: b"w",
            want: Want::Stored {
                name: "x-t",
                value: b"w",
            },
        },
        Case {
            name: "reserved name refused",
            setup: Setup::Open,
            trailer: false,
            binary: false,
            key: b"grpc-status",
            value: b"0",
            want: Want::BadField(c"the metadata name is reserved for the transport"),
        },
        Case {
            name: "text method refuses -bin",
            setup: Setup::Open,
            trailer: false,
            binary: false,
            key: b"x-tok-bin",
            value: b"a",
            want: Want::BadField(
                c"a metadata name with the -bin suffix takes binary values: use addBinaryHeader() or addBinaryTrailer()",
            ),
        },
        Case {
            name: "binary method needs -bin",
            setup: Setup::Open,
            trailer: true,
            binary: true,
            key: b"x-tok",
            value: b"a",
            want: Want::BadField(c"a binary metadata name must have the -bin suffix"),
        },
        Case {
            name: "non-ASCII text value",
            setup: Setup::Open,
            trailer: true,
            binary: false,
            key: b"x-a",
            value: "é".as_bytes(),
            want: Want::BadField(c"a text metadata value must be printable ASCII"),
        },
        Case {
            name: "control byte in text value",
            setup: Setup::Open,
            trailer: true,
            binary: false,
            key: b"x-a",
            value: b"a\nb",
            want: Want::BadField(c"a text metadata value must be printable ASCII"),
        },
        Case {
            name: "empty text value",
            setup: Setup::Open,
            trailer: false,
            binary: false,
            key: b"x-a",
            value: b"",
            want: Want::Stored {
                name: "x-a",
                value: b"",
            },
        },
        Case {
            name: "plus in the name",
            setup: Setup::Open,
            trailer: false,
            binary: false,
            key: b"x+debug",
            value: b"a",
            want: Want::BadField(c"a metadata name must use only 0-9 a-z _ - ."),
        },
        Case {
            name: "exclamation mark in the name",
            setup: Setup::Open,
            trailer: true,
            binary: false,
            key: b"x!a",
            value: b"a",
            want: Want::BadField(c"a metadata name must use only 0-9 a-z _ - ."),
        },
        Case {
            name: "empty name",
            setup: Setup::Open,
            trailer: false,
            binary: false,
            key: b"",
            value: b"a",
            want: Want::BadField(c"the metadata name is not a valid header name"),
        },
        Case {
            name: "bad name wins over finalized",
            setup: Setup::Responded,
            trailer: false,
            binary: false,
            key: b"grpc-x",
            value: b"a",
            want: Want::BadField(c"the metadata name is reserved for the transport"),
        },
        Case {
            name: "after finalize",
            setup: Setup::Responded,
            trailer: false,
            binary: false,
            key: b"x-a",
            value: b"a",
            want: Want::Finalized,
        },
        Case {
            name: "after host close",
            setup: Setup::HostClosed,
            trailer: true,
            binary: false,
            key: b"x-a",
            value: b"a",
            want: Want::Stored {
                name: "x-a",
                value: b"a",
            },
        },
    ];
    for c in CASES {
        let (tx, rx) = oneshot::channel();
        let mut st = unary_state(tx);
        let _rx = match c.setup {
            Setup::Responded => {
                assert_eq!(
                    st.finish(Ok(&[]), try_send_part),
                    (Verb::Ok, Some(Handled::Ok)),
                    "{}",
                    c.name
                );
                Some(rx)
            }
            Setup::HostClosed => {
                drop(rx);
                None
            }
            Setup::Open => Some(rx),
        };
        let v = st.add(c.trailer, c.binary, c.key, c.value);
        match c.want {
            Want::Stored { name, value } => {
                assert_eq!(v, Verb::Ok, "{}", c.name);
                let half = if c.trailer {
                    st.trailers()
                } else {
                    st.headers()
                };
                assert_eq!(half, owned(&[(name, &[value])]), "{}", c.name);
            }
            Want::BadField(msg) => assert_eq!(v, Verb::BadField(msg), "{}", c.name),
            Want::Finalized => assert_eq!(v, Verb::Finalized, "{}", c.name),
        }
    }
}

struct Status {
    code: ErrorCode,
    message: &'static str,
    details: &'static [Detail],
}

struct Detail {
    type_url: &'static str,
    /// Unpadded base64, as connectrpc carries it.
    value: &'static str,
}

enum Finish {
    Respond(&'static [u8]),
    Fail(Status),
}

enum Sent {
    /// The receiver gets this outcome: headers, trailers (wire form) and the result.
    Outcome {
        headers: &'static [(&'static str, &'static str)],
        trailers: &'static [(&'static str, &'static str)],
        result: Result<&'static [u8], Status>,
    },
    /// The test dropped the receiver.
    Nothing,
}

fn error(s: &Status) -> ConnectError {
    let mut err = ConnectError::new(s.code, s.message);
    for d in s.details {
        err.details.push(ErrorDetail {
            type_url: d.type_url.to_owned(),
            value: Some(d.value.to_owned()),
            debug: None,
        });
    }
    err
}

#[test]
fn finish_sends_one_outcome_in_wire_form() {
    struct Entry {
        trailer: bool,
        binary: bool,
        name: &'static [u8],
        value: &'static [u8],
    }
    struct Case {
        name: &'static str,
        adds: &'static [Entry],
        drop_receiver: bool,
        finishes: &'static [Finish],
        outcomes: &'static [(Verb, Option<Handled>)],
        settled: bool,
        cancelled: bool,
        sent: Sent,
    }
    // Sources: Responder.php and Work.php, PROTOCOL-HTTP2 Requests (unpadded -bin values), RFC 4648 (01 02 is "AQI", 0a 01 78 is "CgF4").
    const CASES: &[Case] = &[
        Case {
            name: "respond carries both halves",
            adds: &[
                Entry {
                    trailer: false,
                    binary: false,
                    name: b"x-a",
                    value: b"1",
                },
                Entry {
                    trailer: false,
                    binary: false,
                    name: b"x-a",
                    value: b"2",
                },
                Entry {
                    trailer: false,
                    binary: true,
                    name: b"x-b-bin",
                    value: b"\x01\x02",
                },
                Entry {
                    trailer: true,
                    binary: false,
                    name: b"x-t",
                    value: b"w",
                },
            ],
            drop_receiver: false,
            finishes: &[Finish::Respond(b"\x0a\x02hi")],
            outcomes: &[(Verb::Ok, Some(Handled::Ok))],
            settled: true,
            cancelled: false,
            sent: Sent::Outcome {
                headers: &[("x-a", "1"), ("x-a", "2"), ("x-b-bin", "AQI")],
                trailers: &[("x-t", "w")],
                result: Ok(b"\x0a\x02hi"),
            },
        },
        Case {
            name: "fail carries the status",
            adds: &[],
            drop_receiver: false,
            finishes: &[Finish::Fail(Status {
                code: ErrorCode::NotFound,
                message: "no invoice",
                details: &[Detail {
                    type_url: "type.googleapis.com/google.rpc.ErrorInfo",
                    value: "CgF4",
                }],
            })],
            outcomes: &[(Verb::Ok, Some(Handled::Ok))],
            settled: true,
            cancelled: false,
            sent: Sent::Outcome {
                headers: &[],
                trailers: &[],
                result: Err(Status {
                    code: ErrorCode::NotFound,
                    message: "no invoice",
                    details: &[Detail {
                        type_url: "type.googleapis.com/google.rpc.ErrorInfo",
                        value: "CgF4",
                    }],
                }),
            },
        },
        Case {
            name: "second finalize",
            adds: &[],
            drop_receiver: false,
            finishes: &[Finish::Respond(b"a"), Finish::Respond(b"b")],
            outcomes: &[(Verb::Ok, Some(Handled::Ok)), (Verb::Finalized, None)],
            settled: true,
            cancelled: false,
            sent: Sent::Outcome {
                headers: &[],
                trailers: &[],
                result: Ok(b"a"),
            },
        },
        Case {
            name: "receiver dropped before respond",
            adds: &[],
            drop_receiver: true,
            finishes: &[Finish::Respond(b"a")],
            outcomes: &[(Verb::Discarded, Some(Handled::Errored))],
            settled: true,
            cancelled: true,
            sent: Sent::Nothing,
        },
    ];
    for c in CASES {
        let (tx, rx) = oneshot::channel();
        let mut st = unary_state(tx);
        let mut rx = (!c.drop_receiver).then_some(rx);
        for e in c.adds {
            assert_eq!(
                st.add(e.trailer, e.binary, e.name, e.value),
                Verb::Ok,
                "{}",
                c.name
            );
        }
        let outcomes: Vec<(Verb, Option<Handled>)> = c
            .finishes
            .iter()
            .map(|f| match f {
                Finish::Respond(message) => st.finish(Ok(message), try_send_part),
                Finish::Fail(s) => st.finish(Err(error(s)), try_send_part),
            })
            .collect();
        assert_eq!(outcomes, c.outcomes, "{}", c.name);
        assert_eq!(st.settled(), c.settled, "{}: settled", c.name);
        assert_eq!(st.cancelled(), c.cancelled, "{}: cancelled", c.name);
        drop(st);

        match &c.sent {
            Sent::Outcome {
                headers,
                trailers,
                result,
            } => {
                let Some(got) = rx.as_mut().and_then(|rx| rx.try_recv().ok()) else {
                    panic!("{}: no outcome", c.name);
                };
                assert_eq!(got.headers, map(headers), "{}", c.name);
                assert_eq!(got.trailers, map(trailers), "{}", c.name);
                // ConnectError has no PartialEq, so the test compares its code, message and details.
                match (got.outcome, result) {
                    (Ok(message), Ok(want)) => assert_eq!(message, *want, "{}", c.name),
                    (Err(err), Err(want)) => {
                        assert_eq!(err.code, want.code, "{}", c.name);
                        assert_eq!(err.message.as_deref(), Some(want.message), "{}", c.name);
                        assert_eq!(err.details.len(), want.details.len(), "{}", c.name);
                        for (detail, want) in err.details.iter().zip(want.details) {
                            assert_eq!(detail.type_url, want.type_url, "{}", c.name);
                            assert_eq!(detail.value.as_deref(), Some(want.value), "{}", c.name);
                        }
                    }
                    (got, _) => panic!("{}: the other outcome kind: {got:?}", c.name),
                }
            }
            Sent::Nothing => {}
        }
    }
}

#[test]
fn stream_call_lifecycle() {
    enum Scenario {
        BeginChecksClosedAndDeadline,
        CommitHeadThenLateHeaderAndTrailerOnEnd,
        ZeroYieldCleanEnd,
        PreHeadFailSendsHeadersAndTrailers,
        PostHeadFailIncludesLateTrailers,
        SendFailureDiscardsCall,
    }
    struct Case {
        name: &'static str,
        scenario: Scenario,
    }
    const CASES: &[Case] = &[
        Case {
            name: "begin discards on closed parts or expired deadline and rejects finalized",
            scenario: Scenario::BeginChecksClosedAndDeadline,
        },
        Case {
            name: "first send commits head, blocks late header, omits late trailer on clean end",
            scenario: Scenario::CommitHeadThenLateHeaderAndTrailerOnEnd,
        },
        Case {
            name: "zero-yield stream commits head and sends Part::End",
            scenario: Scenario::ZeroYieldCleanEnd,
        },
        Case {
            name: "pre-head fail sends headers and trailers via head oneshot",
            scenario: Scenario::PreHeadFailSendsHeadersAndTrailers,
        },
        Case {
            name: "post-head fail sends Part::Fail with both early and late trailers",
            scenario: Scenario::PostHeadFailIncludesLateTrailers,
        },
        Case {
            name: "send failure discards stream call and reports errored once",
            scenario: Scenario::SendFailureDiscardsCall,
        },
    ];

    for c in CASES {
        match c.scenario {
            Scenario::BeginChecksClosedAndDeadline => {
                let (mut st, _head, parts) = stream_state(None);
                drop(parts);
                assert_eq!(
                    st.begin(),
                    (Verb::Discarded, Some(Handled::Errored)),
                    "{}",
                    c.name
                );
                assert!(st.settled(), "{}", c.name);

                let past = Deadline {
                    at: Instant::now() - Duration::from_millis(1),
                    unix: 0.0,
                };
                let (mut st, _head, _parts) = stream_state(Some(past));
                assert_eq!(
                    st.begin(),
                    (Verb::Discarded, Some(Handled::Errored)),
                    "{}",
                    c.name
                );

                let (mut st, _head, _parts) = stream_state(None);
                assert_eq!(st.end(try_send_part), Some(Handled::Ok), "{}", c.name);
                assert_eq!(st.begin(), (Verb::Finalized, None), "{}", c.name);
            }
            Scenario::CommitHeadThenLateHeaderAndTrailerOnEnd => {
                let (mut st, mut head_rx, mut parts_rx) = stream_state(None);
                assert_eq!(st.add(false, false, b"x-h", b"1"), Verb::Ok, "{}", c.name);
                assert_eq!(
                    st.add(true, false, b"x-t", b"early"),
                    Verb::Ok,
                    "{}",
                    c.name
                );
                assert_eq!(st.begin(), (Verb::Ok, None), "{}", c.name);
                assert_eq!(st.send(b"msg1", try_send_part), (true, None), "{}", c.name);

                // After first yield, headers are committed; late trailers are accepted but flagged.
                assert_eq!(
                    st.add(false, false, b"x-late-h", b"2"),
                    Verb::HeadersCommitted,
                    "{}",
                    c.name
                );
                assert_eq!(
                    st.add(true, false, b"x-late-t", b"late"),
                    Verb::Ok,
                    "{}",
                    c.name
                );
                assert_eq!(st.end(try_send_part), Some(Handled::Ok), "{}", c.name);
                assert!(st.settled(), "{}", c.name);

                let head = head_rx.try_recv().unwrap().unwrap();
                assert_eq!(head.headers, map(&[("x-h", "1")]), "{}", c.name);
                assert_eq!(head.trailers, map(&[("x-t", "early")]), "{}", c.name);
                assert!(
                    matches!(parts_rx.try_recv(), Ok(Part::Message(m)) if m.as_ref() == b"msg1"),
                    "{}",
                    c.name
                );
                assert!(matches!(parts_rx.try_recv(), Ok(Part::End)), "{}", c.name);
            }
            Scenario::ZeroYieldCleanEnd => {
                let (mut st, mut head_rx, mut parts_rx) = stream_state(None);
                assert_eq!(st.add(false, false, b"x-h", b"ok"), Verb::Ok, "{}", c.name);
                assert_eq!(st.begin(), (Verb::Ok, None), "{}", c.name);
                assert_eq!(st.end(try_send_part), Some(Handled::Ok), "{}", c.name);
                let head = head_rx.try_recv().unwrap().unwrap();
                assert_eq!(head.headers, map(&[("x-h", "ok")]), "{}", c.name);
                assert!(matches!(parts_rx.try_recv(), Ok(Part::End)), "{}", c.name);
            }
            Scenario::PreHeadFailSendsHeadersAndTrailers => {
                let (mut st, mut head_rx, _parts_rx) = stream_state(None);
                assert_eq!(st.add(false, false, b"x-h", b"a"), Verb::Ok, "{}", c.name);
                assert_eq!(st.add(true, false, b"x-t", b"b"), Verb::Ok, "{}", c.name);
                let err = ConnectError::new(ErrorCode::PermissionDenied, "denied");
                assert_eq!(
                    st.finish(Err(err), try_send_part),
                    (Verb::Ok, Some(Handled::Ok)),
                    "{}",
                    c.name
                );
                let got = head_rx.try_recv().unwrap().unwrap_err();
                assert_eq!(got.code, ErrorCode::PermissionDenied, "{}", c.name);
                assert_eq!(got.response_headers(), &map(&[("x-h", "a")]), "{}", c.name);
                assert_eq!(got.trailers(), &map(&[("x-t", "b")]), "{}", c.name);
            }
            Scenario::PostHeadFailIncludesLateTrailers => {
                let (mut st, mut head_rx, mut parts_rx) = stream_state(None);
                assert_eq!(
                    st.add(true, false, b"x-t1", b"early"),
                    Verb::Ok,
                    "{}",
                    c.name
                );
                assert_eq!(st.send(b"m1", try_send_part), (true, None), "{}", c.name);
                assert_eq!(
                    st.add(true, false, b"x-t2", b"late"),
                    Verb::Ok,
                    "{}",
                    c.name
                );
                let err = ConnectError::new(ErrorCode::Aborted, "mid-stream");
                assert_eq!(
                    st.finish(Err(err), try_send_part),
                    (Verb::Ok, Some(Handled::Ok)),
                    "{}",
                    c.name
                );
                let _ = head_rx.try_recv().unwrap().unwrap();
                assert!(
                    matches!(parts_rx.try_recv(), Ok(Part::Message(_))),
                    "{}",
                    c.name
                );
                match parts_rx.try_recv() {
                    Ok(Part::Fail(got)) => {
                        assert_eq!(got.code, ErrorCode::Aborted, "{}", c.name);
                        assert_eq!(
                            got.trailers(),
                            &map(&[("x-t1", "early"), ("x-t2", "late")]),
                            "{}",
                            c.name
                        );
                    }
                    other => panic!("{}: expected Part::Fail, got {other:?}", c.name),
                }
            }
            Scenario::SendFailureDiscardsCall => {
                let (mut st, _head_rx, _parts_rx) = stream_state(None);
                assert_eq!(
                    st.send(b"m1", |_, _, _| false),
                    (false, Some(Handled::Errored)),
                    "{}",
                    c.name
                );
                assert!(st.settled(), "{}", c.name);
                assert!(st.cancelled(), "{}", c.name);
                assert_eq!(st.discard(), None, "{}", c.name);
            }
        }
    }
}

#[test]
fn shed_and_discard_lifecycle() {
    enum Scenario {
        ShedUnary,
        ShedPreHeadStream,
        ShedPostHeadStream,
        DiscardIdempotent,
        DropUnsettledClosesReceiver,
    }
    struct Case {
        name: &'static str,
        scenario: Scenario,
    }
    const CASES: &[Case] = &[
        Case {
            name: "shed unary sends UNAVAILABLE",
            scenario: Scenario::ShedUnary,
        },
        Case {
            name: "shed pre-head stream sends UNAVAILABLE on head",
            scenario: Scenario::ShedPreHeadStream,
        },
        Case {
            name: "shed post-head stream drops cleanly without panicking",
            scenario: Scenario::ShedPostHeadStream,
        },
        Case {
            name: "discard settles once and is idempotent",
            scenario: Scenario::DiscardIdempotent,
        },
        Case {
            name: "dropping unsettled call closes receiver (lost call)",
            scenario: Scenario::DropUnsettledClosesReceiver,
        },
    ];

    for c in CASES {
        match c.scenario {
            Scenario::ShedUnary => {
                let (tx, mut rx) = oneshot::channel();
                unary_state(tx).shed();
                let reply = rx.try_recv().unwrap();
                let err = reply.outcome.unwrap_err();
                assert_eq!(err.code, ErrorCode::Unavailable, "{}", c.name);
            }
            Scenario::ShedPreHeadStream => {
                let (st, mut head_rx, _parts_rx) = stream_state(None);
                st.shed();
                let err = head_rx.try_recv().unwrap().unwrap_err();
                assert_eq!(err.code, ErrorCode::Unavailable, "{}", c.name);
            }
            Scenario::ShedPostHeadStream => {
                let (mut st, mut head_rx, mut parts_rx) = stream_state(None);
                assert_eq!(st.send(b"m1", try_send_part), (true, None), "{}", c.name);
                let _ = head_rx.try_recv().unwrap().unwrap();
                st.shed();
                assert!(
                    matches!(parts_rx.try_recv(), Ok(Part::Message(_))),
                    "{}",
                    c.name
                );
                assert!(
                    matches!(
                        parts_rx.try_recv(),
                        Err(mpsc::error::TryRecvError::Disconnected)
                    ),
                    "{}",
                    c.name
                );
            }
            Scenario::DiscardIdempotent => {
                let (tx, _rx) = oneshot::channel();
                let mut st = unary_state(tx);
                assert_eq!(st.discard(), Some(Handled::Errored), "{}", c.name);
                assert!(st.settled(), "{}", c.name);
                assert!(st.cancelled(), "{}", c.name);
                assert_eq!(st.discard(), None, "{}", c.name);
            }
            Scenario::DropUnsettledClosesReceiver => {
                let (tx, mut rx) = oneshot::channel();
                let st = unary_state(tx);
                drop(st);
                assert!(
                    matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
                    "{}",
                    c.name
                );
            }
        }
    }
}
