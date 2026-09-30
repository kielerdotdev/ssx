//! Running external tools (`gsettings`, `kwriteconfig6`, `swaymsg`, ...), injectable so the
//! logic around them can be tested with a fake.

use std::io;

use crate::command::quote_posix;

use super::BindingError;

/// What a tool printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunOutput {
    /// Exited with status 0.
    pub success: bool,
    /// Exit code, if the process exited normally.
    pub code: Option<i32>,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl RunOutput {
    /// A successful run printing `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self { success: true, code: Some(0), stdout: stdout.into(), stderr: String::new() }
    }

    /// A failed run with `stderr`.
    pub fn failed(stderr: impl Into<String>) -> Self {
        Self { success: false, code: Some(1), stdout: String::new(), stderr: stderr.into() }
    }
}

/// Runs a program to completion, capturing output. `Err` means it could not be started
/// (typically `NotFound`); a non-zero exit is an `Ok` with `success == false`.
pub trait CommandRunner {
    /// Runs `program args...`.
    fn run(&self, program: &str, args: &[String]) -> io::Result<RunOutput>;
}

/// Runs real processes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String]) -> io::Result<RunOutput> {
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(RunOutput {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// The command line for messages: words quoted for a POSIX shell.
pub(crate) fn display(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(quote_posix)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs and requires success, mapping failures to actionable errors.
pub(crate) fn run_checked(
    runner: &dyn CommandRunner,
    program: &str,
    args: &[String],
    install_hint: &'static str,
) -> Result<RunOutput, BindingError> {
    match runner.run(program, args) {
        Ok(out) if out.success => Ok(out),
        Ok(out) => Err(BindingError::CommandFailed {
            command: display(program, args),
            status: out
                .code
                .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
            stderr: out.stderr.trim().to_owned(),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(BindingError::ToolMissing(program.to_owned(), install_hint))
        }
        Err(e) => Err(BindingError::CommandFailed {
            command: display(program, args),
            status: "could not start".to_owned(),
            stderr: e.to_string(),
        }),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;

    /// A scripted runner: records every call and answers from a `gsettings`-like store.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub calls: RefCell<Vec<Vec<String>>>,
        /// `(program, first-arg-prefix)` -> canned output, checked before `handler`.
        pub canned: RefCell<HashMap<String, RunOutput>>,
    }

    impl Fake {
        pub fn calls_text(&self) -> Vec<String> {
            self.calls.borrow().iter().map(|c| c.join(" ")).collect()
        }
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[String]) -> io::Result<RunOutput> {
            let mut call = vec![program.to_owned()];
            call.extend(args.iter().cloned());
            let key = call.join(" ");
            self.calls.borrow_mut().push(call);
            Ok(self.canned.borrow().get(&key).cloned().unwrap_or_else(|| RunOutput::ok("")))
        }
    }
}
