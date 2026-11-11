use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tests::server_log::{app_contexts, wait_app_record};
use tests::wire::submit;
use tests::{drain, poll, req};

/// USR2 restarts the app inside every worker: the listener and the worker processes stay, and no request fails.
#[test]
fn usr2_reload_restarts_the_apps_while_serving() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/boot-log-worker.php"),
    )
    .processes(3)
    .json_log()
    .spawn();
    let mut pids = wait_workers(&srv, Duration::from_secs(20), "3 workers", |p| p.len() == 3);
    pids.sort();
    let storm = storm(srv.addr, 8);
    std::thread::sleep(Duration::from_millis(500));
    signal(srv.pid(), libc::SIGUSR2);
    let log = srv.log_file();
    let booted = poll(Duration::from_secs(20), || {
        (app_contexts(&log, "booted").len() >= 6).then_some(())
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
    let mut after = worker_pids(srv.pid());
    after.sort();
    assert_eq!(
        after,
        pids,
        "the worker processes stay\n{}",
        diagnostics(&srv)
    );
}

/// SIGUSR2 to a worker that waits for work restarts its app without a request, in the same process.
#[test]
fn usr2_restarts_an_idle_app() {
    let srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/boot-log-worker.php"),
    )
    .json_log()
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    let log = srv.log_file();
    wait_app_record(&log, "booted");
    signal(pid, libc::SIGUSR2);
    let booted = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "booted").len() >= 2).then_some(())
    });
    assert!(
        booted.is_some(),
        "the idle app booted again\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        worker_pids(srv.pid()),
        vec![pid],
        "the worker process stays\n{}",
        diagnostics(&srv)
    );
}

/// Classic mode runs every request in a new request cycle, so a reload has nothing to restart and the worker keeps serving.
#[test]
fn a_reload_leaves_a_classic_pool_alone() {
    let srv = Spawn::http(Mode::Classic, fixture_path("classic/cwd.php")).spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    signal(srv.pid(), libc::SIGUSR2);
    // Nothing must happen, so the test waits a fixed time before it looks.
    std::thread::sleep(Duration::from_secs(1));
    let code = http_get(srv.addr, "/", Duration::from_secs(10))
        .expect("GET /")
        .status;
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        worker_pids(srv.pid()),
        vec![pid],
        "the worker process stays\n{}",
        diagnostics(&srv)
    );
}

/// A reload into code that fails its boot makes the worker unhealthy. The pool booted before the reload, so the master replaces the worker and keeps the server running.
#[test]
fn a_reload_into_a_failing_boot_keeps_the_server_running() {
    let mut srv = Spawn::http(
        Mode::Dispatcher,
        fixture_path("lifecycle/breakable-boot-worker.php"),
    )
    .json_log()
    .spawn();
    let pid = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1)[0];
    wait_app_record(&srv.log_file(), "booted");
    std::fs::write(srv.dir.join("http/broken.flag"), "").expect("break the boot");
    signal(srv.pid(), libc::SIGUSR2);
    // A request during a failed boot gets 503 and starts the next boot at once, so a few requests make the worker unhealthy.
    for _ in 0..5 {
        let _ = http_get(srv.addr, "/", Duration::from_secs(10));
    }
    assert!(
        wait_log_contains(&srv, "flagged unhealthy", Duration::from_secs(20)),
        "the worker stays healthy\n{}",
        diagnostics(&srv)
    );
    let exit = srv.wait_exit(Duration::from_secs(3));
    assert_eq!(
        exit,
        None,
        "the master keeps running\n{}",
        diagnostics(&srv)
    );
    wait_workers(&srv, Duration::from_secs(20), "a replacement worker", |p| {
        p.len() == 1 && p[0] != pid
    });
}

struct DeployCase {
    name: &'static str,
    mode: Mode,
    /// The fixture in `deploy/` that each release folder gets as its entrypoint.
    entrypoint: &'static str,
    /// opcache.validate_timestamps: 0 is the usual production value.
    validate_timestamps: u8,
}

/// The body of a `deploy/` fixture: the release of the code, the working directory and the content of release.txt.
fn seen(srv: &Server) -> String {
    let got = drain(submit(srv.addr, req("/")).expect("GET /"));
    assert_eq!(got.status, 200, "\n{}", diagnostics(srv));
    got.body
}

/// Writes `contents` to `path` with a modification time of one hour ago: OPcache caches a file only when it is older than opcache.file_update_protection (2 s), as the files of a real deploy are.
fn write_old(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(SystemTime::now() - Duration::from_secs(3600)))
        .unwrap_or_else(|e| panic!("set the time of {}: {e}", path.display()));
}

/// `releases/1` and `releases/2` under `dir`, each with the entrypoint, release.php and release.txt, and `current` pointing at release 1. Returns the real path of each release folder: getcwd() reports it.
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

/// A reload after a symlink deploy moves the pool to release 2: its code, its working directory, its relative file reads and the root of sendFile().
#[test]
fn a_reload_after_a_deploy_runs_the_new_release() {
    let cases = [
        DeployCase {
            name: "worker mode, symlink switch",
            mode: Mode::Worker,
            entrypoint: "worker.php",
            validate_timestamps: 1,
        },
        DeployCase {
            name: "dispatcher mode, symlink switch, sendFile() under __DIR__",
            mode: Mode::Dispatcher,
            entrypoint: "dispatcher.php",
            validate_timestamps: 1,
        },
        DeployCase {
            name: "classic mode, symlink switch",
            mode: Mode::Classic,
            entrypoint: "classic.php",
            validate_timestamps: 1,
        },
        DeployCase {
            name: "worker mode, symlink switch, opcache.validate_timestamps = 0",
            mode: Mode::Worker,
            entrypoint: "worker.php",
            validate_timestamps: 0,
        },
        DeployCase {
            name: "classic mode, symlink switch, opcache.validate_timestamps = 0",
            mode: Mode::Classic,
            entrypoint: "classic.php",
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
            .spawn();

        assert_eq!(
            seen(&srv),
            format!("code=1 cwd={release_1} file=1"),
            "{}: before the deploy\n{}",
            case.name,
            diagnostics(&srv)
        );
        let next = dir.join("current.next");
        std::os::unix::fs::symlink("releases/2", &next).expect("link current.next");
        std::fs::rename(&next, dir.join("current")).expect("switch current");
        signal(srv.pid(), libc::SIGUSR2);

        let want = format!("code=2 cwd={release_2} file=2");
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
