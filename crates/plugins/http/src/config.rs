use std::fs::{OpenOptions, remove_file};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use rapira_config::{
    ConfigCtx, ListenAddr, Mode, PoolSection, PoolSettings, check_entrypoint, nonzero_timeout,
    parse_listen, resolve_pool,
};
use serde::Deserialize;

use crate::multipart::Limits;
use crate::{Config, Server, UnsafeFieldNames};

/// The `[http]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Section {
    pub listen: Option<String>,
    pub server_name: Option<String>,
    pub server_port: Option<u16>,
    pub max_body_size_mb: Option<usize>,
    pub write_timeout_secs: Option<u64>,
    pub keepalive_timeout_secs: Option<u64>,
    pub unsafe_field_names: Option<UnsafeFieldNames>,
    pub uploads: Option<UploadsSection>,
    #[serde(default)]
    pub sendfile: SendfileSection,
    #[serde(default)]
    pub middleware: Vec<String>,
    pub r#static: Option<rapira_static_files::Section>,
    #[serde(default)]
    pub pool: PoolSection,
}

/// The `[http.sendfile]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendfileSection {
    pub root: Option<String>,
}

/// The `[http.uploads]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadsSection {
    pub dir: Option<String>,
    pub max_file_size_mb: Option<u64>,
    pub max_field_size_kb: Option<usize>,
    pub max_files: Option<usize>,
    pub max_parts: Option<usize>,
    pub max_part_headers: Option<usize>,
}

#[derive(Debug)]
pub struct Settings {
    pub listen: ListenAddr,
    pub server_name: String,
    pub server_port: u16,
    pub max_body_size: usize,
    pub write_timeout: Duration,
    pub keepalive_timeout: Duration,
    pub unsafe_field_names: UnsafeFieldNames,
    /// The multipart limits of a dispatcher pool. None in the other modes.
    pub uploads: Option<Limits>,
    /// sendFile() containment root; falls back to the entrypoint directory.
    pub sendfile_root: PathBuf,
    // [http].middleware list order
    pub middleware: Vec<Middleware>,
    pub pool: PoolSettings,
}

// ------------ middleware stuff ---------------
// add new middleware here, with settings if needed
#[derive(Debug)]
pub enum Middleware {
    Static(rapira_static_files::Settings),
}
// ---------------------------------------------

/// Boot checks run here: entrypoint file, uploads dir, static root, middleware names.
pub fn resolve(section: Section, ctx: &ConfigCtx) -> Result<Settings> {
    let settings = settings(section, ctx)?;
    check_entrypoint("http.pool", &settings.pool.entrypoint)?;
    for mw in &settings.middleware {
        match mw {
            Middleware::Static(st) => check_static_root(&st.root)?,
        }
    }
    if let Some(uploads) = &settings.uploads {
        check_uploads_dir(&uploads.dir)?;
    }
    Ok(settings)
}

/// The settings of `section`. Reads no file.
fn settings(section: Section, ctx: &ConfigCtx) -> Result<Settings> {
    let listen = parse_listen(
        "http",
        section.listen.as_deref(),
        ListenAddr::Tcp(SocketAddr::from((Ipv4Addr::LOCALHOST, 8000))),
    )?;

    let server_port = match section.server_port {
        Some(p) => p,
        None => match &listen {
            ListenAddr::Tcp(addr) => addr.port(),
            ListenAddr::Unix(_) => 80,
        },
    };

    let max_body_size_mb = section.max_body_size_mb.unwrap_or(8);
    if max_body_size_mb == 0 {
        bail!("http.max_body_size_mb must be at least 1");
    }
    let max_body_size = max_body_size_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("http.max_body_size_mb {max_body_size_mb} is too large"))?;

    let write_timeout = nonzero_timeout(
        "http",
        "write_timeout_secs",
        section.write_timeout_secs.unwrap_or(30),
    )?;
    let keepalive_timeout = nonzero_timeout(
        "http",
        "keepalive_timeout_secs",
        section.keepalive_timeout_secs.unwrap_or(60),
    )?;

    let pool = resolve_pool(section.pool, "http.pool", ctx)?;
    let uploads = if pool.mode == Mode::Dispatcher {
        Some(resolve_uploads(section.uploads.unwrap_or_default(), ctx)?)
    } else if section.uploads.is_some() {
        bail!(
            "http.uploads applies to dispatcher mode only (http.pool.mode = \"{}\")",
            pool.mode
        );
    } else {
        None
    };

    let sendfile_root = match section.sendfile.root.filter(|r| !r.is_empty()) {
        Some(r) => ctx.resolve_path(&r)?,
        None => pool
            .entrypoint
            .parent()
            .unwrap_or(Path::new("/"))
            .to_path_buf(),
    };

    let static_files = section
        .r#static
        .map(|s| rapira_static_files::resolve(s, ctx))
        .transpose()?;
    let middleware = resolve_middleware(section.middleware, static_files)?;

    Ok(Settings {
        listen,
        server_name: section
            .server_name
            .unwrap_or_else(|| "localhost".to_owned()),
        server_port,
        max_body_size,
        write_timeout,
        keepalive_timeout,
        unsafe_field_names: section.unsafe_field_names.unwrap_or_default(),
        uploads,
        sendfile_root,
        middleware,
        pool,
    })
}

// resolve all existing vs requested middlewares
// TODO: think about some container, like endure in RR
fn resolve_middleware(
    list: Vec<String>,
    mut static_files: Option<rapira_static_files::Settings>,
) -> Result<Vec<Middleware>> {
    for (i, name) in list.iter().enumerate() {
        if list[..i].contains(name) {
            bail!("http.middleware lists \"{name}\" twice")
        }
    }

    let mut middleware: Vec<Middleware> = Vec::new();
    for name in &list {
        match name.as_str() {
            // check new middleware here
            "static" => match static_files.take() {
                Some(settings) => middleware.push(Middleware::Static(settings)),
                None => {
                    bail!("http.middleware lists \"static\" but [http.static] is missing")
                }
            },
            other => {
                bail!("http.middleware entry \"{other}\" is unknown; known middleware: \"static\"")
            }
        }
    }

    // check for the configured, but not listed
    // TODO: worth double checking
    // some people requested some kind of enabled=false/true
    if static_files.is_some() {
        bail!("[http.static] is configured but http.middleware does not list \"static\"");
    }

    Ok(middleware)
}

fn resolve_uploads(section: UploadsSection, ctx: &ConfigCtx) -> Result<Limits> {
    let dir = match section.dir.filter(|d| !d.is_empty()) {
        Some(d) => ctx.resolve_path(&d)?,
        None => std::env::temp_dir(),
    };
    let max_file_size_mb = section.max_file_size_mb.unwrap_or(2);
    if max_file_size_mb == 0 {
        bail!("http.uploads.max_file_size_mb must be at least 1");
    }
    let max_file_size = max_file_size_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| anyhow!("http.uploads.max_file_size_mb {max_file_size_mb} is too large"))?;
    let max_field_size_kb = section.max_field_size_kb.unwrap_or(256);
    if max_field_size_kb == 0 {
        bail!("http.uploads.max_field_size_kb must be at least 1");
    }
    let max_field_size = max_field_size_kb.checked_mul(1024).ok_or_else(|| {
        anyhow!("http.uploads.max_field_size_kb {max_field_size_kb} is too large")
    })?;
    let max_parts = section.max_parts.unwrap_or(1024);
    if max_parts == 0 {
        bail!("http.uploads.max_parts must be at least 1");
    }
    let max_part_headers = section.max_part_headers.unwrap_or(32);
    if max_part_headers == 0 {
        bail!("http.uploads.max_part_headers must be at least 1");
    }
    let max_files = section.max_files.unwrap_or(20);
    if max_files == 0 {
        bail!("http.uploads.max_files must be at least 1");
    }
    Ok(Limits {
        dir,
        max_file_size,
        max_field_size,
        max_files,
        max_parts,
        max_part_headers,
    })
}

fn check_static_root(root: &Path) -> Result<()> {
    // is_dir() folds every stat error into false; metadata keeps the errno visible.
    let meta = std::fs::metadata(root)
        .map_err(|e| anyhow!("http.static.root {} is not accessible: {e}", root.display()))?;
    ensure!(
        meta.is_dir(),
        "http.static.root {} is not a directory",
        root.display()
    );
    // Serving needs search permission, not read; resolving `.` inside the root proves it.
    std::fs::metadata(root.join("."))
        .map_err(|e| anyhow!("http.static.root {} is not accessible: {e}", root.display()))?;
    Ok(())
}

/// The plugin spools file parts here, so the dir must exist and accept a new file.
fn check_uploads_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow!("creating http.uploads.dir {}: {e}", dir.display()))?;
    let probe = dir.join(format!(".rapira-probe-{}", std::process::id()));
    let _ = remove_file(&probe);
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| anyhow!("http.uploads.dir {} is not writable: {e}", dir.display()))?;
    let _ = remove_file(&probe);
    Ok(())
}

impl Server {
    /// Builds the middleware layers in list order. Runs in the master after the logger starts, so the diagnostics here reach the log.
    pub fn from_settings(settings: Settings) -> Self {
        let mut middleware: Vec<crate::middleware::Layer> = Vec::new();
        for mw in settings.middleware {
            match mw {
                Middleware::Static(st) => {
                    tracing::info!(target: "rapira", "static files from {}, forbid {:?}", st.root.display(), st.forbid);
                    middleware
                        .push(rapira_static_files::StaticFiles::new(st.root, st.forbid).layer());
                }
            }
        }

        // sendFile() root: a boot-time diagnostic so a bad path is caught before the first request,
        // even under ondemand scaling where no worker forks at boot.
        if let Err(e) = std::fs::metadata(&settings.sendfile_root) {
            tracing::warn!(
                target: "rapira",
                "http.sendfile.root {} is not accessible: {e}; sendFile() will reject every path",
                settings.sendfile_root.display()
            );
        }

        Self::init(Config {
            listen: settings.listen,
            server_name: settings.server_name,
            server_port: settings.server_port,
            max_body_size: settings.max_body_size,
            write_timeout: settings.write_timeout,
            unsafe_field_names: settings.unsafe_field_names,
            superglobals: settings.pool.mode != Mode::Dispatcher,
            keepalive_timeout: settings.keepalive_timeout,
            middleware,
            uploads: settings.uploads,
            sendfile_root: settings.sendfile_root,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ConfigCtx {
        ConfigCtx {
            dir: PathBuf::from("/w"),
        }
    }

    /// The settings of an `[http]` table in TOML, without the boot checks.
    fn settings_of(toml: &str) -> Result<Settings> {
        settings(toml::from_str(toml)?, &ctx())
    }

    struct Case {
        name: &'static str,
        toml: &'static str,
        error: &'static str,
    }

    /// The list is the activation switch: every configured section must be listed, every listed name must be known, configured, and unique.
    #[test]
    fn middleware_list_and_tables_must_agree() {
        let cases = [
            Case {
                name: "unknown name",
                toml: "middleware = [\"staticc\"]\n[pool]\nentrypoint = \"w.php\"",
                error: "http.middleware entry \"staticc\" is unknown; known middleware: \"static\"",
            },
            Case {
                name: "listed without table",
                toml: "middleware = [\"static\"]\n[pool]\nentrypoint = \"w.php\"",
                error: "http.middleware lists \"static\" but [http.static] is missing",
            },
            Case {
                name: "table without listing",
                toml: "[static]\nroot = \".\"\n[pool]\nentrypoint = \"w.php\"",
                error: "[http.static] is configured but http.middleware does not list \"static\"",
            },
            Case {
                name: "duplicate",
                toml: "middleware = [\"static\", \"static\"]\n[static]\nroot = \".\"\n[pool]\nentrypoint = \"w.php\"",
                error: "http.middleware lists \"static\" twice",
            },
        ];
        for case in cases {
            let section: Section = toml::from_str(case.toml).unwrap();
            let err = resolve(section, &ctx()).unwrap_err().to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }

    /// `allow` is rejected too: there is no off-switch, so asking for one must fail loudly.
    #[test]
    fn section_errors_name_the_key() {
        let cases = [
            Case {
                name: "unknown unsafe_field_names value",
                toml: "unsafe_field_names = \"dorp\"\n[pool]\nentrypoint = \"a.php\"\n",
                error: "unknown variant `dorp`",
            },
            Case {
                name: "allow is no unsafe_field_names value",
                toml: "unsafe_field_names = \"allow\"\n[pool]\nentrypoint = \"a.php\"\n",
                error: "unknown variant `allow`",
            },
            Case {
                name: "max body size overflows",
                toml: "max_body_size_mb = 17592186044416\n[pool]\nentrypoint = \"a.php\"\n",
                error: "http.max_body_size_mb 17592186044416 is too large",
            },
            Case {
                name: "write timeout above the cap",
                toml: "write_timeout_secs = 100000\n[pool]\nentrypoint = \"a.php\"\n",
                error: "http.write_timeout_secs 100000 is too large (max 86400)",
            },
            Case {
                name: "keepalive timeout above the cap",
                toml: "keepalive_timeout_secs = 100000\n[pool]\nentrypoint = \"a.php\"\n",
                error: "http.keepalive_timeout_secs 100000 is too large (max 86400)",
            },
            Case {
                name: "zero max_files would 413 every file part",
                toml: "[pool]\nentrypoint = \"a.php\"\n[uploads]\nmax_files = 0\n",
                error: "http.uploads.max_files must be at least 1",
            },
            Case {
                name: "uploads under classic mode",
                toml: "[pool]\nentrypoint = \"a.php\"\nmode = \"classic\"\n[uploads]\nmax_files = 4\n",
                error: "http.uploads applies to dispatcher mode only (http.pool.mode = \"classic\")",
            },
            Case {
                name: "uploads under worker mode",
                toml: "[pool]\nentrypoint = \"a.php\"\nmode = \"worker\"\n[uploads]\nmax_files = 4\n",
                error: "http.uploads applies to dispatcher mode only (http.pool.mode = \"worker\")",
            },
            Case {
                name: "static without root",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\n",
                error: "http.static.root is required",
            },
            Case {
                name: "static with an empty root",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"\"\n",
                error: "http.static.root is required",
            },
            Case {
                name: "forbid entry without a dot",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = [\"php\"]\n",
                error: "http.static.forbid entries must be extensions with a leading dot (`php`)",
            },
            Case {
                name: "empty forbid entry",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = [\"\"]\n",
                error: "http.static.forbid entries must be extensions with a leading dot (``)",
            },
            Case {
                name: "forbid entry of a dot only",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = [\".\"]\n",
                error: "http.static.forbid entries must be extensions with a leading dot (`.`)",
            },
            Case {
                name: "forbid entry with whitespace",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = [\".php \"]\n",
                error: "http.static.forbid entries must be extensions with a leading dot (`.php `)",
            },
            Case {
                name: "forbid entry with a separator",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = [\"./php\"]\n",
                error: "http.static.forbid entries must be extensions with a leading dot (`./php`)",
            },
            Case {
                name: "unknown static key",
                toml: "[pool]\nentrypoint = \"a.php\"\n[static]\nbogus = 1\n",
                error: "unknown field `bogus`",
            },
        ];
        for case in cases {
            let err = settings_of(case.toml).expect_err(case.name).to_string();
            assert!(err.contains(case.error), "{}: {err}", case.name);
        }
    }

    struct Want {
        listen: &'static str,
        server_port: u16,
        max_body_size: usize,
        unsafe_field_names: UnsafeFieldNames,
        sendfile_root: &'static str,
    }

    /// What `[pool]\nentrypoint = "a.php"` resolves to.
    fn base() -> Want {
        Want {
            listen: "127.0.0.1:8000",
            server_port: 8000,
            max_body_size: 8 * 1024 * 1024,
            unsafe_field_names: UnsafeFieldNames::Drop,
            sendfile_root: "/w",
        }
    }

    struct ResolveCase {
        name: &'static str,
        toml: &'static str,
        want: Want,
    }

    #[test]
    fn section_resolves() {
        let cases = [
            ResolveCase {
                name: "defaults",
                toml: "[pool]\nentrypoint = \"a.php\"\n",
                want: base(),
            },
            ResolveCase {
                name: "tcp listen sets the server port",
                toml: "listen = \"0.0.0.0:9000\"\n[pool]\nentrypoint = \"a.php\"\n",
                want: Want {
                    listen: "0.0.0.0:9000",
                    server_port: 9000,
                    ..base()
                },
            },
            ResolveCase {
                name: "port-only listen binds every interface",
                toml: "listen = \":9000\"\n[pool]\nentrypoint = \"a.php\"\n",
                want: Want {
                    listen: "0.0.0.0:9000",
                    server_port: 9000,
                    ..base()
                },
            },
            ResolveCase {
                name: "unix listen serves port 80",
                toml: "listen = \"unix:/run/r.sock\"\n[pool]\nentrypoint = \"a.php\"\n",
                want: Want {
                    listen: "unix:/run/r.sock",
                    server_port: 80,
                    ..base()
                },
            },
            ResolveCase {
                name: "max body size converts from MB",
                toml: "max_body_size_mb = 2\n[pool]\nentrypoint = \"a.php\"\n",
                want: Want {
                    max_body_size: 2 * 1024 * 1024,
                    ..base()
                },
            },
            ResolveCase {
                name: "drop unsafe field names",
                toml: "unsafe_field_names = \"drop\"\n[pool]\nentrypoint = \"a.php\"\n",
                want: base(),
            },
            ResolveCase {
                name: "reject unsafe field names",
                toml: "unsafe_field_names = \"reject\"\n[pool]\nentrypoint = \"a.php\"\n",
                want: Want {
                    unsafe_field_names: UnsafeFieldNames::Reject,
                    ..base()
                },
            },
            ResolveCase {
                name: "sendfile root follows the entrypoint",
                toml: "[pool]\nentrypoint = \"public/index.php\"\n",
                want: Want {
                    sendfile_root: "/w/public",
                    ..base()
                },
            },
            ResolveCase {
                name: "sendfile root resolves against the config dir",
                toml: "[pool]\nentrypoint = \"public/index.php\"\n[sendfile]\nroot = \"assets\"\n",
                want: Want {
                    sendfile_root: "/w/assets",
                    ..base()
                },
            },
        ];
        for case in cases {
            let got = settings_of(case.toml).unwrap_or_else(|e| panic!("{}: {e}", case.name));
            assert_eq!(got.listen.to_string(), case.want.listen, "{}", case.name);
            assert_eq!(got.server_port, case.want.server_port, "{}", case.name);
            assert_eq!(got.max_body_size, case.want.max_body_size, "{}", case.name);
            assert_eq!(
                got.unsafe_field_names, case.want.unsafe_field_names,
                "{}",
                case.name
            );
            assert_eq!(
                got.sendfile_root,
                Path::new(case.want.sendfile_root),
                "{}",
                case.name
            );
        }
    }

    struct UploadsCase {
        name: &'static str,
        toml: &'static str,
        want: Option<Limits>,
    }

    /// Every knob resolves with unit conversion and a config-relative dir.
    #[test]
    fn uploads_resolve_in_dispatcher_mode_only() {
        let cases = [
            UploadsCase {
                name: "every knob",
                toml: "[pool]\nentrypoint = \"a.php\"\n[uploads]\ndir = \"spool\"\n\
                       max_file_size_mb = 3\nmax_field_size_kb = 7\nmax_files = 4\n\
                       max_parts = 9\nmax_part_headers = 5\n",
                want: Some(Limits {
                    dir: PathBuf::from("/w/spool"),
                    max_file_size: 3 * 1024 * 1024,
                    max_field_size: 7 * 1024,
                    max_files: 4,
                    max_parts: 9,
                    max_part_headers: 5,
                }),
            },
            UploadsCase {
                name: "an empty table keeps the defaults",
                toml: "[pool]\nentrypoint = \"a.php\"\n[uploads]\n",
                want: Some(Limits::default()),
            },
            UploadsCase {
                name: "no table in dispatcher mode",
                toml: "[pool]\nentrypoint = \"a.php\"\n",
                want: Some(Limits::default()),
            },
            UploadsCase {
                name: "no table in classic mode",
                toml: "[pool]\nentrypoint = \"a.php\"\nmode = \"classic\"\n",
                want: None,
            },
        ];
        for case in cases {
            let got = settings_of(case.toml).unwrap_or_else(|e| panic!("{}: {e}", case.name));
            assert_eq!(got.uploads, case.want, "{}", case.name);
        }
    }

    struct StaticCase {
        name: &'static str,
        toml: &'static str,
        /// The root and forbid list of the one static middleware; None for no middleware.
        want: Option<(&'static str, &'static [&'static str])>,
    }

    /// The root resolves config-relative like every other path key; forbid defaults to the PHP source guard. The resolver validates shape only. The middleware constructor normalizes the case.
    #[test]
    fn static_middleware_resolves() {
        let cases = [
            StaticCase {
                name: "relative root and default forbid",
                toml: "middleware = [\"static\"]\n[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"public\"\n",
                want: Some(("/w/public", &[".php"])),
            },
            StaticCase {
                name: "absolute root",
                toml: "middleware = [\"static\"]\n[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"/srv/pub\"\n",
                want: Some(("/srv/pub", &[".php"])),
            },
            StaticCase {
                name: "forbid keeps its case",
                toml: "middleware = [\"static\"]\n[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = [\".PHP\", \".Phtml\"]\n",
                want: Some(("/w/p", &[".PHP", ".Phtml"])),
            },
            StaticCase {
                name: "empty forbid",
                toml: "middleware = [\"static\"]\n[pool]\nentrypoint = \"a.php\"\n[static]\nroot = \"p\"\nforbid = []\n",
                want: Some(("/w/p", &[])),
            },
            StaticCase {
                name: "no middleware",
                toml: "[pool]\nentrypoint = \"a.php\"\n",
                want: None,
            },
        ];
        for case in cases {
            let got = settings_of(case.toml).unwrap_or_else(|e| panic!("{}: {e}", case.name));
            let got: Vec<(PathBuf, Vec<String>)> = got
                .middleware
                .into_iter()
                .map(|mw| match mw {
                    Middleware::Static(st) => (st.root, st.forbid),
                })
                .collect();
            let want: Vec<(PathBuf, Vec<String>)> = case
                .want
                .into_iter()
                .map(|(root, forbid)| {
                    (
                        PathBuf::from(root),
                        forbid.iter().map(|s| (*s).to_owned()).collect(),
                    )
                })
                .collect();
            assert_eq!(got, want, "{}", case.name);
        }
    }

    /// The shipped example is the file users copy, so its `[http]` table must resolve and keep its documented values.
    #[test]
    fn shipped_example_resolves() {
        let file: toml::Table = toml::from_str(include_str!("../../../../examples/rapira.toml"))
            .expect("the example parses");
        let section: Section = file["http"].clone().try_into().expect("[http] parses");
        let http = settings(
            section,
            &ConfigCtx {
                dir: PathBuf::from("/srv/app"),
            },
        )
        .expect("[http] resolves");
        assert_eq!(http.listen.to_string(), "127.0.0.1:8000");
        assert_eq!(http.server_port, 8000);
        assert_eq!(
            http.pool.entrypoint,
            Path::new("/srv/app/dispatcher-sync.php")
        );
        assert_eq!(http.pool.mode, Mode::Dispatcher);
        assert_eq!(http.sendfile_root, Path::new("/srv/app"));
    }
}
