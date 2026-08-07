#[macro_use]
mod macros;

use std::env;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

const ALLOWED_BINDINGS: &[&str] = include!("allowed_bindings.rs");

struct PhpBuild {
    /// include directories (no `-I` prefix).
    includes: Vec<String>,
    /// directories to search for libphp.
    lib_dirs: Vec<String>,
    version: (u32, u32),
}

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

    // `php84` covers every version below 8.5, not just 8.4: those are the ones whose
    // OPcache gates startup on a SAPI-name allowlist (see build_sapi_module).
    if php.version >= (8, 5) {
        println!("cargo:rustc-cfg=php85");
    } else {
        println!("cargo:rustc-cfg=php84");
    }

    let mut c = cc::Build::new();
    // Zend fixes the parameter list of every entry point it declares through a
    // macro (INTERNAL_FUNCTION_PARAMETERS' `return_value`, PHP_MINIT_FUNCTION's
    // INIT_FUNC_ARGS `type`/`module_number`), so an unused one is never actionable here.
    c.flag_if_supported("-Wno-unused-parameter");
    c.file("wrapper.c")
        .file("module.c")
        .file("rapira_classes.c")
        .file("rapira_http.c")
        .file("rapira_dispatcher.c");
    for d in &php.includes {
        c.include(d);
    }
    c.compile("rapira_shim");

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_args(php.includes.iter().map(|d| format!("-I{d}")))
        // php-src master on clang >=19 compiles the Zend VM in tail-call dispatch mode: every
        // opcode handler is a function using the `preserve_none` calling convention, and
        // `zend_op.handler` points to one. bindgen renders C structs field-by-field, so it must
        // emit that pointer's ABI - but 0.72.1 has no token for `preserve_none` and panics.
        // `opaque_type` makes bindgen emit `zend_op` as a byte array of clang's reported
        // size/align instead of its fields, so the handler pointer is never rendered. rapira reads
        // no `zend_op` field, so nothing is lost; a no-op on 8.4/8.5, which have no preserve_none.
        // https://clang.llvm.org/docs/AttributeReference.html#preserve-none
        .opaque_type("_zend_op");
    bindings = bindings.layout_tests(true);
    bindings = macos_sysroot(bindings);

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
        "rapira.stub.php",
        "rapira_exception_arginfo.h",
        "rapira_exception.stub.php",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_sysroot(bindings: bindgen::Builder) -> bindgen::Builder {
    // The libclang 19+ that reports the preserve_none convention no longer infers the SDK path, so
    // point it at the active SDK or the parse fails to find <stdlib.h> and friends.
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

#[cfg(not(target_os = "macos"))]
fn macos_sysroot(bindings: bindgen::Builder) -> bindgen::Builder {
    bindings
}

fn parse_version(v: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next().context("php version missing major")?.parse()?;
    let minor: u32 = it.next().context("php version missing minor")?.parse()?;
    Ok((major, minor))
}

fn discover_php() -> anyhow::Result<PhpBuild> {
    // `php-config --includes` emits space-separated `-I` flags; paths with spaces are
    // unrepresentable in that output, so splitting on whitespace is as good as it gets here.
    let includes: Vec<String> = php_config("--includes")?
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I").to_string())
        .collect();
    let prefix: String = php_config("--prefix")?;
    let version: (u32, u32) = parse_version(&php_config("--version")?)?;
    let bin: String = resolve_php_binary();
    anyhow::ensure!(
        !detect_zts(&bin)?,
        "rapira is NTS-only: `{bin}` is a thread-safe (ZTS) PHP build.\n\
         Rebuild PHP without --enable-zts, or point PHP_CONFIG at an NTS php-config."
    );
    Ok(PhpBuild {
        includes,
        lib_dirs: vec![format!("{prefix}/lib"), format!("{prefix}/lib64")],
        version,
    })
}

// `php-config --php-binary` can name a path that doesn't exist (Homebrew kegs report one),
// so fall back to `php` on PATH, which setup-php/Homebrew also install.
fn resolve_php_binary() -> String {
    if let Ok(bin) = php_config("--php-binary")
        && std::path::Path::new(&bin).exists()
    {
        return bin;
    }
    "php".to_string()
}

// ZTS reads the PHP_ZTS constant first (robust), falling back to the `php -i`
// "Thread Safety" field if the constant can't be read.
fn detect_zts(php_binary: &str) -> anyhow::Result<bool> {
    let zts_const = Command::new(php_binary)
        .args(["-r", "echo PHP_ZTS;"])
        .output()
        .ok()
        .and_then(|out| match String::from_utf8_lossy(&out.stdout).trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None, // usually not possible, but fall back to `php -i` if it happens
        });

    match zts_const {
        Some(z) => Ok(z),
        None => php_info_field(&php_info(php_binary)?, "Thread Safety")
            .map(|v| v == "enabled")
            .context("could not determine Thread Safety from `php -i`"),
    }
}

fn php_info(php_binary: &str) -> anyhow::Result<String> {
    let out = Command::new(php_binary)
        .arg("-i")
        .output()
        .context("running `php -i`")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// Value after a `Key => ` field in `php -i` output, trimmed.
fn php_info_field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key} => ");
    info.lines()
        .find_map(|l| l.split(needle.as_str()).nth(1))
        .map(str::trim)
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
