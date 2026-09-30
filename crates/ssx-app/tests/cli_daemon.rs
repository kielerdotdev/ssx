//! The `ssx` CLI against the real `ssx-app`: `daemon start|stop|status|restart`, autostart,
//! `record start|status|stop` through the daemon, `doctor` learning about the daemon, and the
//! `SSX_NO_DAEMON` switch. (`post-file --coalesce` batching is in `daemon_x11.rs`.)
#![cfg(unix)]

mod common;

use std::{path::Path, time::Duration};

use common::{TestEnv, app_bin, fixture, skip_unless, wait_until};
use ssx_core::ipc::{Request, Response};

fn with_app(env: &mut TestEnv) {
    env.set("SSX_APP", app_bin().display().to_string());
}

fn socket_gone(env: &TestEnv) -> bool {
    !env.run.join("ssx/ssx.sock").exists()
}

fn find_video(dir: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.extension().is_some_and(|e| e == "mp4" || e == "gif")
    })
}

#[test]
fn daemon_start_status_restart_stop_are_idempotent_and_scriptable() {
    let mut env = TestEnv::new();
    with_app(&mut env);
    // Without the daemon: status says so and exits non-zero (a script can test it).
    let none = env.ssx(&["daemon", "status"]);
    assert_ne!(none.code, 0, "{}{}", none.stdout, none.stderr);
    assert!(none.stdout.contains("not running") || none.stderr.contains("not running"));
    // ...and stop is a no-op that says so.
    let out = env.ssx(&["daemon", "stop"]);
    assert!(out.stdout.contains("not running") || out.stderr.contains("not running"), "{out:?}");

    // start: waits until it answers.
    env.ssx(&["daemon", "start"]).ok();
    let st = env.ssx(&["daemon", "status", "--json"]).ok().json();
    let pid = st["pid"].as_u64().expect("pid");
    assert!(pid > 0);
    // A second start reports it is already running and keeps the same process.
    let again = env.ssx(&["daemon", "start"]).ok();
    assert!(
        again.stdout.contains("already") || again.stderr.contains("already"),
        "{}{}",
        again.stdout,
        again.stderr
    );
    assert_eq!(env.ssx(&["daemon", "status", "--json"]).ok().json()["pid"].as_u64(), Some(pid));
    let text = env.ssx(&["daemon", "status"]).ok();
    assert!(text.stdout.contains("running"), "{}", text.stdout);

    // doctor sees it.
    let d = env.ssx(&["doctor", "--json"]).json();
    assert_eq!(d["daemon"]["running"], true, "{}", d["daemon"]);
    assert_eq!(d["daemon"]["version"], env!("CARGO_PKG_VERSION"), "{}", d["daemon"]);

    // restart replaces the process.
    env.ssx(&["daemon", "restart"]).ok();
    let new_pid = env.ssx(&["daemon", "status", "--json"]).ok().json()["pid"].as_u64().unwrap();
    assert_ne!(new_pid, pid, "restart starts a new process");

    // stop: the process is gone and the socket removed.
    env.ssx(&["daemon", "stop"]).ok();
    assert!(
        wait_until(Duration::from_secs(10), || socket_gone(&env).then_some(())).is_some(),
        "the socket is removed"
    );
    assert_ne!(env.ssx(&["daemon", "status"]).code, 0);
    let log = env.daemon_log();
    assert!(log.contains("bye"), "a clean shutdown is logged\n{log}");
}

#[test]
fn a_missing_app_is_a_clear_error_not_a_hang() {
    let mut env = TestEnv::new();
    env.set("SSX_APP", "none");
    let out = env.ssx(&["daemon", "start"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("SSX_APP"), "{}", out.stderr);
}

#[test]
fn autostart_enable_disable_writes_only_our_own_file() {
    let mut env = TestEnv::new();
    with_app(&mut env);
    env.set("XDG_SESSION_TYPE", "x11");
    let file = env.home.join(".config/autostart/ssx.desktop");
    assert!(env.ssx(&["daemon", "autostart", "status"]).ok().stdout.contains("disabled"));
    let out = env.ssx(&["daemon", "autostart", "enable"]).ok();
    assert!(out.stdout.contains("start at login"), "{}", out.stdout);
    let text = std::fs::read_to_string(&file).expect("the autostart entry");
    assert!(text.contains("Exec=") && text.contains("ssx-app"), "{text}");
    assert!(env.ssx(&["daemon", "autostart", "status"]).ok().stdout.contains("enabled"));
    // Idempotent.
    env.ssx(&["daemon", "autostart", "enable"]).ok();
    env.ssx(&["daemon", "autostart", "disable"]).ok();
    assert!(!file.exists());
    // A file of the user's own is never overwritten or removed.
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "[Desktop Entry]\nName=mine\nExec=/bin/true\n").unwrap();
    let out = env.ssx(&["daemon", "autostart", "enable"]);
    assert_ne!(out.code, 0, "{}", out.stdout);
    env.ssx(&["daemon", "autostart", "disable"]);
    assert!(std::fs::read_to_string(&file).unwrap().contains("Name=mine"));
}

#[test]
fn record_start_status_stop_through_the_daemon_leaves_a_file() {
    if skip_unless(&["ffprobe"]) {
        return;
    }
    let Some(mut f) = fixture() else { return };
    with_app(&mut f.env);
    f.env.ssx(&["daemon", "start"]).ok();
    let idle = f.env.ssx(&["record", "status"]).ok();
    assert!(idle.stdout.to_lowercase().contains("not recording"), "{}", idle.stdout);

    f.env.ssx(&["record", "start", "--rect", "0,0,320,240"]).ok();
    let st = wait_until(Duration::from_secs(10), || {
        let out = f.env.ssx(&["record", "status", "--json"]);
        (out.code == 0 && out.json()["active"] == true).then(|| out.json())
    })
    .expect("the recording is active");
    assert!(st["run_id"].as_u64().is_some());
    // A second start is refused with a message, exit code non-zero.
    let second = f.env.ssx(&["record", "start", "--rect", "0,0,320,240"]);
    assert_ne!(second.code, 0);
    std::thread::sleep(Duration::from_millis(1500));
    f.env.ssx(&["record", "stop"]).ok();
    let file = wait_until(Duration::from_secs(30), || find_video(&f.save)).unwrap_or_else(|| {
        panic!("no recording appeared in {}\n{}", f.save.display(), f.env.daemon_log())
    });
    // Finalised: the run is over (the file was still growing until then).
    let idle = wait_until(Duration::from_secs(30), || {
        let Response::Status(s) = f.env.request(&Request::Status) else { return None };
        s.active_runs.is_empty().then_some(())
    });
    assert!(idle.is_some(), "the recording run finishes\n{}", f.env.daemon_log());
    assert!(std::fs::metadata(&file).unwrap().len() > 1000, "{}", file.display());
    // Stopping when nothing records is an error a script can see.
    assert_ne!(f.env.ssx(&["record", "stop"]).code, 0);
    assert_eq!(f.env.request(&Request::Quit), Response::Ok);
}

#[test]
fn ssx_run_forwards_to_the_daemon_and_falls_back_without_it() {
    let Some(mut f) = fixture() else { return };
    with_app(&mut f.env);
    // No daemon: runs in-process.
    let out = f.env.ssx(&["run", "local"]).ok();
    assert!(Path::new(out.stdout.trim()).is_file(), "{}", out.stdout);
    assert!(!f.env.daemon_log().contains("run started"), "no daemon involved");
    // With a daemon the same command is served by it (its log records the run).
    f.env.ssx(&["daemon", "start"]).ok();
    let out = f.env.ssx(&["run", "local"]).ok();
    assert!(Path::new(out.stdout.trim()).is_file(), "{}", out.stdout);
    let log = f.env.daemon_log();
    assert!(log.contains("run started") && log.contains("Shot only"), "{log}");
    f.env.ssx(&["daemon", "stop"]).ok();
}
