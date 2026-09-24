use std::collections::HashMap;

use php_sys::{Mode, Rapira};
use tests::{captured, fixture, init_log_capture, php_lock};

struct Case {
    name: &'static str,
    expected: &'static str,
}

// Expected values come from the Rapira\Grpc contract classes; a text metadata value must be printable ASCII (0x20-0x7E).
const CASES: &[Case] = &[
    Case {
        name: "metadata rejects an empty key",
        expected: "ValueError",
    },
    Case {
        name: "metadata rejects an upper-case key",
        expected: "ValueError",
    },
    Case {
        name: "metadata rejects a non-ASCII key",
        expected: "ValueError",
    },
    Case {
        name: "metadata rejects a non-ASCII text value",
        expected: "ValueError",
    },
    Case {
        name: "metadata rejects a control byte in a text value",
        expected: "ValueError",
    },
    Case {
        name: "metadata accepts an empty text value",
        expected: "ok",
    },
    Case {
        name: "metadata keeps any bytes under a -bin key",
        expected: "ok",
    },
    Case {
        name: "metadata rejects an entry that is not a list",
        expected: "TypeError",
    },
    Case {
        name: "metadata rejects a value that is not a string",
        expected: "TypeError",
    },
    Case {
        name: "values() is case-insensitive and ordered",
        expected: r#"["1","2"]"#,
    },
    Case {
        name: "values() of an absent key",
        expected: "[]",
    },
    Case {
        name: "values() finds a numeric key",
        expected: r#"["a"]"#,
    },
    Case {
        name: "count() counts keys",
        expected: "2",
    },
    Case {
        name: "getIterator() walks the entries",
        expected: "x-a,x-b-bin",
    },
    Case {
        name: "method kind axes",
        expected: "unary:00 server-streaming:01 client-streaming:10 bidi-streaming:11",
    },
    Case {
        name: "grpc exception carries its status",
        expected: "NotFound|nf|nf|0",
    },
    Case {
        name: "grpc exception subclass calls the parent",
        expected: "Aborted",
    },
    Case {
        name: "context rejects a remote that is not an address",
        expected: "TypeError",
    },
    Case {
        name: "context keeps a null tls",
        expected: "NULL",
    },
];

/// The fixture runs one PHP case per row and logs `case {name, result}`; a thrown case logs its class.
#[test]
fn value_types_follow_the_contract() -> anyhow::Result<()> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear();

    let r = Rapira::start(Mode::Dispatcher(fixture("grpc/values.php")))?;
    drop(r);

    let mut results = HashMap::new();
    for record in captured()
        .iter()
        .filter(|c| c.target == "app" && c.message == "case")
    {
        let ctx: serde_json::Value = serde_json::from_str(&record.context)?;
        let name = ctx["name"].as_str().unwrap_or_default().to_owned();
        let result = match &ctx["result"] {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        results.insert(name, result);
    }

    assert_eq!(
        results.len(),
        CASES.len(),
        "one record per case (got {results:?})"
    );
    let mismatches: Vec<String> = CASES
        .iter()
        .filter(|c| results.get(c.name).map(String::as_str) != Some(c.expected))
        .map(|c| {
            format!(
                "{}: expected {:?}, got {:?}",
                c.name,
                c.expected,
                results.get(c.name)
            )
        })
        .collect();
    assert!(mismatches.is_empty(), "{mismatches:#?}");
    Ok(())
}
