//! Generated messages of `grpc_fuzz/sample.proto` through the gRPC listener, with `grpc/wire-worker.php` as the application: it replies with the request message.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::Method;
use serde_json::{Map, Value, json};
use tests::fixture;
use tests::grpc::{Conn, Wire, envelope};

use crate::harness::{
    BOOT, Server, Spawn, calls, diagnostics, free_port, http_get, listen, worker_pids,
};

const SERVICE: &str = "rapira.test.fuzz.v1.FuzzService";
const ECHO: &str = "/rapira.test.fuzz.v1.FuzzService/Echo";
const RSS: &str = r#"rapira_worker_rss_bytes{pool="grpc",worker="0"}"#;
/// The largest RSS growth of the worker over all scenarios. The requests are below 1 KB each and below 200 KB in total, and a run grows the RSS by about 1 MB. 16 MiB is more than 80 times the total input, so only an allocation out of proportion to the input goes above it.
const RSS_GROWTH: u64 = 16 << 20;

/// SplitMix64, https://prng.di.unimi.it/splitmix64.c
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }

    fn coin(&mut self) -> bool {
        self.below(2) == 1
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// Canonical proto3 JSON over Connect. The reply is the same JSON.
    RoundTrip,
    /// A round-trip body with 1 to 3 byte edits. The reply is 200 or `invalid_argument`.
    Mutated,
    /// Random bytes as a Connect JSON body. The reply is `invalid_argument` and PHP gets no call. Almost all of these bodies fail the UTF-8 check before the JSON parser.
    RandomBytes,
    /// A binary message over gRPC on h2. The server sends the bytes to PHP without a decode, so the reply is the same bytes.
    Binary,
}

struct Scenario {
    name: &'static str,
    seed: u64,
    shape: Shape,
    count: usize,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "canonical json round trip",
        seed: 1,
        shape: Shape::RoundTrip,
        count: 160,
    },
    Scenario {
        name: "mutated json",
        seed: 2,
        shape: Shape::Mutated,
        count: 160,
    },
    Scenario {
        name: "random bytes as json",
        seed: 3,
        shape: Shape::RandomBytes,
        count: 64,
    },
    Scenario {
        name: "binary pass-through over grpc",
        seed: 4,
        shape: Shape::Binary,
        count: 64,
    },
];

/// 64 bits: an edge value, a small value of either sign, or a wide value. A cast to a 32-bit type keeps the low bits, so the 32-bit edges come from the same list.
fn bits(rng: &mut Rng) -> u64 {
    const EDGES: &[u64] = &[
        0,
        1,
        u64::MAX,
        1 << 31,
        (1 << 31) - 1,
        1 << 32,
        1 << 63,
        (1 << 63) - 1,
    ];
    match rng.below(4) {
        0 => rng.pick(EDGES),
        1 => rng.below(300) as u64,
        2 => (rng.below(300) as u64).wrapping_neg(),
        _ => rng.next() >> rng.below(64),
    }
}

/// A finite decimal with at most `digits` significant digits, so it round-trips through a binary type of that precision. The significand stays below 2^53 and the exponent within 22, so a JSON parser without correct rounding still parses each form of it exactly.
fn decimal(rng: &mut Rng, digits: u32) -> f64 {
    let m = rng.next() % 10u64.pow(digits);
    let e = rng.below(13) as i32 - 8;
    let sign = if rng.coin() { "-" } else { "" };
    format!("{sign}{m}e{e}").parse().expect("a decimal")
}

/// Characters that JSON escapes, characters outside ASCII, and plain ones.
const CHARS: &[char] = &[
    'a', 'Z', '0', ' ', '"', '\\', '/', '\n', '\t', '\0', '\u{1f}', '\u{7f}', 'é', '\u{2028}',
    '中', '\u{fffd}', '\u{ffff}', '😀',
];

fn text(rng: &mut Rng) -> String {
    (0..rng.below(9)).map(|_| rng.pick(CHARS)).collect()
}

fn bytes(rng: &mut Rng) -> Vec<u8> {
    (0..rng.below(17)).map(|_| rng.next() as u8).collect()
}

const KINDS: &[&str] = &["KIND_UNSPECIFIED", "KIND_ONE", "KIND_TWO", "KIND_NEGATIVE"];

#[derive(Clone, Copy)]
enum Int {
    I32,
    U32,
    I64,
    U64,
}

/// The integer fields of `Sample` by JSON name.
const INTS: &[(&str, Int)] = &[
    ("int32Value", Int::I32),
    ("int64Value", Int::I64),
    ("uint32Value", Int::U32),
    ("uint64Value", Int::U64),
    ("sint32Value", Int::I32),
    ("sint64Value", Int::I64),
    ("fixed32Value", Int::U32),
    ("fixed64Value", Int::U64),
    ("sfixed32Value", Int::I32),
    ("sfixed64Value", Int::I64),
];

/// Inserts a field with implicit presence: a set field at its default value is left out.
fn put(m: &mut Map<String, Value>, set: bool, default: bool, key: &str, value: Value) {
    if set && !default {
        m.insert(key.to_owned(), value);
    }
}

fn inner_json(rng: &mut Rng) -> Value {
    let mut m = Map::new();
    let id = bits(rng) as i32;
    put(&mut m, rng.coin(), id == 0, "id", json!(id));
    let label = text(rng);
    put(&mut m, rng.coin(), label.is_empty(), "label", json!(label));
    Value::Object(m)
}

/// A random `Sample` as canonical proto3 JSON, https://protobuf.dev/programming-guides/json/. Field names are lowerCamelCase. 64-bit integers are decimal strings, bytes are standard base64 with padding, enums are names, and map keys are strings. A field with explicit presence (a message, a oneof member, `optional`) is written when set, also at its default value.
fn sample_json(rng: &mut Rng) -> Value {
    let mut m = Map::new();
    for &(key, int) in INTS {
        let v = bits(rng);
        let (value, default) = match int {
            Int::I32 => (json!(v as i32), v as i32 == 0),
            Int::U32 => (json!(v as u32), v as u32 == 0),
            Int::I64 => (json!((v as i64).to_string()), v == 0),
            Int::U64 => (json!(v.to_string()), v == 0),
        };
        put(&mut m, rng.coin(), default, key, value);
    }
    for (key, digits) in [("floatValue", 6), ("doubleValue", 15)] {
        let (value, default) = if rng.below(8) == 0 {
            (json!(rng.pick(&["NaN", "Infinity", "-Infinity"])), false)
        } else {
            let v = decimal(rng, digits);
            (json!(v), v == 0.0)
        };
        put(&mut m, rng.coin(), default, key, value);
    }
    put(&mut m, rng.coin(), false, "boolValue", json!(true));
    let s = text(rng);
    put(&mut m, rng.coin(), s.is_empty(), "stringValue", json!(s));
    let b = bytes(rng);
    put(
        &mut m,
        rng.coin(),
        b.is_empty(),
        "bytesValue",
        json!(STANDARD.encode(&b)),
    );
    let kind = rng.pick(KINDS);
    put(&mut m, rng.coin(), kind == KINDS[0], "kind", json!(kind));
    if rng.coin() {
        m.insert("inner".to_owned(), inner_json(rng));
    }

    let n = rng.below(4);
    let ints: Vec<Value> = (0..n).map(|_| json!(bits(rng) as i32)).collect();
    let n = rng.below(4);
    let texts: Vec<Value> = (0..n).map(|_| json!(text(rng))).collect();
    let n = rng.below(4);
    let kinds: Vec<Value> = (0..n).map(|_| json!(rng.pick(KINDS))).collect();
    let n = rng.below(4);
    let inners: Vec<Value> = (0..n).map(|_| inner_json(rng)).collect();
    for (key, list) in [
        ("repeatedInt32", ints),
        ("repeatedString", texts),
        ("repeatedKind", kinds),
        ("repeatedInner", inners),
    ] {
        put(&mut m, true, list.is_empty(), key, Value::Array(list));
    }

    let mut to_int64 = Map::new();
    for _ in 0..rng.below(4) {
        to_int64.insert(text(rng), json!((bits(rng) as i64).to_string()));
    }
    let mut to_string = Map::new();
    for _ in 0..rng.below(4) {
        to_string.insert((bits(rng) as i32).to_string(), json!(text(rng)));
    }
    for (key, map) in [("stringToInt64", to_int64), ("int32ToString", to_string)] {
        put(&mut m, true, map.is_empty(), key, Value::Object(map));
    }

    match rng.below(4) {
        0 => {}
        1 => {
            m.insert("choiceText".to_owned(), json!(text(rng)));
        }
        2 => {
            m.insert("choiceInner".to_owned(), inner_json(rng));
        }
        _ => {
            m.insert(
                "choiceNumber".to_owned(),
                json!((bits(rng) as i64).to_string()),
            );
        }
    }
    if rng.coin() {
        m.insert("optionalInt32".to_owned(), json!(bits(rng) as i32));
    }
    Value::Object(m)
}

/// Numbers as f64, so a comparison does not depend on the integer or the float form of a JSON number. `floatValue` holds a 32-bit float, so it compares at that precision.
fn normalized(v: &Value) -> Value {
    match v {
        Value::Number(n) => json!(n.as_f64().expect("a JSON number")),
        Value::Array(list) => Value::Array(list.iter().map(normalized).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| match v {
                    Value::Number(n) if k == "floatValue" => {
                        (k.clone(), json!(n.as_f64().expect("a JSON number") as f32))
                    }
                    _ => (k.clone(), normalized(v)),
                })
                .collect(),
        ),
        _ => v.clone(),
    }
}

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A field key, https://protobuf.dev/programming-guides/encoding/#structure
fn key(out: &mut Vec<u8>, field: u64, wire_type: u64) {
    varint(out, field << 3 | wire_type);
}

fn len_field(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    key(out, field, 2);
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A negative int32 or enum value is a 10-byte varint of its sign extension.
fn int32(v: i32) -> u64 {
    i64::from(v) as u64
}

fn inner_binary(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    if rng.coin() {
        key(&mut out, 1, 0);
        varint(&mut out, int32(bits(rng) as i32));
    }
    if rng.coin() {
        len_field(&mut out, 2, text(rng).as_bytes());
    }
    out
}

/// A random `Sample` in the binary wire format, https://protobuf.dev/programming-guides/encoding/
fn sample_binary(rng: &mut Rng) -> Vec<u8> {
    let mut out = Vec::new();
    for field in 1..=27u64 {
        if !rng.coin() {
            continue;
        }
        let v = bits(rng);
        match field {
            1 | 16 | 27 => {
                key(&mut out, field, 0);
                varint(&mut out, int32(v as i32));
            }
            2..=4 | 13 => {
                key(&mut out, field, 0);
                varint(&mut out, if field == 3 { v as u32 as u64 } else { v });
            }
            5 => {
                let n = v as i32;
                key(&mut out, field, 0);
                varint(&mut out, u64::from(((n << 1) ^ (n >> 31)) as u32));
            }
            6 => {
                let n = v as i64;
                key(&mut out, field, 0);
                varint(&mut out, ((n << 1) ^ (n >> 63)) as u64);
            }
            7 | 9 | 11 => {
                key(&mut out, field, 5);
                out.extend_from_slice(&(v as u32).to_le_bytes());
            }
            8 | 10 | 12 => {
                key(&mut out, field, 1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            14 | 24 => len_field(&mut out, field, text(rng).as_bytes()),
            15 => len_field(&mut out, field, &bytes(rng)),
            17 | 25 => len_field(&mut out, field, &inner_binary(rng)),
            18 | 20 => {
                let mut packed = Vec::new();
                for _ in 0..rng.below(4) {
                    varint(&mut packed, int32(bits(rng) as i32));
                }
                len_field(&mut out, field, &packed);
            }
            19 => {
                for _ in 0..rng.below(4) {
                    len_field(&mut out, field, text(rng).as_bytes());
                }
            }
            21 => {
                for _ in 0..rng.below(4) {
                    len_field(&mut out, field, &inner_binary(rng));
                }
            }
            22 | 23 => {
                for _ in 0..rng.below(4) {
                    let mut entry = Vec::new();
                    if field == 22 {
                        len_field(&mut entry, 1, text(rng).as_bytes());
                        key(&mut entry, 2, 0);
                        varint(&mut entry, bits(rng));
                    } else {
                        key(&mut entry, 1, 0);
                        varint(&mut entry, int32(bits(rng) as i32));
                        len_field(&mut entry, 2, text(rng).as_bytes());
                    }
                    len_field(&mut out, field, &entry);
                }
            }
            _ => {
                // 26: choice_number, int64.
                key(&mut out, field, 0);
                varint(&mut out, v);
            }
        }
    }
    out
}

/// One to three byte inserts, replacements or removals.
fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(3) {
        let i = rng.below(bytes.len() + 1);
        match rng.below(3) {
            0 => bytes.insert(i, rng.next() as u8),
            _ if i == bytes.len() => {}
            1 => bytes[i] = rng.next() as u8,
            _ => {
                bytes.remove(i);
            }
        }
    }
}

/// The `code` of a Connect error body.
fn connect_code(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("code")?.as_str().map(str::to_owned)
}

/// Runs the cases of `s` and returns how many of them reached PHP.
async fn run(srv: &Server, s: &Scenario) -> anyhow::Result<usize> {
    let wire = match s.shape {
        Shape::Binary => Wire::H2,
        _ => Wire::Http1,
    };
    let mut conn = Conn::open(&listen(srv), wire).await?;
    let json = [("content-type", "application/json")];
    let mut rng = Rng(s.seed);
    let mut php = 0;
    for case in 0..s.count {
        let at = format!("{}, case {case}", s.name);
        match s.shape {
            Shape::RoundTrip => {
                let want = sample_json(&mut rng);
                let body = want.to_string();
                let got = conn
                    .send(Method::POST, ECHO, &json, body.as_bytes())
                    .await?;
                let reply = String::from_utf8_lossy(&got.body);
                assert_eq!(got.status, 200, "{at}: {body}\n{reply}");
                let got: Value = serde_json::from_slice(&got.body)?;
                assert_eq!(normalized(&got), normalized(&want), "{at}: {body}\n{reply}");
                php += 1;
            }
            Shape::Mutated => {
                let mut body = sample_json(&mut rng).to_string().into_bytes();
                mutate(&mut rng, &mut body);
                let got = conn.send(Method::POST, ECHO, &json, &body).await?;
                let at = format!("{at}: {}\n{got:?}", body.escape_ascii());
                match got.status {
                    200 => {
                        let reply: Value = serde_json::from_slice(&got.body)?;
                        assert!(reply.is_object(), "{at}");
                        php += 1;
                    }
                    400 => assert_eq!(
                        connect_code(&got.body).as_deref(),
                        Some("invalid_argument"),
                        "{at}"
                    ),
                    _ => panic!("{at}"),
                }
            }
            Shape::RandomBytes => {
                let body: Vec<u8> = (0..1 + rng.below(256)).map(|_| rng.next() as u8).collect();
                let got = conn.send(Method::POST, ECHO, &json, &body).await?;
                let at = format!("{at}: {}\n{got:?}", body.escape_ascii());
                assert_eq!(got.status, 400, "{at}");
                assert_eq!(
                    connect_code(&got.body).as_deref(),
                    Some("invalid_argument"),
                    "{at}"
                );
            }
            Shape::Binary => {
                let message = sample_binary(&mut rng);
                let frame = envelope(&message);
                let got = conn.grpc(ECHO, &[], &frame).await?;
                let at = format!("{at}: {}\n{got:?}", message.escape_ascii());
                assert_eq!(got.grpc_status(), Some("0"), "{at}");
                assert_eq!(got.body.as_ref(), frame.as_slice(), "{at}");
                php += 1;
            }
        }
    }
    Ok(php)
}

/// The RSS of the gRPC worker from the metrics endpoint. Only Linux reports it: the endpoint reads `/proc/<pid>/smaps_rollup`.
fn rss(srv: &Server, observability: SocketAddr) -> u64 {
    let (status, body) = http_get(observability, "/metrics", Duration::from_secs(10))
        .unwrap_or_else(|e| panic!("GET /metrics: {e}\n{}", diagnostics(srv)));
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 200, "{body}");
    *tests::metrics::samples(&body)
        .get(RSS)
        .unwrap_or_else(|| panic!("no series {RSS}\n{body}"))
}

/// Seeded messages of each field kind through the Connect JSON transcode and the binary gRPC path. Each scenario row sends `count` requests on one connection. Sources: the proto3 JSON mapping (https://protobuf.dev/programming-guides/json/), the binary encoding (https://protobuf.dev/programming-guides/encoding/), the Connect protocol (a bad request body is `invalid_argument`, HTTP 400) and PROTOCOL-HTTP2 (envelope, `grpc-status`).
///
/// After the scenarios the worker processes are the same, the RSS of the worker grew by less than [`RSS_GROWTH`] (Linux only), and PHP got only the calls that had a reply.
#[test]
fn generated_messages_cross_the_grpc_transcode() -> anyhow::Result<()> {
    let observability = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let srv = Spawn::grpc(fixture("grpc/wire-worker.php"))
        .descriptor_set(&fixture("grpc_fuzz/sample.binpb"))
        .services(Some(&[SERVICE]))
        .toml(&format!(
            "[observability]\nlisten = \"{observability}\"\n[observability.metrics]"
        ))
        .json_log()
        .spawn();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // The first call waits out the worker boot, so the RSS baseline holds a booted worker.
    let warm = rt.block_on(async {
        let mut conn = Conn::open(&listen(&srv), Wire::Http1).await?;
        let json = [("content-type", "application/json")];
        tokio::time::timeout(BOOT, conn.send(Method::POST, ECHO, &json, b"{}")).await?
    })?;
    assert_eq!(
        (warm.status, warm.body.as_ref()),
        (200, b"{}".as_slice()),
        "{}",
        diagnostics(&srv)
    );
    let pids = worker_pids(srv.pid());
    let before = cfg!(target_os = "linux").then(|| rss(&srv, observability));

    let mut php = 1;
    for s in SCENARIOS {
        php += rt.block_on(async { tokio::time::timeout(BOOT, run(&srv, s)).await })??;
    }

    assert_eq!(worker_pids(srv.pid()), pids, "{}", diagnostics(&srv));
    if let Some(before) = before {
        let after = rss(&srv, observability);
        assert!(
            after < before + RSS_GROWTH,
            "the worker RSS grew from {before} to {after} bytes"
        );
    }
    assert_eq!(calls(&srv).len(), php, "{}", diagnostics(&srv));
    Ok(())
}
