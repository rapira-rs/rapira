use tests::php_lock;

/// The plugin classes extend the base ones, so the order base then parts must hold in MINIT.
#[test]
fn every_plugin_class_exists_after_boot_with_both_parts() {
    let _g = php_lock();
    let module = rapira_sapi::boot_master(&[rapira_http::PHP_PART, rapira_grpc::PHP_PART]).unwrap();
    for class in [
        "Rapira\\Work",
        "Rapira\\Http\\Exchange",
        "Rapira\\Internal\\Http\\Exchange",
        "Rapira\\Grpc\\UnaryCall",
        "Rapira\\Internal\\Grpc\\UnaryCall",
    ] {
        // SAFETY: between boot_master and the module drop.
        assert!(
            unsafe { rapira_sapi::class_exists(class) },
            "{class} missing"
        );
    }
    // SAFETY: as above.
    let extends =
        unsafe { rapira_sapi::class_extends("Rapira\\Internal\\Http\\Exchange", "Rapira\\Work") };
    assert!(
        extends,
        "Rapira\\Internal\\Http\\Exchange does not extend Rapira\\Work"
    );
    drop(module);
}

/// Each boot registers the parts it was given, not those of an earlier boot in the process.
#[test]
fn a_later_boot_registers_its_own_parts() {
    let _g = php_lock();
    let module = rapira_sapi::boot_master(&[rapira_http::PHP_PART]).unwrap();
    // SAFETY: between boot_master and the module drop.
    let before = unsafe { rapira_sapi::class_exists("Rapira\\Http\\Exchange") };
    drop(module);
    let module = rapira_sapi::boot_master(&[]).unwrap();
    // SAFETY: as above.
    let after = unsafe { rapira_sapi::class_exists("Rapira\\Http\\Exchange") };
    drop(module);
    assert!(
        before,
        "a boot with the http part did not register its classes"
    );
    assert!(
        !after,
        "a boot with no parts kept the http classes of the earlier boot"
    );
}
