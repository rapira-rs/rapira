#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

struct PhpBuild {
    includes: Vec<String>,
    lib_dirs: Vec<String>,
    version: (u32, u32),
}

// bindgen 0.73 panics on php-src master's `preserve_none` opcode handlers, so `_zend_op` stays opaque: https://clang.llvm.org/docs/AttributeReference.html#preserve-none
fn main() -> anyhow::Result<()> {
    println!("cargo:rustc-check-cfg=cfg(php84)");
    println!("cargo:rustc-check-cfg=cfg(php85)");
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");

    let php = discover_php()?;

    for dir in &php.lib_dirs {
        println!("cargo:rustc-link-search=native={dir}");
    }
    println!("cargo:rustc-link-lib=dylib=php");

    if php.version >= (8, 5) {
        println!("cargo:rustc-cfg=php85");
    } else {
        println!("cargo:rustc-cfg=php84");
    }

    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let mut c = cc::Build::new();
    c.flag_if_supported("-Wno-unused-parameter");
    c.define("RAPIRA_VERSION", format!("\"{version}\"").as_str());
    c.file("wrapper.c")
        .file("module.c")
        .file("rapira_classes.c")
        .file("rapira_http.c")
        .file("rapira_dispatcher.c")
        .file("rapira_exchange.c");
    for d in &php.includes {
        c.include(d);
    }
    c.compile("rapira_shim");

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
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

    for f in [
        "wrapper.h",
        "module.c",
        "wrapper.c",
        "rapira_http.c",
        "rapira_http_arginfo.h",
        "rapira_http.stub.php",
        "rapira_arginfo.h",
        "allowed_bindings.rs",
        "rapira_classes.c",
        "rapira_classes.h",
        "rapira_dispatcher.c",
        "rapira_exchange.c",
        "rapira.stub.php",
        "rapira_exception_arginfo.h",
        "rapira_exception.stub.php",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }

    Ok(())
}

// libclang 19+ does not infer the macOS SDK path: without -isysroot the parse cannot find <stdlib.h>.
#[cfg(target_os = "macos")]
fn macos_sysroot(bindings: bindgen::Builder) -> bindgen::Builder {
    if let Ok(out) = Command::new("xcrun").args(["--show-sdk-path"]).output()
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

fn parse_version(v: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next().context("php version missing major")?.parse()?;
    let minor: u32 = it.next().context("php version missing minor")?.parse()?;
    Ok((major, minor))
}

fn discover_php() -> anyhow::Result<PhpBuild> {
    let includes: Vec<String> = php_config("--includes")?
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I").to_string())
        .collect();
    let prefix: String = php_config("--prefix")?;
    let version: (u32, u32) = parse_version(&php_config("--version")?)?;
    Ok(PhpBuild {
        includes,
        lib_dirs: vec![format!("{prefix}/lib"), format!("{prefix}/lib64")],
        version,
    })
}

fn php_config(arg: &str) -> anyhow::Result<String> {
    let bin = env::var("PHP_CONFIG").unwrap_or_else(|_| "php-config".into());
    let out = Command::new(&bin)
        .arg(arg)
        .output()
        .with_context(|| format!("running {bin} {arg}"))?;

    anyhow::ensure!(
        out.status.success(),
        "{bin} {arg} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
