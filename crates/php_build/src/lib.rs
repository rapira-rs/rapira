use std::env;
use std::process::Command;

use anyhow::Context;

/// What the build scripts need from `php-config`.
pub struct Php {
    pub includes: Vec<String>,
    pub lib_dirs: Vec<String>,
    pub version: (u32, u32),
}

/// Runs `php-config` (or `$PHP_CONFIG`) and fails with its stderr when it cannot run.
pub fn discover() -> anyhow::Result<Php> {
    let includes: Vec<String> = php_config("--includes")?
        .split_whitespace()
        .map(|s| s.trim_start_matches("-I").to_string())
        .collect();
    let prefix: String = php_config("--prefix")?;
    let version: (u32, u32) = parse_version(&php_config("--version")?)?;
    Ok(Php {
        includes,
        lib_dirs: vec![format!("{prefix}/lib"), format!("{prefix}/lib64")],
        version,
    })
}

/// Compiles `files` with cc into a static library named `name`, with the PHP include dirs and `extra_includes`, and defines RAPIRA_VERSION from CARGO_PKG_VERSION.
pub fn compile(name: &str, files: &[&str], php: &Php, extra_includes: &[&str]) {
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let mut c = cc::Build::new();
    c.flag_if_supported("-Wno-unused-parameter");
    c.define("RAPIRA_VERSION", format!("\"{version}\"").as_str());
    c.files(files);
    c.includes(&php.includes);
    c.includes(extra_includes);
    c.compile(name);
}

/// Emits rerun-if-changed for each file and for PATH and PHP_CONFIG.
pub fn rerun_if_changed(files: &[&str]) {
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PHP_CONFIG");
    for f in files {
        println!("cargo:rerun-if-changed={f}");
    }
}

fn parse_version(v: &str) -> anyhow::Result<(u32, u32)> {
    let mut it = v.trim().split('.');
    let major: u32 = it.next().context("php version missing major")?.parse()?;
    let minor: u32 = it.next().context("php version missing minor")?.parse()?;
    Ok((major, minor))
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
