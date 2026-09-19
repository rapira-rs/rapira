use php_sys::{Mode, Rapira};
use serde_json::json;
use tests::{drain, fixture, php_lock, req};

#[test]
fn metadata_validates_and_copies_nested_references() -> anyhow::Result<()> {
    let _guard = php_lock();
    let path = "grpc/metadata.php";
    let rapira = Rapira::start(Mode::Dispatcher(fixture(path)))?;
    let handle = rapira.handle();
    let (status, body) = drain(handle.handle_blocking(req("/", path))?);
    drop(handle);
    rapira.shutdown();
    assert_eq!(status, 200, "{body}");
    let actual: serde_json::Value = serde_json::from_str(&body)?;
    assert_eq!(
        actual,
        json!({
            "snapshot": ["first", "second"],
            "binary": "00ff",
            "count": 2,
            "iteration": ["x-text", "x-data-bin"],
            "missing": [],
            "empty_name": "ValueError",
            "uppercase": "ValueError",
            "invalid_name": "ValueError",
            "non_ascii_name": "ValueError",
            "text_control": "ValueError",
            "text_non_ascii": "ValueError",
            "not_a_list": "TypeError",
            "not_a_string": "TypeError",
        "scalar_values": "TypeError",
        "uninitialized_count": "Error",
        "uninitialized_values": "Error",
        "uninitialized_iterator": "Error"
        })
    );
    Ok(())
}
