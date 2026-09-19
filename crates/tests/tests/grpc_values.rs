use php_sys::{Mode, Rapira};
use serde_json::json;
use tests::{drain, fixture, php_lock, req};

#[test]
fn status_and_service_values_validate_and_snapshot() -> anyhow::Result<()> {
    let _guard = php_lock();
    let path = "grpc/values-worker.php";
    let rapira = Rapira::start(Mode::Worker(fixture(path)))?;
    let handle = rapira.handle();
    let (status, body) = drain(handle.handle_blocking(req("/", path))?);
    drop(handle);
    rapira.shutdown();

    assert_eq!(status, 200, "{body}");
    let actual: serde_json::Value = serde_json::from_str(&body)?;
    let expected = json!({
        "rich_status": [3, "bad % input", "type.googleapis.com/example.Detail", "00ff"],
        "invalid_detail_type": "TypeError",
        "invalid_method_type": "TypeError",
        "exception_status": ["busy", 14, "0801"],
        "exception_constructor_message_precedence": ["", ""],
        "unary_axes": [false, false],
        "client_streaming_axes": [true, false],
        "server_streaming_axes": [false, true],
        "bidi_streaming_axes": [true, true],
        "service_methods_snapshot": ["Echo", "example.Message"],
    });
    assert_eq!(actual, expected);
    Ok(())
}
