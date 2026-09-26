fn main() -> anyhow::Result<()> {
    let php = rapira_php_build::discover()?;
    let sapi_include = std::env::var("DEP_RAPIRA_SAPI_INCLUDE")?;
    rapira_php_build::compile(
        "rapira_grpc_php",
        &["rapira_grpc.c", "rapira_grpc_classes.c"],
        &php,
        &[&sapi_include],
    );
    rapira_php_build::rerun_if_changed(&[
        "rapira_grpc.c",
        "rapira_grpc_classes.c",
        "rapira_grpc.h",
        "rapira_grpc.stub.php",
        "rapira_grpc_arginfo.h",
    ]);
    Ok(())
}
