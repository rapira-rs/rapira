use php_sys::{Mode, Rapira};
use serde_json::{Value, json};
use tests::{captured, echo_services, fixture, init_log_capture, php_lock};

/// Runs one worker to its end and returns the context of its one `dispatcher` app record. The caller holds the PHP lock.
fn dispatcher_record(mode: Mode) -> anyhow::Result<Value> {
    init_log_capture();
    captured().clear();

    let r = Rapira::start(mode)?;
    drop(r);

    let all = captured();
    let records: Vec<&str> = all
        .iter()
        .filter(|c| c.target == "app" && c.message == "dispatcher")
        .map(|c| c.context.as_str())
        .collect();
    let php: Vec<&str> = all
        .iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message.as_str())
        .collect();
    assert_eq!(
        records.len(),
        1,
        "one dispatcher record (got {records:?}, php: {php:?})"
    );
    Ok(serde_json::from_str(records[0])?)
}

fn grpc_mode() -> Mode {
    Mode::GrpcDispatcher {
        script: fixture("grpc/identity.php"),
        services: echo_services(),
    }
}

/// Expected values come from the Rapira\Grpc\GrpcDispatcher contract and from echo.proto.
#[test]
fn dispatcher_identity_and_services() -> anyhow::Result<()> {
    let _guard = php_lock();
    let ctx = dispatcher_record(grpc_mode())?;

    let method = |name: &str, kind: &str| {
        json!({
            "class": "Rapira\\Grpc\\MethodInfo",
            "name": name,
            "inputType": "rapira.test.v1.EchoRequest",
            "outputType": "rapira.test.v1.EchoResponse",
            "kind": kind,
        })
    };
    assert_eq!(
        ctx,
        json!({
            "class": "Rapira\\Internal\\Grpc\\Dispatcher",
            "name": "grpc",
            "same": true,
            "grpc": true,
            "base": true,
            "clone": "blocked",
            "info": "Rapira\\Internal\\Grpc\\DispatcherInfo",
            "mode": "Dispatcher",
            "services": [{
                "class": "Rapira\\Grpc\\ServiceInfo",
                "name": "rapira.test.v1.EchoService",
                "methods": [
                    method("Echo", "unary"),
                    method("Get", "unary"),
                    method("Watch", "server-streaming"),
                ],
            }],
        })
    );
    Ok(())
}

/// The gRPC service list stays with its own worker: a later HTTP worker gets the HTTP dispatcher.
#[test]
fn an_http_worker_after_a_grpc_worker_keeps_the_http_dispatcher() -> anyhow::Result<()> {
    let _guard = php_lock();
    let grpc = dispatcher_record(grpc_mode())?;
    assert_eq!(grpc["name"], "grpc");

    let http = dispatcher_record(Mode::Dispatcher(fixture("dispatcher/worker-singleton.php")))?;
    assert_eq!(http["class"], "Rapira\\Internal\\Http\\Dispatcher");
    assert_eq!(http["name"], "http");
    Ok(())
}
