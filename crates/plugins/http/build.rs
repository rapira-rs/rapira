fn main() -> Result<(), Box<dyn std::error::Error>> {
    let php = rapira_php_build::discover()?;
    rapira_php_build::compile(
        "rapira_http_php",
        &[
            "rapira_http.c",
            "rapira_http_classes.c",
            "rapira_exchange.c",
        ],
        &php,
    );
    rapira_php_build::rerun_if_changed(&[
        "rapira_http.h",
        "rapira_http_arginfo.h",
        "../../sapi/rapira_sapi.h",
    ]);
    Ok(())
}
