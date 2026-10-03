//! Transcodes Connect JSON to binary, back to JSON and to binary again, and compares the two decoded messages.

#![no_main]

use std::path::Path;
use std::sync::{Arc, LazyLock};

use buffa_descriptor::DynamicMessage;
use buffa_descriptor::reflect::{ReflectMessageMut, Value};
use rapira_fuzz::MAX_LEN;
use rapira_grpc::schema::{Method, Schema};

const SAMPLE_BINPB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/grpc/sample.binpb");
const ECHO: &str = "rapira.test.fuzz.v1.FuzzService/Echo";
// The field numbers of float_value and double_value in sample.proto.
const FLOAT: u32 = 11;
const DOUBLE: u32 = 12;

static SCHEMA: LazyLock<Schema> =
    LazyLock::new(|| Schema::load(Path::new(SAMPLE_BINPB), None).expect("loading sample.binpb"));

fn decode(s: &Schema, m: &Method, bytes: &[u8]) -> DynamicMessage {
    DynamicMessage::decode(Arc::clone(s.pool()), m.input, bytes)
        .expect("decoding a transcoded message")
}

/// The distance in units in the last place. Both zeros are 0 and two NaNs have no distance.
fn ulps(a: f64, b: f64, single: bool) -> u64 {
    if a.is_nan() && b.is_nan() {
        return 0;
    }
    let ord = |x: f64| -> i64 {
        if single {
            let b = (x as f32).to_bits() as i32;
            i64::from(if b < 0 { i32::MIN - b } else { b })
        } else {
            let b = x.to_bits() as i64;
            if b < 0 { i64::MIN - b } else { b }
        }
    };
    ord(a).abs_diff(ord(b))
}

/// A float or a double field. An absent field is 0.
fn float(msg: &DynamicMessage, number: u32) -> f64 {
    match msg.field_by_number(number) {
        None => 0.0,
        Some(Value::F32(f)) => f64::from(*f),
        Some(Value::F64(f)) => *f,
        Some(other) => panic!("float field {other:?}"),
    }
}

/// float_value is within 2 ULP of f32::MAX.
fn near_f32_max(msg: &DynamicMessage) -> bool {
    let f = float(msg, FLOAT).abs();
    f.is_finite() && ulps(f, f64::from(f32::MAX), true) <= 2
}

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    // #167: the transcode memory grows with the input, https://github.com/rapira-rs/rapira/issues/167
    if data.len() > MAX_LEN {
        return;
    }
    let c = &*SCHEMA;
    let m = c.method(ECHO).expect("the Echo route");
    // A rejected input has no round trip to compare.
    let Ok(p1) = c.json_to_proto(m, data) else {
        return;
    };
    let j1 = c
        .proto_to_json(m, &p1)
        .unwrap_or_else(|e| panic!("proto_to_json of an accepted message: {e}"));
    let mut m1 = decode(c, m, &p1);
    let p2 = match c.json_to_proto(m, &j1) {
        Ok(p2) => p2,
        // #177: FLT_MAX does not round trip, https://github.com/rapira-rs/rapira/issues/177
        Err(_) if near_f32_max(&m1) => return,
        Err(e) => panic!(
            "json_to_proto of its own output {}: {e}",
            String::from_utf8_lossy(&j1)
        ),
    };
    let mut m2 = decode(c, m, &p2);
    // #177: the JSON parse can move a float or a double by up to 2 ULP, https://github.com/rapira-rs/rapira/issues/177
    for (number, single) in [(FLOAT, true), (DOUBLE, false)] {
        let (a, b) = (float(&m1, number), float(&m2, number));
        assert!(ulps(a, b, single) <= 2, "field {number}: {a} -> {b}");
        let field = c
            .pool()
            .message(m.input)
            .field(number)
            .expect("a float field");
        m1.clear(field);
        m2.clear(field);
    }
    // A map compares as a sorted list of entries, so the entry order does not matter.
    assert_eq!(m1, m2, "the round trip changed the message");
});
