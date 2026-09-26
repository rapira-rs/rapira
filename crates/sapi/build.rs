#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

const C_FILES: &[&str] = &[
    "wrapper.c",
    "module.c",
    "rapira_classes.c",
    "rapira_dispatcher.c",
    "rapira_http.c",
    "rapira_http_classes.c",
    "rapira_exchange.c",
    "rapira_grpc.c",
    "rapira_grpc_classes.c",
];

// bindgen panics on php-src master's `preserve_none` opcode handlers, so `_zend_op` stays opaque: https://clang.llvm.org/docs/AttributeReference.html#preserve-none
fn main() -> anyhow::Result<()> {
    println!("cargo:rustc-check-cfg=cfg(php84)");
    println!("cargo:rustc-check-cfg=cfg(php85)");

    let php = rapira_php_build::discover()?;

    for dir in &php.lib_dirs {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=dylib=php");

    if php.version >= (8, 5) {
        println!("cargo:rustc-cfg=php85");
    } else {
        println!("cargo:rustc-cfg=php84");
    }

    rapira_php_build::compile("rapira_sapi", C_FILES, &php, &[]);

    let mut bindings = bindgen::Builder::default()
        .header("bindgen.h")
        .clang_args(php.includes.iter().map(|d| format!("-I{d}")))
        .opaque_type("_zend_op");
    #[cfg(target_os = "macos")]
    {
        bindings = macos_sysroot(bindings);
    }

    for binding in ALLOWED_BINDINGS {
        bindings = bindings
            .allowlist_function(binding)
            .allowlist_type(binding)
            .allowlist_var(binding);
    }

    bindings
        .generate()?
        .write_to_file(PathBuf::from(env::var("OUT_DIR")?).join("bindings.rs"))?;

    // A plugin's build script reads it as DEP_RAPIRA_SAPI_INCLUDE and finds rapira_sapi.h there.
    println!("cargo:include={}", env::var("CARGO_MANIFEST_DIR")?);

    let inputs: &[&str] = &[
        "bindgen.h",
        "rapira_sapi.h",
        "rapira_http.h",
        "rapira_grpc.h",
        "allowed_bindings.rs",
        "rapira.stub.php",
        "rapira_arginfo.h",
        "rapira_exception.stub.php",
        "rapira_exception_arginfo.h",
        "rapira_http.stub.php",
        "rapira_http_arginfo.h",
        "rapira_grpc.stub.php",
        "rapira_grpc_arginfo.h",
    ];
    rapira_php_build::rerun_if_changed(&[C_FILES, inputs].concat());

    Ok(())
}

// libclang 19+ does not infer the macOS SDK path: without -isysroot the parse cannot find <stdlib.h>.
#[cfg(target_os = "macos")]
fn macos_sysroot(bindings: bindgen::Builder) -> bindgen::Builder {
    if let Ok(out) = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        && out.status.success()
    {
        let sdk = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !sdk.is_empty() {
            return bindings
                .clang_arg(format!("-isysroot{sdk}"))
                .clang_arg(format!("-I{sdk}/usr/include"));
        }
    }
    bindings
}
