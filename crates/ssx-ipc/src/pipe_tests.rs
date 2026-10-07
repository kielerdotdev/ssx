//! The framing layer over a double with Windows named-pipe semantics: blocking reads, no
//! timeouts, no half-close, and EOF only when the peer is dropped. These run on every platform,
//! so the Windows transport logic is exercised by the Linux test run too.

use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, TimeoutKind};
use crate::framing::{Conn, LineReader, write_line};
use crate::threaded::fake::duplex;

const D: Duration = Duration::from_secs(5);

fn conns(capacity: usize) -> (Conn, Conn) {
    let (a, b) = duplex(capacity);
    (Conn::from_halves(a.read, a.write), Conn::from_halves(b.read, b.write))
}

#[test]
fn silent_connection_is_idle_not_end_of_stream() {
    // Regression: the Windows build treated "no data yet" as EOF, so a server hung up on every
    // client that had not written its request yet, and a client saw its reply "close".
    let (mut client, mut server) = conns(64);
    let mut r = LineReader::new(100);
    let err = r
        .read_line(&mut server, Duration::from_millis(60), D)
        .expect_err("idle must time out, not report EOF");
    assert!(matches!(err, Error::Timeout { kind: TimeoutKind::Read, .. }), "{err}");

    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(80));
        write_line(&mut client, "late request", 100, D).expect("write");
        client
    });
    assert_eq!(r.read_line(&mut server, D, D).expect("line").as_deref(), Some("late request"));
    drop(t.join().expect("join"));
}

#[test]
fn request_response_roundtrips_and_server_drop_is_eof() {
    let (mut client, mut server) = conns(16);
    let srv = thread::spawn(move || {
        let mut r = LineReader::new(100_000);
        for _ in 0..3 {
            let line = r.read_line(&mut server, D, D).expect("read").expect("some");
            write_line(&mut server, &format!("echo:{line}"), 100_000, D).expect("reply");
        }
        // Dropping `server` is the only way to signal "done" on a pipe.
    });
    let mut r = LineReader::new(100_000);
    for i in 0..3 {
        let big = "x".repeat(40 * i); // larger than the 16-byte pipe buffer
        write_line(&mut client, &big, 100_000, D).expect("write");
        let reply = r.read_line(&mut client, D, D).expect("read").expect("reply");
        assert_eq!(reply, format!("echo:{big}"));
    }
    srv.join().expect("join");
    assert_eq!(r.read_line(&mut client, D, D).expect("eof"), None);
}

#[test]
fn eof_inside_a_line_is_connection_closed() {
    let (mut client, mut server) = conns(64);
    client.write_all(b"half a li", Instant::now() + D).expect("write");
    drop(client);
    let mut r = LineReader::new(100);
    assert!(matches!(r.read_line(&mut server, D, D), Err(Error::ConnectionClosed)));
}

#[test]
fn data_arriving_after_a_timed_out_read_is_not_lost() {
    let (mut client, mut server) = conns(64);
    let mut r = LineReader::new(100);
    assert!(matches!(
        r.read_line(&mut server, Duration::from_millis(30), D),
        Err(Error::Timeout { kind: TimeoutKind::Read, .. })
    ));
    write_line(&mut client, "one", 100, D).expect("write");
    write_line(&mut client, "two", 100, D).expect("write");
    assert_eq!(r.read_line(&mut server, D, D).expect("l1").as_deref(), Some("one"));
    assert_eq!(r.read_line(&mut server, D, D).expect("l2").as_deref(), Some("two"));
}

#[test]
fn oversize_line_is_rejected() {
    let (mut client, mut server) = conns(64);
    let t = thread::spawn(move || {
        // Fails with a broken pipe once the server gives up and drops; that is expected.
        let _ = client.write_all(&[b'a'; 300], Instant::now() + D);
        client
    });
    let mut r = LineReader::new(100);
    let err = r.read_line(&mut server, D, D).expect_err("oversize");
    assert!(matches!(err, Error::LineTooLong { max: 100 }), "{err}");
    drop(server);
    drop(t.join().expect("join"));
}

#[test]
fn write_to_a_peer_that_never_reads_times_out_and_keeps_order() {
    let (mut client, mut server) = conns(4);
    let err = write_line(&mut client, "0123456789", 100, Duration::from_millis(60))
        .expect_err("pipe is full");
    assert!(matches!(err, Error::Timeout { kind: TimeoutKind::Write, .. }), "{err}");
    // The stalled write is finished first, so bytes never interleave.
    let t = thread::spawn(move || {
        write_line(&mut client, "next", 100, D).expect("second write");
        client
    });
    let mut r = LineReader::new(100);
    assert_eq!(r.read_line(&mut server, D, D).expect("l1").as_deref(), Some("0123456789"));
    assert_eq!(r.read_line(&mut server, D, D).expect("l2").as_deref(), Some("next"));
    drop(t.join().expect("join"));
}

#[test]
fn dropping_a_connection_releases_the_pipe_for_the_peer() {
    // Write-only use: no reader thread was ever started, so dropping closes at once.
    let (mut client, mut server) = conns(64);
    write_line(&mut client, "bye", 100, D).expect("write");
    drop(client);
    let mut r = LineReader::new(100);
    assert_eq!(r.read_line(&mut server, D, D).expect("line").as_deref(), Some("bye"));
    assert_eq!(r.read_line(&mut server, D, D).expect("eof"), None);

    // After a completed read the helper thread is idle (never reads ahead), so it lets go too.
    let (mut client, mut server) = conns(64);
    write_line(&mut server, "hi", 100, D).expect("write");
    assert_eq!(r.read_line(&mut client, D, D).expect("line").as_deref(), Some("hi"));
    drop(client);
    let start = Instant::now();
    loop {
        match write_line(&mut server, "anyone?", 100, D) {
            Err(Error::ConnectionClosed) => break,
            other => assert!(start.elapsed() < D, "peer never saw the close: {other:?}"),
        }
        thread::sleep(Duration::from_millis(5));
    }
}
