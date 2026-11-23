use crate::harness::*;
use rapira_config::Mode;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tests::server_log::{app_contexts, records, wait_app_record};
use tests::wire::{Frame, submit};
use tests::{drain, poll, req};
use tokio::sync::mpsc::Receiver;

/// USR2 recycles every app while the listener continues to serve requests.
#[test]
fn usr2_reload_restarts_the_apps_while_serving() {
    let workers = if is_zts() { 3 } else { 1 };
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/boot-log-worker.php"),
    )
    .workers(workers)
    .json_log()
    .spawn();
    wait_serving(&srv, "http", workers);
    let storm = storm(srv.addr, 8);
    std::thread::sleep(Duration::from_millis(500));
    signal(srv.pid(), libc::SIGUSR2);
    let log = srv.log_file();
    let booted = poll(Duration::from_secs(20), || {
        (app_contexts(&log, "booted").len() >= 2 * workers).then_some(())
    });
    let tally = storm.halt();
    assert!(
        booted.is_some(),
        "every app booted again\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        (tally.failed, tally.refused, tally.dropped),
        (0, 0, 0),
        "failed, refused and dropped requests; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
    assert!(tally.ok > 0, "the storm got no response");
    assert!(app_contexts(&log, "app loop ended").len() >= workers);
    wait_recycled(&srv, "http");
}

/// SIGUSR2 recycles an idle app without a request.
#[test]
fn usr2_restarts_an_idle_app() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/boot-log-worker.php"),
    )
    .json_log()
    .spawn();
    wait_serving(&srv, "http", 1);
    let log = srv.log_file();
    wait_app_record(&log, "booted");
    signal(srv.pid(), libc::SIGUSR2);
    let booted = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        booted.is_some(),
        "the idle app booted again\n{}",
        diagnostics(&srv)
    );
    slot_line(&srv, "state=idle handled=0 errors=0 recycles=1 ");
}

/// How a deploy puts release 2 in place.
enum Deploy {
    /// The `current` symlink points at the folder of release 2.
    Symlink,
    /// Release 2 replaces the files of release 1 at the same paths.
    InPlace,
}

struct DeployCase {
    name: &'static str,
    mode: Mode,
    /// The fixture in `deploy/` that each release folder gets as its entrypoint.
    entrypoint: &'static str,
    deploy: Deploy,
    /// A reload before the deploy, so the deploy reload comes less than opcache.revalidate_freq (2 s) after an app boot.
    reload_first: bool,
    /// The opcache.validate_timestamps value. 0 is the usual production value.
    validate_timestamps: u8,
}

/// The body that a `deploy/` fixture prints.
fn seen(srv: &Server) -> String {
    let got = drain(submit(srv.addr, req("/")).expect("GET /"));
    assert_eq!(got.status, 200, "\n{}", diagnostics(srv));
    got.body
}

/// Writes `contents` to `path` with a modification time of one hour ago. OPcache caches a file only when it is older than opcache.file_update_protection (2 s), as the files of a real deploy are.
fn write_old(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now() - Duration::from_secs(3600)))
        .unwrap_or_else(|e| panic!("set the time of {}: {e}", path.display()));
}

/// `releases/1` and `releases/2` under `dir`, each with the entrypoint, release.php and release.txt, and `current` pointing at release 1. Returns the real path of each release folder, as getcwd() reports it.
fn stage_releases(dir: &Path, entrypoint: &str) -> [String; 2] {
    let source = std::fs::read_to_string(fixture_path(&format!("deploy/{entrypoint}")))
        .expect("read the deploy fixture");
    let folders = ["1", "2"].map(|release| {
        let folder = dir.join("releases").join(release);
        std::fs::create_dir_all(&folder).expect("create the release folder");
        write_old(&folder.join(entrypoint), &source);
        write_old(
            &folder.join("release.php"),
            &format!("<?php return '{release}';\n"),
        );
        write_old(&folder.join("release.txt"), release);
        std::fs::canonicalize(&folder)
            .expect("canonicalize the release folder")
            .display()
            .to_string()
    });
    std::os::unix::fs::symlink("releases/1", dir.join("current")).expect("link current");
    folders
}

/// Waits until the classic worker handles the reload before sending the first request of the new release.
fn wait_for_the_classic_reload(srv: &Server) {
    let log = srv.log_file();
    let handled = poll(Duration::from_secs(10), || {
        std::fs::read_to_string(&log)
            .is_ok_and(|text| text.contains("reload requested; resolving the entrypoint again"))
            .then_some(())
    });
    assert!(
        handled.is_some(),
        "the worker handles the reload\n{}",
        diagnostics(srv)
    );
}

/// A reload after a deploy moves the pool to release 2: its code, its working directory, its relative file reads and the root of sendFile(). The OPcache reset of the reload logs no WARN record of a scheduled restart.
#[test]
fn a_reload_after_a_deploy_runs_the_new_release() {
    let cases = [
        DeployCase {
            name: "worker mode, symlink switch",
            mode: Mode::Worker,
            entrypoint: "worker.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 1,
        },
        DeployCase {
            name: "dispatcher mode, symlink switch, sendFile() under __DIR__",
            mode: Mode::Dispatcher,
            entrypoint: "dispatcher.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 1,
        },
        DeployCase {
            name: "classic mode, symlink switch",
            mode: Mode::Classic,
            entrypoint: "classic.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 1,
        },
        DeployCase {
            name: "worker mode, symlink switch, opcache.validate_timestamps = 0",
            mode: Mode::Worker,
            entrypoint: "worker.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 0,
        },
        DeployCase {
            name: "classic mode, symlink switch, opcache.validate_timestamps = 0",
            mode: Mode::Classic,
            entrypoint: "classic.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 0,
        },
        DeployCase {
            name: "classic mode, symlink switch, a require through the symlink path",
            mode: Mode::Classic,
            entrypoint: "classic-through-current.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 1,
        },
        DeployCase {
            name: "worker mode, symlink switch, a require through the symlink path",
            mode: Mode::Worker,
            entrypoint: "worker-through-current.php",
            deploy: Deploy::Symlink,
            reload_first: false,
            validate_timestamps: 1,
        },
        DeployCase {
            name: "worker mode, in-place change less than 1 s after a reload",
            mode: Mode::Worker,
            entrypoint: "worker.php",
            deploy: Deploy::InPlace,
            reload_first: true,
            validate_timestamps: 1,
        },
        DeployCase {
            name: "worker mode, in-place change, opcache.validate_timestamps = 0",
            mode: Mode::Worker,
            entrypoint: "worker.php",
            deploy: Deploy::InPlace,
            reload_first: false,
            validate_timestamps: 0,
        },
        DeployCase {
            name: "classic mode, in-place change, opcache.validate_timestamps = 0",
            mode: Mode::Classic,
            entrypoint: "classic.php",
            deploy: Deploy::InPlace,
            reload_first: false,
            validate_timestamps: 0,
        },
    ];
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    for case in &cases {
        // The OPcache values, written out so that an ambient ini file does not change them.
        let ini = format!(
            "{shared}\nopcache.enable = 1\nopcache.validate_timestamps = {}\nopcache.revalidate_freq = 2\n",
            case.validate_timestamps
        );
        let dir: PathBuf = scratch_dir();
        let [release_1, release_2] = stage_releases(&dir, case.entrypoint);
        let entrypoint = dir.join("current").join(case.entrypoint);
        let srv = Spawn::http_entrypoint(case.mode, &entrypoint.display().to_string())
            .php_ini(&ini)
            .json_log()
            .spawn();

        assert_eq!(
            seen(&srv),
            format!("code=1 cwd={release_1} file=1"),
            "{}: before the deploy\n{}",
            case.name,
            diagnostics(&srv)
        );
        if case.reload_first {
            signal(srv.pid(), libc::SIGUSR2);
            let log = srv.log_file();
            let rebooted = poll(Duration::from_secs(10), || {
                (app_contexts(&log, "booted").len() >= 2).then_some(())
            });
            assert!(
                rebooted.is_some(),
                "{}: the first reload boots the app again\n{}",
                case.name,
                diagnostics(&srv)
            );
        }
        let want = match case.deploy {
            Deploy::Symlink => {
                let next = dir.join("current.next");
                std::os::unix::fs::symlink("releases/2", &next).expect("link current.next");
                std::fs::rename(&next, dir.join("current")).expect("switch current");
                format!("code=2 cwd={release_2} file=2")
            }
            Deploy::InPlace => {
                let folder = dir.join("releases/1");
                std::fs::write(folder.join("release.php"), "<?php return '2';\n")
                    .expect("write release.php");
                std::fs::write(folder.join("release.txt"), "2").expect("write release.txt");
                format!("code=2 cwd={release_1} file=2")
            }
        };
        signal(srv.pid(), libc::SIGUSR2);

        let mut last = String::new();
        let moved = poll(Duration::from_secs(10), || {
            last = seen(&srv);
            (last == want).then_some(())
        });
        assert!(
            moved.is_some(),
            "{}: after the deploy and the reload\n want: {want}\n last: {last}\n{}",
            case.name,
            diagnostics(&srv)
        );
        // A restart reason that the hook keeps reaches the log at the next pull, so one more request runs that pull.
        assert_eq!(
            seen(&srv),
            want,
            "{}: one more request after the reload\n{}",
            case.name,
            diagnostics(&srv)
        );
        let restarts: Vec<String> = records(&srv.log_file())
            .into_iter()
            .filter(|r| {
                r.level == tracing::Level::WARN
                    && r.message.starts_with("OPcache scheduled a restart")
            })
            .map(|r| r.message)
            .collect();
        assert!(
            restarts.is_empty(),
            "{}: no WARN record of a scheduled OPcache restart\n got: {restarts:?}\n{}",
            case.name,
            diagnostics(&srv)
        );
        drop(srv);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A reload after a symlink deploy moves the script paths of $_SERVER to release 2. SCRIPT_NAME and PHP_SELF keep the configured file name.
#[test]
fn a_reload_after_a_deploy_moves_the_server_script_paths() {
    struct Case {
        name: &'static str,
        mode: Mode,
        /// The fixture in `deploy/` that each release folder gets as its entrypoint.
        entrypoint: &'static str,
    }
    let cases = [
        Case {
            name: "classic mode",
            mode: Mode::Classic,
            entrypoint: "classic-server-paths.php",
        },
        Case {
            name: "worker mode",
            mode: Mode::Worker,
            entrypoint: "worker-server-paths.php",
        },
    ];
    for case in &cases {
        let dir: PathBuf = scratch_dir();
        let [release_1, release_2] = stage_releases(&dir, case.entrypoint);
        let entrypoint = dir.join("current").join(case.entrypoint);
        let srv = Spawn::http_entrypoint(case.mode, &entrypoint.display().to_string()).spawn();
        let paths = |release: &str| {
            format!(
                "file={release}/{e} root={release} name=/{e} self=/{e}",
                e = case.entrypoint
            )
        };

        assert_eq!(
            seen(&srv),
            paths(&release_1),
            "{}: the first request before the deploy\n{}",
            case.name,
            diagnostics(&srv)
        );
        assert_eq!(
            seen(&srv),
            paths(&release_1),
            "{}: the second request before the deploy\n{}",
            case.name,
            diagnostics(&srv)
        );
        let next = dir.join("current.next");
        std::os::unix::fs::symlink("releases/2", &next).expect("link current.next");
        std::fs::rename(&next, dir.join("current")).expect("switch current");
        signal(srv.pid(), libc::SIGUSR2);

        let want = paths(&release_2);
        let mut last = String::new();
        let moved = poll(Duration::from_secs(10), || {
            last = seen(&srv);
            (last == want).then_some(())
        });
        assert!(
            moved.is_some(),
            "{}: after the deploy and the reload\n want: {want}\n last: {last}\n{}",
            case.name,
            diagnostics(&srv)
        );
        drop(srv);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// In classic mode the first request after a reload runs the new release, also with opcache.validate_timestamps = 0.
#[test]
fn the_first_classic_request_after_a_reload_runs_the_new_release() {
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    let ini = format!("{shared}\nopcache.enable = 1\nopcache.validate_timestamps = 0\n");
    let dir: PathBuf = scratch_dir();
    let [release_1, _] = stage_releases(&dir, "classic.php");
    let entrypoint = dir.join("current").join("classic.php");
    let srv = Spawn::http_entrypoint(Mode::Classic, &entrypoint.display().to_string())
        .php_ini(&ini)
        .spawn();
    assert_eq!(
        seen(&srv),
        format!("code=1 cwd={release_1} file=1"),
        "before the deploy\n{}",
        diagnostics(&srv)
    );
    let folder = dir.join("releases/1");
    std::fs::write(folder.join("release.php"), "<?php return '2';\n").expect("write release.php");
    std::fs::write(folder.join("release.txt"), "2").expect("write release.txt");
    signal(srv.pid(), libc::SIGUSR2);
    wait_for_the_classic_reload(&srv);
    assert_eq!(
        seen(&srv),
        format!("code=2 cwd={release_1} file=2"),
        "the first request after the reload\n{}",
        diagnostics(&srv)
    );
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A chdir() of a worker app stays for the next units of the same app instance. After a reload the next app instance starts in the entrypoint directory. A proc_open() child without a cwd starts in the PHP cwd on NTS and in the launch dir of the server on ZTS.
#[test]
fn a_worker_chdir_lasts_until_the_reload() {
    let srv = Spawn::http(Mode::Worker, fixture_path("lifecycle/cwd-worker.php"))
        .json_log()
        .spawn();
    // getcwd() reports the resolved path, and the scratch directory can be under a symlinked temp directory.
    let entrypoint_dir = std::fs::canonicalize(srv.dir.join("http"))
        .expect("canonicalize the entrypoint dir")
        .display()
        .to_string();
    // sys_get_temp_dir() reads TMPDIR, which the server inherits from the test.
    let temp_dir = std::fs::canonicalize(std::env::temp_dir())
        .expect("canonicalize the temp dir")
        .display()
        .to_string();
    let cwd = |path: &str| {
        let got = drain(submit(srv.addr, req(path)).expect("GET"));
        assert_eq!(got.status, 200, "GET {path}\n{}", diagnostics(&srv));
        got.body
    };

    assert_eq!(
        cwd("/"),
        entrypoint_dir,
        "the first unit runs in the entrypoint directory\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        cwd("/chdir"),
        temp_dir,
        "the chdir() takes effect in its unit\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        cwd("/"),
        temp_dir,
        "the chdir() of the previous unit still holds\n{}",
        diagnostics(&srv)
    );
    // proc_open() without a cwd starts the child in the cwd of the process. On ZTS the PHP cwd is per thread, and the process stays in its launch dir. On NTS the chdir() moves the process.
    let launch_dir = std::fs::canonicalize(&srv.dir)
        .expect("canonicalize the scratch dir")
        .display()
        .to_string();
    let child = cwd("/child");
    let want = match child.split_once(' ') {
        Some(("1", _)) => format!("1 {launch_dir}"),
        _ => format!("0 {temp_dir}"),
    };
    assert_eq!(
        child,
        want,
        "the directory of a proc_open() child\n{}",
        diagnostics(&srv)
    );

    signal(srv.pid(), libc::SIGUSR2);
    let log = srv.log_file();
    let rebooted = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        rebooted.is_some(),
        "the reload boots the app again\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        cwd("/"),
        entrypoint_dir,
        "the first unit after the reload runs in the entrypoint directory\n{}",
        diagnostics(&srv)
    );
}

/// PHP keeps its copy of a content type that it does not parse until the request ends. With USE_ZEND_ALLOC=0 each free of PHP goes to the C library, which aborts the worker on a second free of that copy.
#[test]
fn a_reload_after_a_json_post_keeps_the_classic_worker() {
    let srv = Spawn::http(Mode::Classic, fixture_path("classic/cwd.php"))
        .env("USE_ZEND_ALLOC", "0")
        .spawn();
    wait_serving(&srv, "http", 1);
    let posted = http_post(
        srv.addr,
        "/",
        b"application/json",
        b"{}",
        Duration::from_secs(10),
    )
    .map(|r| r.status)
    .ok();
    assert_eq!(posted, Some(200), "the POST\n{}", diagnostics(&srv));
    signal(srv.pid(), libc::SIGUSR2);
    wait_for_the_classic_reload(&srv);
    let got = http_get(srv.addr, "/", Duration::from_secs(10))
        .map(|r| r.status)
        .ok();
    assert_eq!(
        got,
        Some(200),
        "the first request after the reload\n{}",
        diagnostics(&srv)
    );
    slot_line(&srv, "handled=2 errors=0 ");
}

/// Starts release 1, serves a request, then reloads a file that fails at boot. Returns None without OPcache.
fn breakable_server(dir: &Path) -> Option<Server> {
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    // With file_update_protection = 0, OPcache caches the broken release.php at the first failed boot.
    let ini = format!(
        "{shared}\nopcache.enable = 1\nopcache.validate_timestamps = 0\nopcache.file_update_protection = 0\n"
    );
    stage_releases(dir, "breakable-worker.php");
    let entrypoint = dir.join("current/breakable-worker.php");
    let srv = Spawn::http_entrypoint(Mode::Worker, &entrypoint.display().to_string())
        .http_pool("max_boot_failures = 1")
        .php_ini(&ini)
        .json_log()
        .spawn();
    let first = seen(&srv);
    if first == "skip" {
        return None;
    }
    assert_eq!(first, "code=1", "before the deploy\n{}", diagnostics(&srv));
    std::fs::write(
        dir.join("releases/1/release.php"),
        "<?php return 'broken';\n",
    )
    .expect("write the broken release.php");
    signal(srv.pid(), libc::SIGUSR2);
    Some(srv)
}

/// Polls GET / until it answers 200 with code=2. Fails with the last answer when the time runs out.
fn wait_for_code_2(srv: &Server, within: Duration) {
    let mut last = String::new();
    let fixed = poll(within, || {
        last = match http_get(srv.addr, "/", Duration::from_secs(10)) {
            Ok(r) => format!("{} {}", r.status, String::from_utf8_lossy(&r.body)),
            Err(e) => format!("error {e}"),
        };
        (last == "200 code=2").then_some(())
    });
    assert!(
        fixed.is_some(),
        "after the fix and the reload\n want: 200 code=2\n last: {last}\n{}",
        diagnostics(srv)
    );
}

/// A reload that reaches a worker after a failed boot also empties OPcache. So after an in-place fix of a release whose boot fails, one reload runs the fix, also with opcache.validate_timestamps = 0. The reload starts no boot of the broken release.
#[test]
fn a_reload_after_a_failed_boot_runs_the_fixed_release() {
    let dir: PathBuf = scratch_dir();
    let Some(srv) = breakable_server(&dir) else {
        return;
    };
    let log = srv.log_file();
    wait_app_record(&log, "boot failed");
    let refused = http_get(srv.addr, "/", Duration::from_secs(10)).expect("request during retries");
    assert_eq!(refused.status, 503, "\n{}", diagnostics(&srv));
    // The wait after the fourth failed boot is 4 s, so the reload arrives before the next boot.
    let Some(failed) = poll(Duration::from_secs(20), || {
        let boots = app_contexts(&log, "boot failed").len();
        (boots >= 4).then_some(boots)
    }) else {
        panic!(
            "the broken release fails its boot 4 times\n{}",
            diagnostics(&srv)
        );
    };
    std::fs::write(dir.join("releases/1/release.php"), "<?php return '2';\n")
        .expect("write the fixed release.php");
    signal(srv.pid(), libc::SIGUSR2);
    let booted = poll(Duration::from_secs(10), || {
        app_contexts(&log, "booted")
            .iter()
            .filter_map(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .any(|ctx| ctx["code"] == "2")
            .then_some(())
    });
    assert!(
        booted.is_some(),
        "the fixed release boots after the reload\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        app_contexts(&log, "boot failed").len(),
        failed,
        "the reload starts no boot of the broken release\n{}",
        diagnostics(&srv)
    );
    wait_for_code_2(&srv, Duration::from_secs(10));
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A boot that fails for a cause outside OPcache, then a reload of a release that also changes cached files. Every worker then runs every file of the new release, also with opcache.validate_timestamps = 0.
#[test]
fn a_reload_after_a_failed_boot_runs_every_file_of_the_new_release() {
    let workers = if is_zts() { 2 } else { 1 };
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    let ini = format!("{shared}\nopcache.enable = 1\nopcache.validate_timestamps = 0\n");
    let dir: PathBuf = scratch_dir();
    stage_releases(&dir, "two-files-worker.php");
    let folder = dir.join("releases/1");
    write_old(&folder.join("extra.php"), "<?php return '1';\n");
    let entrypoint = dir.join("current/two-files-worker.php");
    let srv = Spawn::http_entrypoint(Mode::Worker, &entrypoint.display().to_string())
        .workers(workers)
        .php_ini(&ini)
        .json_log()
        .spawn();
    wait_serving(&srv, "http", workers);
    let before = serving_apps(&srv, workers, "release.php=1 extra.php=1");

    // Deploy step 1: extra.php is not there yet, so the boot of each worker fails.
    std::fs::remove_file(folder.join("extra.php")).expect("remove extra.php");
    signal(srv.pid(), libc::SIGUSR2);
    let log = srv.log_file();
    let failed = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "boot failed").len() >= workers).then_some(())
    });
    assert!(
        failed.is_some(),
        "the boot of each worker fails\n{}",
        diagnostics(&srv)
    );
    // Deploy step 2: release 2 of both files.
    std::fs::write(folder.join("release.php"), "<?php return '2';\n").expect("write release.php");
    std::fs::write(folder.join("extra.php"), "<?php return '2';\n").expect("write extra.php");
    signal(srv.pid(), libc::SIGUSR2);

    let after = serving_apps(&srv, workers, "release.php=2 extra.php=2");
    assert!(
        before.is_disjoint(&after),
        "both files must come from new apps"
    );
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A symlink to a missing release causes a 503 and an ERROR record that names the entrypoint. A reload after the fix serves the new release.
#[test]
fn an_entrypoint_that_does_not_resolve_answers_503_until_a_reload_after_the_fix() {
    let dir: PathBuf = scratch_dir();
    let [release_1, release_2] = stage_releases(&dir, "dispatcher.php");
    let entrypoint = dir.join("current").join("dispatcher.php");
    let srv = Spawn::http_entrypoint(Mode::Dispatcher, &entrypoint.display().to_string())
        .json_log()
        .spawn();
    wait_serving(&srv, "http", 1);
    assert_eq!(
        seen(&srv),
        format!("code=1 cwd={release_1} file=1"),
        "before the deploy\n{}",
        diagnostics(&srv)
    );
    let switch = |release: &str| {
        let next = dir.join("current.next");
        std::os::unix::fs::symlink(release, &next).expect("link current.next");
        std::fs::rename(&next, dir.join("current")).expect("switch current");
    };
    // The wire client decodes the chunked body of the fixture.
    let answer = || match submit(srv.addr, req("/")) {
        Ok(rx) => {
            let got = drain(rx);
            format!("{} {}", got.status, got.body)
        }
        Err(e) => format!("error {e}"),
    };

    switch("releases/missing");
    signal(srv.pid(), libc::SIGUSR2);
    let mut last = String::new();
    let refused = poll(Duration::from_secs(10), || {
        last = answer();
        last.starts_with("503").then_some(())
    });
    assert!(
        refused.is_some(),
        "a request after the broken deploy\n want: 503\n last: {last}\n{}",
        diagnostics(&srv)
    );
    let named = format!("resolving the entrypoint {}", entrypoint.display());
    assert!(
        records(&srv.log_file())
            .iter()
            .any(|r| r.level == tracing::Level::ERROR && r.message.starts_with(&named)),
        "an ERROR record {named:?}\n{}",
        diagnostics(&srv)
    );

    switch("releases/2");
    signal(srv.pid(), libc::SIGUSR2);
    let want = format!("200 code=2 cwd={release_2} file=2");
    let fixed = poll(Duration::from_secs(10), || {
        last = answer();
        (last == want).then_some(())
    });
    assert!(
        fixed.is_some(),
        "after the fix and the reload\n want: {want}\n last: {last}\n{}",
        diagnostics(&srv)
    );
    wait_serving(&srv, "http", 1);
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Submits the probes together. Each app holds its probe briefly, so distinct boot tokens identify the workers that served this batch.
fn serving_apps(srv: &Server, workers: usize, expected: &str) -> BTreeSet<String> {
    let mut last = Vec::new();
    poll(Duration::from_secs(20), || {
        let reads: Vec<_> = (0..workers)
            .map(|_| submit(srv.addr, req("/?probe=1")).expect("GET probe"))
            .collect();
        last = reads.into_iter().map(drain).collect();
        let mut apps = BTreeSet::new();
        for reply in &last {
            let (app, state) = reply.body.split_once(' ')?;
            if reply.status != 200 || state != expected || app.len() != 16 {
                return None;
            }
            apps.insert(app.to_owned());
        }
        (apps.len() == workers).then_some(apps)
    })
    .unwrap_or_else(|| {
        panic!(
            "expected {expected:?} from {workers} apps, got {last:?}\n{}",
            diagnostics(srv)
        )
    })
}

/// Holds one HTTP worker until the test creates `release-{name}`.
fn hold(srv: &Server, name: &str) -> (Receiver<Frame>, String) {
    let reply = submit(srv.addr, req(&format!("/?hold={name}"))).expect("submit held request");
    let ctx = wait_app_record(&srv.log_file(), &format!("holding {name}"));
    let ctx: serde_json::Value = serde_json::from_str(&ctx).expect("hold context");
    (
        reply,
        ctx["app"].as_str().expect("app boot token").to_owned(),
    )
}

const OPCACHE_ON: &str = "enabled=true pending=false";

/// The second reload arrives after one app ends but while another request still holds the old code. Both requests finish. Both new apps must have OPcache enabled with no restart pending.
#[test]
#[cfg_attr(not(php_zts), ignore = "needs ZTS PHP")]
fn two_reloads_during_long_requests_leave_opcache_on_in_every_worker() {
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/opcache-restart-worker.php"),
    )
    .workers(2)
    .php_ini(&format!("{shared}\nopcache.enable = 1\n"))
    .json_log()
    .spawn();
    wait_serving(&srv, "http", 2);
    if seen(&srv) == "skip" {
        return;
    }
    let (a, first) = hold(&srv, "a");
    let (b, second) = hold(&srv, "b");
    assert_ne!(first, second);
    signal(srv.pid(), libc::SIGUSR2);
    std::fs::write(srv.dir.join("http/release-a"), "").expect("release the first request");
    assert_eq!(drain(a).status, 200);
    wait_app_record(&srv.log_file(), &format!("ended {first}"));
    signal(srv.pid(), libc::SIGUSR2);
    std::fs::write(srv.dir.join("http/release-b"), "").expect("release the second request");
    assert_eq!(drain(b).status, 200);
    let after = serving_apps(&srv, 2, OPCACHE_ON);
    assert!(
        after.is_disjoint(&BTreeSet::from([first, second])),
        "both apps restarted: {after:?}"
    );
}

/// A scheduled OPcache restart waits until no app holds OPcache. Every app of every pool restarts, the restart runs, every http worker gets OPcache back, and no worker is killed, also with a request in flight on another worker. One WARN record names the reason of the restart.
#[test]
fn an_opcache_restart_runs_and_every_app_gets_opcache_back() {
    struct Case {
        name: &'static str,
        /// The request that makes OPcache schedule a restart.
        trigger: &'static str,
        /// The reason that the WARN record of the restart names.
        reason: &'static str,
        /// A grpc pool runs next to the http pool. Its apps hold OPcache too.
        grpc: bool,
    }
    let workers = if is_zts() { 2 } else { 1 };
    let cases = [
        Case {
            name: "opcache_reset()",
            trigger: "/?reset=1",
            reason: "opcache_reset()",
            grpc: false,
        },
        Case {
            name: "a full hash table with wasted memory",
            trigger: "/?fill=1",
            reason: "hash table full",
            grpc: false,
        },
        Case {
            name: "an http pool and a grpc pool",
            trigger: "/?reset=1",
            reason: "opcache_reset()",
            grpc: true,
        },
    ];
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    // 223 hash slots and a waste threshold of 84 KB (1 % of 8 MB) make OPcache schedule a restart by itself.
    // php-src counts the force timeout in whole seconds. With 2, a kill can start 1 to 2 s after the schedule, after the boots that follow the schedule and before the 3 s request ends.
    let ini = format!(
        "{shared}\nopcache.enable = 1\nopcache.memory_consumption = 8\nopcache.interned_strings_buffer = 1\n\
         opcache.max_accelerated_files = 200\nopcache.max_wasted_percentage = 1\n\
         opcache.force_restart_timeout = 2\nopcache.file_update_protection = 0\n\
         opcache.revalidate_freq = 0\n"
    );
    for case in cases {
        if case.grpc && workers == 1 {
            continue;
        }
        let mut spawn = Spawn::http(
            Mode::Worker,
            fixture_path("lifecycle/opcache-restart-worker.php"),
        )
        .workers(workers)
        .php_ini(&ini)
        .json_log();
        if case.grpc {
            spawn = spawn.with_grpc(fixture_path("grpc/echo-worker.php"));
        }
        let srv = spawn.spawn();
        wait_serving(&srv, "http", workers);
        if case.grpc {
            wait_serving(&srv, "grpc", workers);
        }
        if seen(&srv) == "skip" {
            return;
        }
        let before = serving_apps(&srv, workers, OPCACHE_ON);
        let held = (workers > 1).then(|| hold(&srv, "reader"));
        let trigger = drain(submit(srv.addr, req(case.trigger)).expect("submit the restart"));
        assert_eq!(trigger.status, 200, "{}: {trigger:?}", case.name);
        assert!(
            trigger.body.ends_with("pending=true"),
            "{}: {trigger:?}",
            case.name
        );
        if let Some((reply, app)) = held {
            assert!(
                !trigger.body.starts_with(&app),
                "the trigger runs on another worker"
            );
            // The request stays active past opcache.force_restart_timeout.
            std::thread::sleep(Duration::from_secs(3));
            std::fs::write(srv.dir.join("http/release-reader"), "").expect("release the reader");
            assert_eq!(
                drain(reply).status,
                200,
                "{}: the held request completes",
                case.name
            );
        }
        let after = serving_apps(&srv, workers, OPCACHE_ON);
        assert!(
            before.is_disjoint(&after),
            "{}: every HTTP app restarted",
            case.name
        );
        if case.grpc {
            wait_recycled(&srv, "grpc");
        }
        let restarts: Vec<String> = records(&srv.log_file())
            .into_iter()
            .filter(|r| {
                r.level == tracing::Level::WARN
                    && r.message.starts_with("OPcache scheduled a restart")
            })
            .map(|r| r.message)
            .collect();
        let want = format!("OPcache scheduled a restart ({})", case.reason);
        assert!(
            restarts.len() == 1 && restarts[0].starts_with(&want),
            "{}: one WARN record {want:?}\n got: {restarts:?}\n{}",
            case.name,
            diagnostics(&srv)
        );
    }
}

/// With opcache.restrict_api set, php-src refuses the OPcache API outside a unit with a warning. The host reads no OPcache state at the app end and at the boot, so the log gets no such warning.
#[test]
fn a_reload_with_a_restricted_opcache_api_logs_no_warning() {
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/boot-log-handle-worker.php"),
    )
    .php_ini(&format!(
        "{shared}\nopcache.restrict_api = \"/nonexistent\"\n"
    ))
    .json_log()
    .spawn();
    let log = srv.log_file();
    wait_app_record(&log, "booted");
    signal(srv.pid(), libc::SIGUSR2);
    let booted = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        booted.is_some(),
        "the app booted again\n{}",
        diagnostics(&srv)
    );
    let text = std::fs::read_to_string(&log).expect("read the server log");
    assert!(
        !text.contains("restrict_api"),
        "a record names restrict_api\n{}",
        diagnostics(&srv)
    );
}
