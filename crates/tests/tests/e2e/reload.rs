use crate::harness::*;
use rapira_sapi::plugin::Mode;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tests::server_log::{app_contexts, wait_app_record};
use tests::wire::submit;
use tests::{StatusBody, drain, poll, req};

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
    /// A reload right before the deploy, so the deploy reload comes less than opcache.revalidate_freq (2 s) after an app boot.
    reload_first: bool,
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

/// A reload after a deploy moves the pool to release 2: its code, its working directory, its relative file reads and the root of sendFile().
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
    // The master counts the reload before it signals the pool, so this record of the worker means that the count is visible.
    let log = srv.log_file();
    let handled = poll(Duration::from_secs(10), || {
        std::fs::read_to_string(&log)
            .is_ok_and(|text| text.contains("reload requested; resolving the entrypoint again"))
            .then_some(())
    });
    assert!(
        handled.is_some(),
        "the worker handles the reload\n{}",
        diagnostics(&srv)
    );
    assert_eq!(
        seen(&srv),
        format!("code=2 cwd={release_1} file=2"),
        "the first request after the reload\n{}",
        diagnostics(&srv)
    );
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A one-worker server on release 1 of `deploy/breakable-worker.php`, with opcache.validate_timestamps = 0, after an in-place deploy of a release.php whose boot fails and its reload. Returns None without OPcache.
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
    let failed = poll(Duration::from_secs(10), || {
        (!app_contexts(&log, "boot failed").is_empty()).then_some(())
    });
    assert!(
        failed.is_some(),
        "the broken release fails its boot\n{}",
        diagnostics(&srv)
    );
    std::fs::write(dir.join("releases/1/release.php"), "<?php return '2';\n")
        .expect("write the fixed release.php");
    signal(srv.pid(), libc::SIGUSR2);
    // No request arrives before the fixed release has booted: a request that reaches the worker before the reload starts one more boot of the broken release.
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
        1,
        "the reload starts no boot of the broken release\n{}",
        diagnostics(&srv)
    );
    wait_for_code_2(&srv, Duration::from_secs(10));
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A reload that finds no live worker process. Under steady traffic a worker whose boot fails ends as unhealthy, and the master forks the next one after a growing delay. That worker has no reload entry: its boot reads the reload count and makes the reset call before the app starts, and it runs the fixed release, also with opcache.validate_timestamps = 0.
#[test]
fn a_reload_with_no_live_worker_runs_the_fixed_release() {
    let dir: PathBuf = scratch_dir();
    let Some(srv) = breakable_server(&dir) else {
        return;
    };
    // Each shed request starts the next boot, so 5 failed boots soon end the process.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let traffic = {
        let stop = std::sync::Arc::clone(&stop);
        let addr = srv.addr;
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = http_get(addr, "/", Duration::from_secs(1));
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let gone = poll(Duration::from_secs(30), || {
        worker_pids(srv.pid()).is_empty().then_some(())
    });
    // A respawn can come before the reload. The reload then reaches a live worker, and the test checks that sequence.
    std::fs::write(dir.join("releases/1/release.php"), "<?php return '2';\n")
        .expect("write the fixed release.php");
    signal(srv.pid(), libc::SIGUSR2);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    traffic.join().expect("the traffic thread");
    assert!(
        gone.is_some(),
        "the worker process ends as unhealthy\n{}",
        diagnostics(&srv)
    );
    // The respawn delay of the master grows to at most 25.6 s.
    wait_for_code_2(&srv, Duration::from_secs(40));
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A boot that fails for a cause outside OPcache, then a reload of a release that also changes cached files. Every worker then runs every file of the new release, also with opcache.validate_timestamps = 0.
#[test]
fn a_reload_after_a_failed_boot_runs_every_file_of_the_new_release() {
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    let ini = format!("{shared}\nopcache.enable = 1\nopcache.validate_timestamps = 0\n");
    let dir: PathBuf = scratch_dir();
    stage_releases(&dir, "two-files-worker.php");
    let folder = dir.join("releases/1");
    write_old(&folder.join("extra.php"), "<?php return '1';\n");
    let entrypoint = dir.join("current/two-files-worker.php");
    let srv = Spawn::http_entrypoint(Mode::Worker, &entrypoint.display().to_string())
        .processes(2)
        .php_ini(&ini)
        .json_log()
        .spawn();
    wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    let [mut a, mut b] = pin_two_workers(&srv).expect("the fixture answers without OPcache");

    // Deploy step 1: extra.php is not there yet, so the boot of each worker fails.
    std::fs::remove_file(folder.join("extra.php")).expect("remove extra.php");
    signal(srv.pid(), libc::SIGUSR2);
    let log = srv.log_file();
    let failed = poll(Duration::from_secs(10), || {
        (app_contexts(&log, "boot failed").len() >= 2).then_some(())
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

    let mut last_a = String::new();
    let mut last_b = String::new();
    let new_release = poll(Duration::from_secs(10), || {
        last_a = get_on(&mut a, "/").body;
        last_b = get_on(&mut b, "/").body;
        let want = "release.php=2 extra.php=2";
        (last_a.ends_with(want) && last_b.ends_with(want)).then_some(())
    });
    assert!(
        new_release.is_some(),
        "every worker runs release 2 of both files\n last A: {last_a}\n last B: {last_b}\n{}",
        diagnostics(&srv)
    );
    // The stop of the server waits for open connections.
    drop([a, b]);
    drop(srv);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The pid at the start of an answer `pid=<pid> ...`.
fn pid_of(body: &str) -> u32 {
    body.strip_prefix("pid=")
        .and_then(|r| r.split(' ').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("unexpected body {body:?}"))
}

/// One kept connection to each of the 2 workers of `srv`, found by the pid at the start of each answer. None when the fixture answers `skip`.
fn pin_two_workers(srv: &Server) -> Option<[Conn; 2]> {
    let mut pids: Vec<u32> = Vec::new();
    let mut pinned: Vec<Conn> = Vec::new();
    for _ in 0..200 {
        let mut c = Conn::open(srv.addr, Duration::from_secs(10)).expect("connect");
        let body = get_on(&mut c, "/").body;
        if body == "skip" {
            return None;
        }
        let pid = pid_of(&body);
        if !pids.contains(&pid) {
            pids.push(pid);
            pinned.push(c);
        }
        if pinned.len() == 2 {
            break;
        }
    }
    assert_eq!(
        pinned.len(),
        2,
        "a connection to each worker\n{}",
        diagnostics(srv)
    );
    pinned.try_into().ok()
}

/// One GET on a kept connection: the status and the body.
fn get_on(c: &mut Conn, path: &str) -> StatusBody<String> {
    let t = Duration::from_secs(30);
    c.send(format!("GET {path} HTTP/1.1\r\nHost: e2e\r\n\r\n").as_bytes())
        .expect("send the request");
    let head = c.read_head(t).expect("read the head");
    let len = head
        .fields
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .expect("content-length");
    let body = c.read_n(len, t).expect("read the body");
    StatusBody {
        status: head.status,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// A second reload while both workers serve long requests. An app can end with its reload entry still in the queue, so the next app of that worker takes the entry after the OPcache restart of the first reload has run. One reset per reload keeps that app from scheduling a restart that no app leaves for.
#[test]
fn two_reloads_during_long_requests_leave_opcache_on_in_every_worker() {
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    let srv = Spawn::http(
        Mode::Worker,
        fixture_path("lifecycle/opcache-restart-worker.php"),
    )
    .processes(2)
    .php_ini(&format!("{shared}\nopcache.enable = 1\n"))
    .spawn();
    wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    // A connection stays with the worker that accepted it, so each long request goes to its own worker.
    let Some([mut a, mut b]) = pin_two_workers(&srv) else {
        return;
    };

    let statuses = std::thread::scope(|scope| {
        let held_a = scope.spawn(|| get_on(&mut a, "/?sleep=3").status);
        std::thread::sleep(Duration::from_millis(300));
        signal(srv.pid(), libc::SIGUSR2);
        std::thread::sleep(Duration::from_millis(500));
        let held_b = scope.spawn(|| get_on(&mut b, "/?sleep=5").status);
        std::thread::sleep(Duration::from_millis(1700));
        signal(srv.pid(), libc::SIGUSR2);
        [
            held_a.join().expect("the request thread of worker A"),
            held_b.join().expect("the request thread of worker B"),
        ]
    });
    assert_eq!(
        statuses,
        [200, 200],
        "both long requests are served\n{}",
        diagnostics(&srv)
    );

    let mut last_a = String::new();
    let mut last_b = String::new();
    let all_on = poll(Duration::from_secs(20), || {
        last_a = get_on(&mut a, "/").body;
        last_b = get_on(&mut b, "/").body;
        (opcache_state(&last_a).on && opcache_state(&last_b).on).then_some(())
    });
    assert!(
        all_on.is_some(),
        "both workers have OPcache on and no restart pending\n last A: {last_a}\n last B: {last_b}\n{}",
        diagnostics(&srv)
    );
}

/// One answer of `opcache-restart-worker.php`.
struct OpcacheState {
    pid: u32,
    /// OPcache is on and no restart is pending.
    on: bool,
}

fn opcache_state(body: &str) -> OpcacheState {
    OpcacheState {
        pid: pid_of(body),
        on: body.ends_with("enabled=true pending=false"),
    }
}

/// A scheduled OPcache restart waits until no app holds OPcache. Every app of every pool restarts, the restart runs, every http worker gets OPcache back, and no worker is killed, also with a request in flight.
#[test]
fn an_opcache_restart_runs_and_every_app_gets_opcache_back() {
    struct Case {
        name: &'static str,
        /// The request that makes OPcache schedule a restart.
        trigger: &'static str,
        /// A grpc pool runs next to the http pool. Its apps hold OPcache too.
        grpc: bool,
    }
    const HTTP_WORKERS: usize = 2;
    let cases = [
        Case {
            name: "opcache_reset()",
            trigger: "/?reset=1",
            grpc: false,
        },
        Case {
            name: "a full hash table with wasted memory",
            trigger: "/?fill=1",
            grpc: false,
        },
        Case {
            name: "an http pool and a grpc pool",
            trigger: "/?reset=1",
            grpc: true,
        },
    ];
    let shared = std::fs::read_to_string(tests::fixture("ini/shared/php.ini"))
        .expect("read the shared php.ini");
    // 223 hash slots and a waste threshold of 84 KB (1 % of 8 MB) make OPcache schedule a restart by itself.
    // php-src counts the force timeout in whole seconds: with 2, a kill can start 1 to 2 s after the schedule, after the boots that follow the schedule and before the 3 s request ends.
    let ini = format!(
        "{shared}\nopcache.memory_consumption = 8\nopcache.interned_strings_buffer = 1\n\
         opcache.max_accelerated_files = 200\nopcache.max_wasted_percentage = 1\n\
         opcache.force_restart_timeout = 2\nopcache.file_update_protection = 0\n\
         opcache.revalidate_freq = 0\n"
    );
    for case in cases {
        let mut spawn = Spawn::http(
            Mode::Worker,
            fixture_path("lifecycle/opcache-restart-worker.php"),
        )
        .processes(HTTP_WORKERS)
        .php_ini(&ini);
        if case.grpc {
            spawn = spawn.with_grpc(fixture_path("grpc/echo-worker.php"));
        }
        let srv = spawn.spawn();
        let workers = if case.grpc {
            2 * HTTP_WORKERS
        } else {
            HTTP_WORKERS
        };
        let pids = wait_workers(&srv, Duration::from_secs(20), "every worker", |p| {
            p.len() == workers
        });
        let first = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
        if first.body == b"skip" {
            return;
        }
        let addr = srv.addr;
        let held = std::thread::spawn(move || {
            http_get(addr, "/?sleep=3", Duration::from_secs(30)).map(|r| r.status)
        });
        std::thread::sleep(Duration::from_millis(500));
        let trigger = http_get(srv.addr, case.trigger, Duration::from_secs(30)).expect("trigger");
        assert!(
            String::from_utf8_lossy(&trigger.body).ends_with("pending=true"),
            "{}: OPcache scheduled a restart: {:?}\n{}",
            case.name,
            String::from_utf8_lossy(&trigger.body),
            diagnostics(&srv)
        );
        let all_on = poll(Duration::from_secs(20), || {
            let states = fan_out(addr, "/", 4, 2, opcache_state);
            let mut on: Vec<u32> = states.iter().filter(|s| s.on).map(|s| s.pid).collect();
            on.sort_unstable();
            on.dedup();
            (states.iter().all(|s| s.on) && on.len() == HTTP_WORKERS).then_some(())
        });
        assert!(
            all_on.is_some(),
            "{}: every worker has OPcache on and no restart is pending\n{}",
            case.name,
            diagnostics(&srv)
        );
        let held = held.join().expect("the held request thread");
        assert_eq!(
            held.ok(),
            Some(200),
            "{}: the request in flight is served\n{}",
            case.name,
            diagnostics(&srv)
        );
        assert_eq!(
            worker_pids(srv.pid()),
            pids,
            "{}: no worker is killed\n{}",
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
