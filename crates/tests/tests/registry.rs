use tests::php_lock;

/// The plugin classes extend the base ones, so the order base then parts must hold in MINIT.
#[test]
fn every_plugin_class_exists_after_boot_with_both_parts() {
    let _g = php_lock();
    let module =
        rapira_sapi::boot_master(&[rapira_sapi::http::PHP_PART, rapira_sapi::grpc::PHP_PART])
            .unwrap();
    for class in [
        "Rapira\\Work",
        "Rapira\\Http\\Exchange",
        "Rapira\\Internal\\Http\\Exchange",
        "Rapira\\Grpc\\UnaryCall",
        "Rapira\\Internal\\Grpc\\UnaryCall",
    ] {
        assert!(rapira_sapi::class_exists(class), "{class} missing");
    }
    drop(module);
}
