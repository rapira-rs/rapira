fn main() -> Result<(), Box<dyn std::error::Error>> {
    let php = rapira_php_build::discover()?;
    rapira_php_build::compile(
        "rapira_grpc_php",
        &["rapira_grpc.c", "rapira_grpc_classes.c"],
        &php,
    );
    rapira_php_build::rerun_if_changed(&[
        "rapira_grpc.h",
        "rapira_grpc_arginfo.h",
        "../../sapi/rapira_sapi.h",
    ]);
    Ok(())
}
