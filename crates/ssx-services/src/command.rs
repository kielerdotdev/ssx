//! Running external programs: the [`CommandRunner`] service and the process helper the other
//! services (clipboard tools, editor helper) build on.
//!
//! Rules, in one place because they matter for security:
//!
//! * **No shell, ever.** The program is executed directly and every element of the argument
//!   vector is exactly one argument, so `; rm -rf`, `$(...)`, backticks, spaces, quotes and a
//!   leading `-` in a file name or URL are inert. (On Windows, Rust's standard library
//!   additionally refuses arguments it cannot pass safely to a `.bat`/`.cmd` target.)
//! * **Bounded everything.** Captured output is capped (stdout: the head, stderr: the tail),
//!   the process has a timeout and is killed on expiry or cancellation.
//! * **A daemonising child cannot hang us.** Tools such as `xclip` and `wl-copy` fork a
//!   background process that inherits the pipes; after the direct child exits we wait only
//!   briefly for the output readers instead of until the daemon closes them.
//! * **No console window flashes** for children on Windows.

use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread,
    time::{Duration, Instant},
};

use ssx_core::workflow::{CancelToken, CommandOutput, CommandRunner, CommandSpec, ServiceError};

/// Most stderr bytes kept in [`CommandOutput::stderr_tail`].
pub const STDERR_TAIL_BYTES: usize = 4096;

/// Default cap for captured standard output.
pub const STDOUT_HEAD_BYTES: usize = 64 * 1024;

/// How long after the child exits we wait for the output readers to finish.
const READER_GRACE: Duration = Duration::from_millis(500);

/// How a process is run by [`run_process`].
#[derive(Debug, Clone)]
pub struct ProcessSpec<'a> {
    /// Executable (looked up on `PATH` when it has no directory part).
    pub program: &'a str,
    /// Arguments, one element per argument.
    pub args: &'a [String],
    /// Bytes fed to the child's stdin (then closed); `None` means stdin is closed at once.
    pub stdin: Option<&'a [u8]>,
    /// Kill the process after this long.
    pub timeout: Duration,
    /// Keep (a bounded head of) stdout; otherwise it is discarded.
    pub capture_stdout: bool,
    /// Keep (a bounded tail of) stderr; otherwise it is discarded.
    pub capture_stderr: bool,
}

impl<'a> ProcessSpec<'a> {
    /// A spec with stdin closed, output discarded and a 30 second timeout.
    pub fn new(program: &'a str, args: &'a [String]) -> Self {
        Self {
            program,
            args,
            stdin: None,
            timeout: Duration::from_secs(30),
            capture_stdout: false,
            capture_stderr: false,
        }
    }
}

/// What a process produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// Exit code (`None` when killed by a signal).
    pub exit_code: Option<i32>,
    /// `true` for exit code 0.
    pub success: bool,
    /// The first [`STDOUT_HEAD_BYTES`] bytes of stdout (when captured).
    pub stdout: Vec<u8>,
    /// The last [`STDERR_TAIL_BYTES`] bytes of stderr, trimmed (when captured).
    pub stderr: String,
}

/// Finds `name` on `PATH` (with the usual executable extensions on Windows).
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let candidate = std::path::Path::new(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_owned())
            .split(';')
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        exts.iter().find_map(|ext| {
            let p = dir.join(format!("{name}{ext}"));
            is_executable(&p).then_some(p)
        })
    })
}

fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// A running output reader: the bytes so far and a signal that it has finished.
type Reader = (Arc<Mutex<Vec<u8>>>, mpsc::Receiver<()>);

#[derive(Clone, Copy)]
enum Keep {
    Head,
    Tail,
}

/// Reads `r` to the end, keeping at most `limit` bytes (the head or the tail), so a chatty
/// child can neither block on a full pipe nor make us allocate without bound.
fn spawn_reader(mut r: impl Read + Send + 'static, limit: usize, keep: Keep) -> Reader {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let shared = buf.clone();
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(n) = r.read(&mut chunk) {
            if n == 0 {
                break;
            }
            let mut b = shared.lock().unwrap_or_else(PoisonError::into_inner);
            match keep {
                Keep::Head => {
                    let room = limit.saturating_sub(b.len());
                    b.extend_from_slice(&chunk[..n.min(room)]);
                }
                Keep::Tail => {
                    b.extend_from_slice(&chunk[..n]);
                    if b.len() > limit {
                        let excess = b.len() - limit;
                        b.drain(..excess);
                    }
                }
            }
        }
        let _ = tx.send(());
    });
    (buf, rx)
}

fn spawn_error(program: &str, e: &std::io::Error) -> ServiceError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ServiceError::failed(format!(
            "cannot start {program:?}: no such program (is it installed and on PATH?)"
        ))
    } else {
        ServiceError::failed(format!("cannot start {program:?}: {e}"))
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs a process according to `spec`. See the [module docs](self) for the guarantees.
///
/// Non-zero exit is *not* an error (see [`ProcessOutput::success`]); failing to start, a
/// timeout and cancellation are.
pub fn run_process(
    spec: &ProcessSpec<'_>,
    cancel: &CancelToken,
) -> Result<ProcessOutput, ServiceError> {
    if spec.program.trim().is_empty() {
        return Err(ServiceError::NotConfigured("no program to run was given".to_owned()));
    }
    cancel.check().map_err(|_| ServiceError::Cancelled)?;
    let mut cmd = Command::new(spec.program);
    cmd.args(spec.args)
        .stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(if spec.capture_stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(if spec.capture_stderr { Stdio::piped() } else { Stdio::null() });
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| spawn_error(spec.program, &e))?;

    if let (Some(data), Some(mut stdin)) = (spec.stdin, child.stdin.take()) {
        let data = data.to_vec();
        // A separate thread, so a child that does not read cannot deadlock us; a broken
        // pipe (child exited early) is fine.
        thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let stdout = child.stdout.take().map(|s| spawn_reader(s, STDOUT_HEAD_BYTES, Keep::Head));
    let stderr = child.stderr.take().map(|s| spawn_reader(s, STDERR_TAIL_BYTES, Keep::Tail));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                kill(&mut child);
                return Err(ServiceError::Io(e));
            }
        }
        if cancel.is_cancelled() {
            kill(&mut child);
            return Err(ServiceError::Cancelled);
        }
        if started.elapsed() >= spec.timeout {
            kill(&mut child);
            return Err(ServiceError::failed(format!(
                "{:?} did not finish within {} seconds and was stopped",
                spec.program,
                spec.timeout.as_secs()
            )));
        }
        cancel.wait_timeout(Duration::from_millis(10));
    };

    let collect = |r: Option<Reader>| -> Vec<u8> {
        r.map(|(buf, done)| {
            let _ = done.recv_timeout(READER_GRACE);
            buf.lock().unwrap_or_else(PoisonError::into_inner).clone()
        })
        .unwrap_or_default()
    };
    let stdout = collect(stdout);
    let stderr = String::from_utf8_lossy(&collect(stderr)).trim().to_owned();
    Ok(ProcessOutput { exit_code: status.code(), success: status.success(), stdout, stderr })
}

/// The [`CommandRunner`] service (`run_command` after-upload step).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, spec: &CommandSpec, cancel: &CancelToken) -> Result<CommandOutput, ServiceError> {
        let out = run_process(
            &ProcessSpec {
                program: &spec.program,
                args: &spec.args,
                stdin: None,
                timeout: spec.timeout,
                capture_stdout: false,
                capture_stderr: true,
            },
            cancel,
        )?;
        Ok(CommandOutput {
            exit_code: out.exit_code,
            success: out.success,
            stderr_tail: out.stderr,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn spec(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            timeout: Duration::from_secs(10),
        }
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn hostile_arguments_arrive_verbatim_and_nothing_is_executed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        let hostile = [
            format!("; touch {}", marker.display()),
            format!("$(touch {})", marker.display()),
            format!("`touch {}`", marker.display()),
            format!("a && touch {}", marker.display()),
            "two words".to_owned(),
            "--flag-like".to_owned(),
            "*".to_owned(),
            "quote\"and'single".to_owned(),
            "new\nline".to_owned(),
            "$HOME".to_owned(),
            String::new(),
        ];
        let out_file = dir.path().join("argv");
        for arg in &hostile {
            // `sh -c 'printf %s "$1" > file' sh ARG`: ARG is only ever data.
            let out = SystemCommandRunner
                .run(
                    &spec(
                        "sh",
                        &["-c", "printf %s \"$1\" > \"$2\"", "sh", arg, out_file.to_str().unwrap()],
                    ),
                    &CancelToken::new(),
                )
                .unwrap();
            assert!(out.success, "{arg:?}");
            assert_eq!(std::fs::read_to_string(&out_file).unwrap(), *arg);
        }
        assert!(!marker.exists(), "an argument was interpreted by a shell");
    }

    #[test]
    fn exit_codes_and_stderr_tail() {
        let out = SystemCommandRunner
            .run(&spec("sh", &["-c", "echo oops >&2; exit 3"]), &CancelToken::new())
            .unwrap();
        assert_eq!(
            (out.exit_code, out.success, out.stderr_tail.as_str()),
            (Some(3), false, "oops")
        );
        assert!(SystemCommandRunner.run(&spec("true", &[]), &CancelToken::new()).unwrap().success);
    }

    #[test]
    fn stderr_is_bounded_to_the_tail() {
        let out = SystemCommandRunner
            .run(
                &spec("sh", &["-c", "head -c 200000 /dev/zero | tr '\\0' a >&2; printf END >&2"]),
                &CancelToken::new(),
            )
            .unwrap();
        assert!(out.stderr_tail.len() <= STDERR_TAIL_BYTES);
        assert!(out.stderr_tail.ends_with("END"), "the tail is kept, not the head");
    }

    #[test]
    fn stdout_is_bounded_to_the_head() {
        let out = run_process(
            &ProcessSpec {
                capture_stdout: true,
                ..ProcessSpec::new(
                    "sh",
                    &strings(&["-c", "printf HEAD; head -c 500000 /dev/zero | tr '\\0' b"]),
                )
            },
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(out.stdout.len(), STDOUT_HEAD_BYTES);
        assert!(out.stdout.starts_with(b"HEAD"));
    }

    #[test]
    fn timeout_kills_the_process() {
        let mut s = spec("sleep", &["30"]);
        s.timeout = Duration::from_millis(150);
        let started = Instant::now();
        let e = SystemCommandRunner.run(&s, &CancelToken::new()).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation_kills_the_process_and_a_cancelled_token_never_spawns() {
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            c2.cancel();
        });
        let started = Instant::now();
        let e = SystemCommandRunner.run(&spec("sleep", &["30"]), &cancel).unwrap_err();
        h.join().unwrap();
        assert!(e.is_cancelled());
        assert!(started.elapsed() < Duration::from_secs(5));

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let e = SystemCommandRunner
            .run(&spec("touch", &[marker.to_str().unwrap()]), &cancel)
            .unwrap_err();
        assert!(e.is_cancelled());
        assert!(!marker.exists());
    }

    #[test]
    fn missing_and_empty_programs_are_clear_errors() {
        let e = SystemCommandRunner
            .run(&spec("definitely-not-a-program-ssx", &[]), &CancelToken::new())
            .unwrap_err();
        assert!(
            e.to_string().contains("definitely-not-a-program-ssx")
                && e.to_string().contains("PATH"),
            "{e}"
        );
        let e = SystemCommandRunner.run(&spec("  ", &[]), &CancelToken::new()).unwrap_err();
        assert!(matches!(e, ServiceError::NotConfigured(_)));
    }

    #[test]
    fn stdin_is_fed_and_a_daemonising_child_cannot_hang_the_caller() {
        // The shell backgrounds a process that keeps stdout/stderr open for 5 seconds and
        // exits at once, exactly like xclip/wl-copy do.
        let started = Instant::now();
        let out = run_process(
            &ProcessSpec {
                stdin: Some(b"payload"),
                capture_stdout: true,
                capture_stderr: true,
                ..ProcessSpec::new(
                    "sh",
                    &strings(&["-c", "cat > /dev/null; echo hi; (sleep 5 &) ; exit 0"]),
                )
            },
            &CancelToken::new(),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(3), "waited {:?}", started.elapsed());
        assert_eq!(out.stdout, b"hi\n");

        let echoed = run_process(
            &ProcessSpec {
                stdin: Some(b"round trip"),
                capture_stdout: true,
                ..ProcessSpec::new("cat", &[])
            },
            &CancelToken::new(),
        )
        .unwrap();
        assert_eq!(echoed.stdout, b"round trip");
    }

    #[test]
    fn find_in_path_finds_real_programs_only() {
        assert!(find_in_path("sh").is_some());
        assert!(find_in_path("definitely-not-a-program-ssx").is_none());
        assert!(find_in_path("/bin/sh").is_some());
        assert!(find_in_path("/nonexistent/sh").is_none());
    }
}
