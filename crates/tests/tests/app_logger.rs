use tests::{app_record, app_records};
use tracing::Level;

/// Every LogLevel case reaches the matching tracing level; an omitted argument lands on Info.
#[test]
fn log_levels_map_onto_tracing_levels() {
    let (records, _) = app_records("app_logger/app-logger-levels.php");

    let got: Vec<(Level, &str)> = records
        .iter()
        .map(|(lvl, msg, _)| (*lvl, msg.as_str()))
        .collect();

    assert_eq!(
        got,
        vec![
            (Level::ERROR, "lvl-error"),
            (Level::WARN, "lvl-warning"),
            (Level::INFO, "lvl-info"),
            (Level::DEBUG, "lvl-debug"),
            (Level::TRACE, "lvl-trace"),
            (Level::INFO, "lvl-omitted"),
        ],
        "each case must map to its own level, in order"
    );
}

/// Absent and empty contexts carry no field at all rather than an empty JSON object.
#[test]
fn log_context_is_json_encoded() {
    let (records, _) = app_records("app_logger/app-logger-context.php");
    let find = |needle: &str| {
        records
            .iter()
            .find(|(_, msg, _)| msg == needle)
            .unwrap_or_else(|| panic!("no {needle:?} record in {records:?}"))
    };

    assert_eq!(find("ctx-absent").2, "", "an omitted context adds no field");
    assert_eq!(find("ctx-empty").2, "", "an empty array adds no field");

    let ctx = &find("ctx-full").2;
    for fragment in [
        r#""route":"\/orders""#,
        r#""tries":3"#,
        r#""ok":false"#,
        r#""nested":{"id":42}"#,
    ] {
        assert!(ctx.contains(fragment), "missing {fragment} in {ctx:?}");
    }
}

/// PSR-3's `['exception' => $e]` stays diagnosable: Throwable state lives in private properties.
#[test]
fn log_context_carries_a_throwable() {
    let (level, _, ctx) = app_record("app_logger/app-logger-exception.php");
    assert_eq!(level, Level::ERROR);

    for fragment in [
        "LogicException",
        "outer failure",
        "42",
        "app-logger-exception.php",
    ] {
        assert!(
            ctx.contains(fragment),
            "a logged exception must be diagnosable: no {fragment:?} in {ctx:?}"
        );
    }
    assert!(
        ctx.contains("inner cause"),
        "the previous exception must survive: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""order":"A-1""#),
        "sibling key lost: {ctx:?}"
    );
}

/// A context value json_encode cannot represent must not throw, drop the record, or lose siblings.
#[test]
fn log_context_tolerates_unencodable_values() {
    let (level, msg, ctx) = app_record("app_logger/app-logger-unencodable.php");

    assert_eq!(level, Level::ERROR);
    assert_eq!(msg, "hostile");
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "encodable neighbours must be intact: {ctx:?}"
    );
    for key in ["closure", "resource", "nan", "inf", "bytes", "pure_enum"] {
        assert!(
            ctx.contains(&format!("\"{key}\"")),
            "{key} must still appear rather than being dropped: {ctx:?}"
        );
    }
}

/// Common context values encode as expected and non-public properties never leak.
#[test]
fn log_context_encodes_common_php_values() {
    let (_, _, ctx) = app_record("app_logger/app-logger-values.php");

    for fragment in [
        r#""obj":{"id":"acc_1","note":null}"#,
        r#""money":{"cents":1250}"#,
        r#""suit":"H""#,
        r#""nothing":null"#,
        r#""list":[1,2,3]"#,
        r#""deep":{"a":{"b":{"c":"bottom"}}}"#,
        r#""zero":0"#,
    ] {
        assert!(ctx.contains(fragment), "missing {fragment} in {ctx:?}");
    }
    assert!(
        !ctx.contains("private") && !ctx.contains("protected"),
        "non-public properties must not leak: {ctx:?}"
    );
}

/// createFromDateString() DateIntervals encode as `from_string`/`date_string`, not y/m/d.
#[test]
fn app_logger_dateinterval_easy() {
    let (level, msg, ctx) = app_record("app_logger/app-logger-dateinterval.php");

    assert_eq!(level, Level::ERROR);
    assert_eq!(msg, "date-interval");
    assert_eq!(
        ctx.as_str(),
        concat!(
            r#"{"fromSpec":{"y":0,"m":1,"d":2,"h":0,"i":0,"s":0,"f":0,"#,
            r#""invert":0,"days":false,"from_string":false},"#,
            r#""fromDateString":{"from_string":true,"date_string":"1 month 2 days"}}"#,
        )
    );
}

/// Cycles are cut at the back-edge and raise no PHP diagnostic.
#[test]
fn cycles_are_broken_without_a_diagnostic() {
    let (records, phpdiag) = app_records("app_logger/limits-cycles.php");
    let [(level, _, ctx)] = records.as_slice() else {
        panic!("expected one app record, got {records:?}");
    };

    assert_eq!(*level, Level::ERROR);
    assert!(
        ctx.contains(r#""objects":{"bar":{"foo":null}}"#),
        "object cycle must be cut at the back-edge: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""arrays":{"x":{"foo":"bar","y":null}}"#),
        "reference cycle must be cut at the back-edge: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "siblings of a cycle must survive: {ctx:?}"
    );
    assert!(phpdiag.is_empty(), "cycles must raise nothing: {phpdiag:?}");
}

/// A throwing jsonSerialize() must not escape log(): the record still reaches the host.
#[test]
fn log_survives_a_throwing_json_serializer() {
    let (level, msg, ctx) = app_record("app_logger/app-logger-throwing-serializer.php");
    assert_eq!(level, Level::ERROR);
    assert_eq!(msg, "bombed");
    assert!(ctx.contains(r#""keep":"visible""#), "got: {ctx:?}");
    assert!(ctx.contains(r#""bomb":null"#), "got: {ctx:?}");
}

/// exit() inside a serializer is an unwind-exit, not a serialization failure: log() must not eat it.
#[test]
fn log_preserves_exit_from_a_serializer() {
    use rapira_sapi::{Mode, Rapira};
    use tests::{drain, fixture, init_log_capture, php_lock, req};

    let _guard = php_lock();
    init_log_capture();
    let r = Rapira::start(
        Mode::Classic,
        fixture("app_logger/app-logger-exit-in-serializer.php"),
        None,
    )
    .expect("classic boot");
    let h = r.sink();
    let (status, body) = drain(tests::submit(&h, req("/")).expect("dispatch"));
    drop(h);
    drop(r);

    assert_eq!(status, 200);
    assert!(body.contains("quitting"), "got: {body:?}");
    assert!(
        !body.contains("after-log"),
        "exit() must terminate the script, not be cleared (got: {body:?})"
    );
}
