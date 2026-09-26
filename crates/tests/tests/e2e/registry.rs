use rapira_sapi::Mode;
use tests::wire::submit;
use tests::{drain, fixture, req};

use crate::harness::Spawn;

/// The plugin classes extend the base ones, so the order base then parts must hold in MINIT.
#[test]
fn every_plugin_class_exists_after_boot_with_both_parts() -> anyhow::Result<()> {
    let srv = Spawn::http(Mode::Worker, fixture("registry/classes.php")).spawn();
    let (status, body) = drain(submit(srv.addr, req("/"))?);
    assert_eq!(status, 200, "body: {body:?}");
    for class in [
        "Rapira\\Work",
        "Rapira\\Http\\Exchange",
        "Rapira\\Internal\\Http\\Exchange",
        "Rapira\\Grpc\\UnaryCall",
        "Rapira\\Internal\\Grpc\\UnaryCall",
    ] {
        assert!(
            body.contains(&format!("{class}: yes\n")),
            "{class} missing (got {body:?})"
        );
    }
    assert!(
        body.contains("Rapira\\Internal\\Http\\Exchange extends Rapira\\Work: yes\n"),
        "Rapira\\Internal\\Http\\Exchange does not extend Rapira\\Work (got {body:?})"
    );
    Ok(())
}
