fn main() -> anyhow::Result<()> {
    let php = rapira_php_build::discover()?;
    let sapi_include = std::env::var("DEP_RAPIRA_SAPI_INCLUDE")?;
    rapira_php_build::compile(
        "rapira_http_php",
        &[
            "rapira_http.c",
            "rapira_http_classes.c",
            "rapira_exchange.c",
        ],
        &php,
        &[&sapi_include],
    );
    rapira_php_build::rerun_if_changed(&[
        "rapira_http.c",
        "rapira_http_classes.c",
        "rapira_exchange.c",
        "rapira_http.h",
        "rapira_http.stub.php",
        "rapira_http_arginfo.h",
    ]);
    Ok(())
}
