//! Transcodes Connect JSON to binary, back to JSON and to binary again, and compares the two decoded messages.

#![no_main]

use std::path::Path;
use std::sync::{Arc, LazyLock};

use buffa_descriptor::reflect::{ReflectMessageMut, Value};
use buffa_descriptor::{DescriptorPool, DynamicMessage, MessageIndex};
use rapira_fuzz::MAX_LEN;
use rapira_grpc::schema::Schema;

const SAMPLE_BINPB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/grpc/sample.binpb");
const ECHO: &str = "rapira.test.fuzz.v1.FuzzService/Echo";
// The field numbers of float_value and double_value in sample.proto.
const FLOAT: u32 = 11;
const DOUBLE: u32 = 12;

/// rapira's schema, and a second pool from the same file that decodes the messages for the compare.
struct Connect {
    schema: Schema,
    pool: Arc<DescriptorPool>,
    sample: MessageIndex,
}

static CONNECT: LazyLock<Connect> = LazyLock::new(|| {
    let path = Path::new(SAMPLE_BINPB);
    let schema = Schema::load(path, None).expect("loading sample.binpb");
    let bytes = std::fs::read(path).expect("reading sample.binpb");
    let pool = Arc::new(DescriptorPool::decode(&bytes).expect("decoding sample.binpb"));
    let sample = pool
        .message_index("rapira.test.fuzz.v1.Sample")
        .expect("the Sample message");
    Connect {
        schema,
        pool,
        sample,
    }
});

impl Connect {
    fn decode(&self, bytes: &[u8]) -> DynamicMessage {
        let opts = buffa::DecodeOptions::new().with_element_memory_limit(usize::MAX);
        DynamicMessage::decode_with_options(Arc::clone(&self.pool), self.sample, bytes, &opts)
            .expect("decoding a transcoded message")
    }
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
    let c = &*CONNECT;
    let m = c.schema.method(ECHO).expect("the Echo route");
    // #190: a valid float-form enum or int64 is rejected, so a rejected input is not compared, https://github.com/rapira-rs/rapira/issues/190
    let Ok(p1) = c.schema.json_to_proto(m, data) else {
        return;
    };
    let j1 = c
        .schema
        .proto_to_json(m, &p1)
        .unwrap_or_else(|e| panic!("proto_to_json of an accepted message: {e}"));
    let mut m1 = c.decode(&p1);
    let p2 = match c.schema.json_to_proto(m, &j1) {
        Ok(p2) => p2,
        // #177: FLT_MAX does not round trip, https://github.com/rapira-rs/rapira/issues/177
        Err(_) if near_f32_max(&m1) => return,
        Err(e) => panic!(
            "json_to_proto of its own output {}: {e}",
            String::from_utf8_lossy(&j1)
        ),
    };
    let mut m2 = c.decode(&p2);
    // #177: the JSON parse can move a float or a double by up to 2 ULP, https://github.com/rapira-rs/rapira/issues/177
    for (number, single) in [(FLOAT, true), (DOUBLE, false)] {
        let (a, b) = (float(&m1, number), float(&m2, number));
        assert!(ulps(a, b, single) <= 2, "field {number}: {a} -> {b}");
        let field = c
            .pool
            .message(c.sample)
            .field(number)
            .expect("a float field");
        m1.clear(field);
        m2.clear(field);
    }
    // A map compares as a sorted list of entries, so the entry order does not matter.
    assert_eq!(m1, m2, "the round trip changed the message");
});
