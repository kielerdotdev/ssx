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

Linux: all unit and integration tests run (real sockets, threads). Windows: compiles and is
clippy-clean for `x86_64-pc-windows-msvc`, **not executed**; the named-pipe path, the ACL and
the non-blocking polling fallback used for timeouts (named pipes have no native read timeout
in `interprocess`) are untested on a real Windows host. macOS: not built here; it uses the
same Unix code with `LOCAL_PEERCRED` via `interprocess`.

## Known limits

* Windows pipe names are machine-global, so a local attacker could pre-create the pipe name
  of another user (squatting) and receive their requests. Mitigation planned: include the
  user SID in the name and verify the server's owner (needs `windows-sys`, i.e. `unsafe`).
* Windows peer check is ACL-only; the pid is informational.
* An elevated primary cannot be reached from a non-elevated client (owner is Administrators).
