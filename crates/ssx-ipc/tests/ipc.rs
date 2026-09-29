//! Two-"process" semantics using threads and real sockets/pipes in temp dirs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use ssx_ipc::{
    Acquired, Client, ClientConfig, Error, Instance, Location, Options, PeerPolicy, ServeHandle,
    ServerConfig, TimeoutKind,
};

fn loc() -> (tempfile::TempDir, Location) {
    let tmp = tempfile::Builder::new().prefix("ssx").tempdir().expect("tempdir");
    let l = Location::in_dir(tmp.path().join("run"));
    (tmp, l)
}

fn primary(l: &Location, opts: Options) -> ssx_ipc::Server {
    match Instance::acquire_with(l, "app", opts).expect("acquire") {
        Acquired::Primary(s) => s,
        Acquired::Secondary(_) => panic!("expected to become primary"),
    }
}

fn echo(l: &Location) -> ServeHandle {
    primary(l, Options::default()).serve(|line| format!("echo:{line}")).expect("serve")
}

fn fast_client() -> ClientConfig {
    ClientConfig {
        connect_timeout: Duration::from_millis(300),
        request_timeout: Duration::from_millis(400),
        startup_timeout: Duration::from_millis(500),
        ..ClientConfig::default()
    }
}

#[test]
fn primary_then_secondary_roundtrip() {
    let (_t, l) = loc();
    let _srv = echo(&l);
    let Acquired::Secondary(client) = Instance::acquire_in(&l, "app").expect("acquire") else {
        panic!("second acquire must be secondary");
    };
    assert_eq!(
        client.request(r#"{"cmd":"post-file","paths":["/a b/ü"]}"#).expect("req"),
        r#"echo:{"cmd":"post-file","paths":["/a b/ü"]}"#
    );
    assert!(client.is_listening().expect("probe"));
}

#[test]
fn different_app_ids_do_not_conflict() {
    let (_t, l) = loc();
    let _a = echo(&l);
    assert!(matches!(Instance::acquire_in(&l, "other").expect("acquire"), Acquired::Primary(_)));
}

#[test]
fn concurrent_acquire_yields_exactly_one_primary() {
    const N: usize = 24;
    let (_t, l) = loc();
    let barrier = Arc::new(Barrier::new(N));
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let (l, b) = (l.clone(), Arc::clone(&barrier));
            thread::spawn(move || {
                b.wait();
                Instance::acquire_in(&l, "app").expect("acquire")
            })
        })
        .collect();
    let results: Vec<Acquired> = handles.into_iter().map(|h| h.join().expect("join")).collect();
    let primaries = results.iter().filter(|r| matches!(r, Acquired::Primary(_))).count();
    assert_eq!(primaries, 1, "exactly one primary among {N}");
    // Every secondary can reach the primary once it serves.
    let server = results
        .into_iter()
        .find_map(|r| if let Acquired::Primary(s) = r { Some(s) } else { None })
        .expect("primary");
    let _h = server.serve(|l| l).expect("serve");
    let Acquired::Secondary(c) = Instance::acquire_in(&l, "app").expect("acquire") else {
        panic!("secondary expected")
    };
    assert_eq!(c.request("ping").expect("ping"), "ping");
}

#[test]
fn repeated_acquire_after_drop_succeeds() {
    let (_t, l) = loc();
    for _ in 0..5 {
        let s = primary(&l, Options::default());
        drop(s);
    }
}

#[test]
fn request_with_nobody_listening_is_not_running() {
    let (_t, l) = loc();
    let c = Client::at(&l, "app").expect("client").with_config(fast_client());
    let start = Instant::now();
    assert!(matches!(c.request("x"), Err(Error::NotRunning)));
    assert!(start.elapsed() >= Duration::from_millis(250), "must have retried");
}

#[test]
fn request_retries_until_server_appears() {
    let (_t, l) = loc();
    let l2 = l.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        echo(&l2)
    });
    let c = Client::at(&l, "app").expect("client");
    assert_eq!(c.request("late").expect("request"), "echo:late");
    drop(t.join().expect("join"));
}

#[test]
fn newline_in_request_is_rejected_locally() {
    let (_t, l) = loc();
    let _s = echo(&l);
    let c = Client::at(&l, "app").expect("client");
    assert!(matches!(c.request("a\nb"), Err(Error::InvalidLine(_))));
}

#[test]
fn oversize_request_is_rejected_by_server() {
    let (_t, l) = loc();
    let opts = Options {
        server: ServerConfig { max_line: 1024, ..ServerConfig::default() },
        ..Options::default()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&calls);
    let _s = primary(&l, opts)
        .serve(move |l| {
            c2.fetch_add(1, Ordering::SeqCst);
            l
        })
        .expect("serve");
    let c = Client::at(&l, "app").expect("client").with_config(fast_client());
    let err = c.request(&"x".repeat(4096)).expect_err("must fail");
    assert!(matches!(err, Error::ConnectionClosed | Error::Io { .. }), "{err}");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "handler must never see the oversize line");
    // The server survives and still serves normal requests.
    assert_eq!(c.request("ok").expect("ok"), "ok");
    assert_eq!(c.request(&"y".repeat(1024)).expect("exact max").len(), 1024);
}

#[test]
fn oversize_response_is_rejected_by_client() {
    let (_t, l) = loc();
    let _s = primary(&l, Options::default()).serve(|_| "z".repeat(5000)).expect("serve");
    let cfg = ClientConfig { max_line: 1000, ..fast_client() };
    let c = Client::at(&l, "app").expect("client").with_config(cfg);
    assert!(matches!(c.request("x"), Err(Error::LineTooLong { max: 1000 })));
}

#[test]
fn client_times_out_on_unresponsive_server() {
    let (_t, l) = loc();
    let _s = primary(&l, Options::default())
        .serve(|l| {
            thread::sleep(Duration::from_secs(2));
            l
        })
        .expect("serve");
    let c = Client::at(&l, "app").expect("client").with_config(fast_client());
    let start = Instant::now();
    let err = c.request("x").expect_err("timeout");
    assert!(matches!(err, Error::Timeout { kind: TimeoutKind::Read, .. }), "{err}");
    assert!(start.elapsed() < Duration::from_millis(1500));
}

#[test]
fn shutdown_removes_socket_and_frees_the_instance() {
    let (_t, l) = loc();
    let srv = primary(&l, Options::default());
    #[cfg(unix)]
    let path = srv.socket_path().to_path_buf();
    #[cfg(unix)]
    assert!(path.exists());
    let h = srv.serve(|l| l).expect("serve");
    h.shutdown();
    #[cfg(unix)]
    assert!(!path.exists(), "socket must be removed on shutdown");
    // A new instance can take over immediately.
    assert!(matches!(Instance::acquire_in(&l, "app").expect("acquire"), Acquired::Primary(_)));
}

#[test]
fn dropping_server_removes_socket() {
    let (_t, l) = loc();
    let srv = primary(&l, Options::default());
    #[cfg(unix)]
    let path = srv.socket_path().to_path_buf();
    drop(srv);
    #[cfg(unix)]
    assert!(!path.exists());
}

#[test]
fn incoming_iterator_ends_on_shutdown() {
    let (_t, l) = loc();
    let srv = primary(&l, Options::default());
    let handle = srv.shutdown_handle();
    let t = thread::spawn(move || {
        let mut seen = Vec::new();
        for conn in srv.incoming() {
            let mut conn = conn.expect("conn");
            while let Some(line) = conn.read_line().expect("line") {
                conn.write_line(&format!("got {line}")).expect("write");
                seen.push(line);
            }
        }
        seen
    });
    let c = Client::at(&l, "app").expect("client");
    assert_eq!(c.request("one").expect("one"), "got one");
    assert_eq!(c.request("two").expect("two"), "got two");
    handle.shutdown();
    handle.shutdown(); // idempotent
    assert_eq!(t.join().expect("join"), ["one", "two"]);
}

#[test]
fn peer_policy_can_veto_connections() {
    let (_t, l) = loc();
    let opts = Options {
        server: ServerConfig {
            peer_policy: PeerPolicy::Also(Arc::new(|_| false)),
            ..ServerConfig::default()
        },
        ..Options::default()
    };
    let _s = primary(&l, opts).serve(|l| l).expect("serve");
    let c = Client::at(&l, "app").expect("client").with_config(fast_client());
    assert!(c.request("x").is_err(), "vetoed peer must not get a response");
}

#[test]
fn peer_info_reports_same_user() {
    let (_t, l) = loc();
    let srv = primary(&l, Options::default());
    let t = thread::spawn(move || {
        let conn = srv.incoming().next().expect("one").expect("conn");
        *conn.peer()
    });
    let c = Client::at(&l, "app").expect("client").with_config(fast_client());
    drop(c.request("x")); // server thread never answers; only peer info matters
    let peer = t.join().expect("join");
    #[cfg(unix)]
    assert_eq!(peer.uid, Some(rustix::process::geteuid().as_raw()));
    assert_eq!(peer.pid, Some(std::process::id()));
}

#[test]
fn send_or_spawn_launches_app_once_and_delivers() {
    let (_t, l) = loc();
    let spawned = Arc::new(AtomicUsize::new(0));
    let (sp, l2) = (Arc::clone(&spawned), l.clone());
    let keep: Arc<std::sync::Mutex<Vec<ServeHandle>>> = Arc::default();
    let keep2 = Arc::clone(&keep);
    let c = Client::at(&l, "app").expect("client");
    let reply = c
        .send_or_spawn("hello", move || {
            sp.fetch_add(1, Ordering::SeqCst);
            let (l3, keep3) = (l2.clone(), Arc::clone(&keep2));
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(200)); // app start-up time
                let h = echo(&l3);
                keep3.lock().expect("lock").push(h);
            });
            Ok(())
        })
        .expect("send_or_spawn");
    assert_eq!(reply, "echo:hello");
    assert_eq!(spawned.load(Ordering::SeqCst), 1);
    // Already running: must not spawn again.
    let reply = c
        .send_or_spawn("again", || panic!("must not spawn when the app is running"))
        .expect("second");
    assert_eq!(reply, "echo:again");
    keep.lock().expect("lock").clear();
}

#[test]
fn send_or_spawn_reports_spawn_failure_and_startup_timeout() {
    let (_t, l) = loc();
    let c = Client::at(&l, "app").expect("client").with_config(fast_client());
    let err = c
        .send_or_spawn("x", || Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no ssx")))
        .expect_err("spawn failure");
    assert!(matches!(err, Error::Spawn(_)), "{err}");

    let err = c.send_or_spawn("x", || Ok(())).expect_err("app never starts");
    assert!(matches!(err, Error::Timeout { kind: TimeoutKind::Startup, .. }), "{err}");
}

#[test]
fn invalid_app_ids_are_rejected() {
    let (_t, l) = loc();
    for bad in ["", "a/b", "..", ".x", "a b"] {
        assert!(matches!(Instance::acquire_in(&l, bad), Err(Error::InvalidAppId(_))), "{bad:?}");
    }
}

#[cfg(unix)]
mod unix_only {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn stale_socket_from_crashed_instance_is_recovered() {
        let (_t, l) = loc();
        // Simulate a crash: a socket file with nobody behind it (std does not unlink on drop).
        let dir = l.dir().to_path_buf();
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let dead = dir.join("app.sock");
        drop(UnixListener::bind(&dead).expect("bind dead"));
        assert!(dead.exists());
        // Connecting to it fails (nobody listening) ...
        assert!(UnixStream::connect(&dead).is_err());
        // ... and acquire takes over cleanly.
        let s = primary(&l, Options::default()).serve(|l| l).expect("serve");
        let Acquired::Secondary(c) = Instance::acquire_in(&l, "app").expect("acquire") else {
            panic!("secondary expected")
        };
        assert_eq!(c.request("alive").expect("req"), "alive");
        drop(s);
    }

    #[test]
    fn non_socket_at_endpoint_path_is_refused() {
        let (_t, l) = loc();
        let dir = l.dir().to_path_buf();
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        std::fs::write(dir.join("app.sock"), b"precious").expect("write");
        let err = Instance::acquire_in(&l, "app").expect_err("must refuse");
        assert!(matches!(err, Error::ForeignEndpoint { .. }), "{err}");
        assert_eq!(std::fs::read(dir.join("app.sock")).expect("read"), b"precious");
    }

    #[test]
    fn permissions_are_owner_only() {
        let (_t, l) = loc();
        let srv = primary(&l, Options::default());
        let mode =
            |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(srv.socket_path()), 0o600, "socket");
        assert_eq!(mode(l.dir()), 0o700, "dir");
        assert_eq!(mode(&l.dir().join("app.lock")), 0o600, "lock file");
    }

    #[test]
    fn server_survives_garbage_and_bad_utf8() {
        let (_t, l) = loc();
        let srv = primary(&l, Options::default());
        let path = srv.socket_path().to_path_buf();
        let _h = srv.serve(|l| l).expect("serve");
        let mut raw = UnixStream::connect(&path).expect("connect");
        raw.write_all(b"\xff\xfe\xfd\n").expect("write");
        let mut buf = [0_u8; 16];
        assert_eq!(raw.read(&mut buf).expect("read"), 0, "server closes on invalid UTF-8");
        let c = Client::at(&l, "app").expect("client");
        assert_eq!(c.request("fine").expect("fine"), "fine");
    }

    #[test]
    fn server_drops_idle_and_slow_clients() {
        let (_t, l) = loc();
        let opts = Options {
            server: ServerConfig {
                idle_timeout: Duration::from_millis(150),
                request_timeout: Duration::from_millis(150),
                ..ServerConfig::default()
            },
            ..Options::default()
        };
        let srv = primary(&l, opts);
        let path = srv.socket_path().to_path_buf();
        let _h = srv.serve(|l| l).expect("serve");

        // Silent client: server closes the connection after the idle timeout.
        let mut idle = UnixStream::connect(&path).expect("connect");
        idle.set_read_timeout(Some(Duration::from_secs(3))).expect("timeout");
        let start = Instant::now();
        let mut b = [0_u8; 4];
        assert_eq!(idle.read(&mut b).expect("eof"), 0);
        assert!(start.elapsed() < Duration::from_secs(2));

        // Slow-loris client: starts a line and never finishes it.
        let mut slow = UnixStream::connect(&path).expect("connect");
        slow.set_read_timeout(Some(Duration::from_secs(3))).expect("timeout");
        slow.write_all(b"{\"partial\":").expect("write");
        let start = Instant::now();
        assert_eq!(slow.read(&mut b).expect("eof"), 0);
        assert!(start.elapsed() < Duration::from_secs(2));

        // Server is still healthy.
        let c = Client::at(&l, "app").expect("client");
        assert_eq!(c.request("ok").expect("ok"), "ok");
    }

    #[test]
    fn connection_limit_drops_excess_clients() {
        let (_t, l) = loc();
        let opts = Options {
            server: ServerConfig { max_connections: 2, ..ServerConfig::default() },
            ..Options::default()
        };
        let srv = primary(&l, opts);
        let path = srv.socket_path().to_path_buf();
        let _h = srv.serve(|l| l).expect("serve");
        let hold: Vec<UnixStream> =
            (0..2).map(|_| UnixStream::connect(&path).expect("c")).collect();
        thread::sleep(Duration::from_millis(100));
        let mut extra = UnixStream::connect(&path).expect("connect");
        extra.set_read_timeout(Some(Duration::from_secs(2))).expect("timeout");
        let mut b = [0_u8; 1];
        // Over the limit: closed right away instead of served.
        assert_eq!(extra.read(&mut b).expect("closed"), 0);
        drop(hold);
    }
}
