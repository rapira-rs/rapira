use rapira_sapi::Mode;
use tests::{fixture, server_log};

use crate::harness::{BOOT, Spawn, diagnostics, wait_log_contains};

// Expected values come from the Rapira\Grpc contract classes, and the Metadata messages from the userland body of its constructor.
const CASES: &[(&str, &str)] = &[
    (
        "metadata rejects an empty key",
        "ValueError: a metadata key must be non-empty lower-case ASCII",
    ),
    (
        "metadata rejects an upper-case key",
        "ValueError: a metadata key must be non-empty lower-case ASCII",
    ),
    (
        "metadata rejects a non-ASCII key",
        "ValueError: a metadata key must be non-empty lower-case ASCII",
    ),
    (
        "metadata rejects a non-ASCII text value",
        "ValueError: a value of metadata key x-a is not printable ASCII",
    ),
    (
        "metadata rejects a control byte in a text value",
        "ValueError: a value of metadata key x-a is not printable ASCII",
    ),
    ("metadata accepts an empty text value", "ok"),
    ("metadata keeps any bytes under a -bin key", "ok"),
    (
        "metadata rejects an entry that is not a list",
        "TypeError: metadata key x-a must map to a list of strings",
    ),
    (
        "metadata rejects a value that is not a string",
        "TypeError: metadata key x-a must map to a list of strings",
    ),
    (
        "metadata rejects a map of values",
        "TypeError: metadata key x-a must map to a list of strings",
    ),
    (
        "metadata checks the list before the values",
        "TypeError: metadata key x-a must map to a list of strings",
    ),
    (
        "metadata names an int key",
        "TypeError: metadata key 123 must map to a list of strings",
    ),
    (
        "metadata rejects a sparse list",
        "TypeError: metadata key x-a must map to a list of strings",
    ),
    ("values() is case-insensitive and ordered", r#"["1","2"]"#),
    ("values() of an absent key", "[]"),
    ("values() finds a numeric key", r#"["a"]"#),
    ("count() counts keys", "2"),
    ("getIterator() walks the entries", "x-a,x-b-bin"),
    (
        "method kind axes",
        "unary:00 server-streaming:01 client-streaming:10 bidi-streaming:11",
    ),
    ("grpc exception carries its status", "NotFound|nf|nf|0"),
    ("grpc exception subclass calls the parent", "Aborted"),
    (
        "context rejects a remote that is not an address",
        "TypeError",
    ),
    ("context keeps a null tls", "NULL"),
];

/// The fixture runs one PHP case per row and logs `case {name, result}`; a thrown case logs its class.
#[test]
fn value_types_follow_the_contract() {
    let mut srv = Spawn::http(Mode::Dispatcher, fixture("grpc/values.php"))
        .json_log()
        .spawn();
    // The worker runs the script once when it starts. The stop comes after the last case of the script, so the log holds every record.
    let last = "context keeps a null tls";
    assert!(
        wait_log_contains(&srv, last, BOOT),
        "no {last:?} case\n{}",
        diagnostics(&srv)
    );
    srv.stop();

    server_log::assert_case_records(&srv.log_file(), CASES);
}
