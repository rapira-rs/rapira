use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    const HEADER: &str = "../sapi/rapira_sapi.h";
    println!("cargo:rustc-check-cfg=cfg(php_zts)");
    println!("cargo:rustc-check-cfg=cfg(php_execution_timers)");
    rapira_php_build::rerun_if_changed(&[HEADER]);

    let php = rapira_php_build::discover()?;
    let macros = cc::Build::new()
        .includes(&php.includes)
        .file(HEADER)
        .flag("-dM")
        .try_expand()?;
    let macros = str::from_utf8(&macros)?;
    match macros
        .lines()
        .find_map(|line| line.strip_prefix("#define PHP_ZTS "))
    {
        Some("1") => println!("cargo:rustc-cfg=php_zts"),
        Some("0") => (),
        _ => return Err("expected PHP_ZTS to be 0 or 1 in the PHP headers".into()),
    }
    // PHP defines the macro when it can arm a separate execution timer for each thread.
    let timers = ["#define", "ZEND_MAX_EXECUTION_TIMERS"];
    if macros
        .lines()
        .any(|line| line.split_whitespace().take(2).eq(timers))
    {
        println!("cargo:rustc-cfg=php_execution_timers");
    }
    Ok(())
}
