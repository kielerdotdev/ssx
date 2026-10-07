# ssx-ipc

Single-instance guard and local IPC transport for ssx. It moves `\n`-terminated UTF-8 lines
(JSON in practice) between processes of the **same user**. Message types live in `ssx-core`;
this crate is deliberately schema-free so the tiny file-manager shim and the full app share it.

```rust
use ssx_ipc::{Acquired, Instance};

match Instance::acquire("ssx")? {
    Acquired::Primary(server) => {
        // We are the app. Serve on background threads (or drive `server.incoming()` yourself).
        let _running = server.serve(|line| handle_command(&line))?;
    }
    Acquired::Secondary(client) => {
        // Another instance exists: forward our command and exit.
        let reply = client.request(r#"{"cmd":"post-file","paths":["/tmp/a.png"]}"#)?;
    }
}
```

The file-manager shim does not want to become the app, so it skips `acquire`:

```rust
let client = ssx_ipc::Client::for_app("ssx")?;
client.send_or_spawn(&line, || std::process::Command::new(ssx_exe).arg("daemon").spawn().map(drop))?;
```

## How it works

| | Linux / macOS | Windows |
|---|---|---|
| Transport | Unix domain socket `<runtime dir>/ssx/<app>.sock`, mode 0600 | Named pipe `\\.\pipe\ssx-<app>-<hash of user dir>` |
| Runtime dir | `$XDG_RUNTIME_DIR/ssx`, else `<tmp>/ssx-<uid>`; created 0700, ownership/mode/symlink verified | `%LOCALAPPDATA%\ssx\run` |
| Single instance | `flock` on `<app>.lock` | `LockFileEx` on `<app>.lock` |
| Access control | dir 0700 + socket 0600 + peer uid check on **both** ends | pipe DACL `D:P(A;;GA;;;OW)` (owner only), remote clients rejected |

* **Race-free, crash-safe**: the winner of the lock is primary. The kernel drops the lock if
  the holder crashes, so leftover socket files are simply removed by the next primary (stale
  socket recovery needs no probing or PID files). N concurrent `acquire` calls yield exactly
  one primary.
* **Framing**: max line 4 MiB (configurable); oversize, non-UTF-8, and embedded-newline lines
  are rejected; idle timeout (30 s) and per-request completion timeout (10 s, slow-loris);
  connection cap for `serve` (32).
* **Client**: connect retries with exponential backoff (5 ms .. 200 ms) up to
  `connect_timeout`; a request is sent **at most once** (never re-sent after the line was
  written). `send_or_spawn` calls your `spawn` closure once if nothing listens and then waits
  up to `startup_timeout`. Two shims racing may both spawn; the app's own `acquire` makes the
  extra process a secondary.
* Socket removal: dropping the `Server` (or `ServeHandle::shutdown`) unlinks the socket and
  then releases the lock. The lock file itself is intentionally never deleted (deleting it
  would let two processes lock different inodes).

## Verification status

Linux: all unit and integration tests run (real sockets, threads). The Windows transport
(blocking named-pipe halves driven from helper threads, because pipes have no native timeouts
and `PIPE_NOWAIT` reports "no data yet" as a hang-up) is additionally covered on Linux by
`src/pipe_tests.rs`, which runs the framing layer over a double with named-pipe semantics.
Windows: compiles and is clippy-clean for `x86_64-pc-windows-msvc`; the real named-pipe path
and the ACL only run in the Windows CI job. macOS: not built here; it uses the same Unix code
with `LOCAL_PEERCRED` via `interprocess`.

Known limitation (Windows): a read that times out cannot be cancelled without FFI, so a peer
that connects and then goes silent keeps one idle helper thread and pipe instance alive until
it disconnects (or the process exits), even though the server has already given up on it.
Peers are same-user only (pipe ACL); `CancelIoEx` would close this if it ever matters.
