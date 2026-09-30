//! The [`Editor`] service: hands the image to the `ssx-editor-ui` helper program.
//!
//! The editor *engine* (`ssx-editor`) is toolkit-free; the window that drives it is a separate
//! executable so that the CLI, the daemon and the hotkey handlers stay small, start fast and
//! do not link a GPU stack. That helper is not part of this workspace yet, so this service
//! defines the contract it must follow and degrades to [`ServiceError::Unsupported`] when the
//! helper is absent.
//!
//! # Helper protocol
//!
//! ```text
//! ssx-editor-ui --output <out.png> <in.png>
//! ```
//!
//! * `<in.png>` (a positional argument, never starting with `-`) exists and is the image to
//!   edit; the helper must not modify it. `--output` puts it in "workflow mode".
//! * The user accepts the edit: the helper writes the result to `<out.png>` and exits `0`
//!   (anything it prints on stdout, such as a JSON outcome line, is ignored).
//! * The user closes the editor without accepting: exit code `3` (the same code the `ssx`
//!   CLI uses for "cancelled"), or exit `0` without writing `<out.png>`.
//! * Anything else is a failure; the last few KiB of its stderr are shown to the user.
//!
//! The helper is found via the `SSX_EDITOR_UI` environment variable (a full path), then next
//! to the running executable, then on `PATH`. Setting `SSX_EDITOR_UI=none` (or `off`, or an
//! empty value) disables the editor entirely, which keeps tests hermetic even when the helper
//! happens to be built next to the binary under test.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use ssx_core::workflow::{CancelToken, EditResult, Editor, ServiceError};
use ssx_types::{EncodeOptions, Frame, ImageFormat};

use crate::command::{ProcessSpec, find_in_path, run_process};

/// File name of the helper (with `.exe` on Windows).
pub const HELPER_NAME: &str = if cfg!(windows) { "ssx-editor-ui.exe" } else { "ssx-editor-ui" };

/// Environment variable that names the helper explicitly.
pub const HELPER_ENV: &str = "SSX_EDITOR_UI";

/// Exit code by which the helper reports "the user cancelled".
pub const EXIT_CANCELLED: i32 = 3;

/// How long an editing session may stay open before it is abandoned.
const SESSION_LIMIT: Duration = Duration::from_secs(12 * 60 * 60);

/// The [`Editor`] implementation. See the [module docs](self).
#[derive(Debug, Clone, Default)]
pub struct ExternalEditor {
    helper: Option<PathBuf>,
}

impl ExternalEditor {
    /// Looks for the helper (environment variable, next to the executable, `PATH`).
    pub fn discover() -> Self {
        let var = std::env::var_os(HELPER_ENV);
        if var.as_deref().is_some_and(is_disabled) {
            return Self { helper: None };
        }
        Self::discover_with(
            var.map(PathBuf::from),
            std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)),
        )
    }

    /// [`discover`](Self::discover) with injectable inputs.
    pub fn discover_with(env_path: Option<PathBuf>, exe_dir: Option<PathBuf>) -> Self {
        let helper = env_path
            .filter(|p| p.is_file())
            .or_else(|| exe_dir.map(|d| d.join(HELPER_NAME)).filter(|p| p.is_file()))
            .or_else(|| find_in_path(HELPER_NAME));
        Self { helper }
    }

    /// Uses this helper program.
    pub fn with_helper(path: impl Into<PathBuf>) -> Self {
        Self { helper: Some(path.into()) }
    }

    /// The helper that will be used, if one was found.
    pub fn helper(&self) -> Option<&Path> {
        self.helper.as_deref()
    }
}

/// `SSX_EDITOR_UI` values that mean "no editor": empty, `none` or `off` (any case).
fn is_disabled(value: &std::ffi::OsStr) -> bool {
    let v = value.to_string_lossy();
    let v = v.trim();
    v.is_empty() || v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("off")
}

impl Editor for ExternalEditor {
    fn edit(&self, frame: &Frame, cancel: &CancelToken) -> Result<EditResult, ServiceError> {
        let Some(helper) = &self.helper else {
            return Err(ServiceError::Unsupported(format!(
                "the image editor (the `{HELPER_NAME}` helper was not found next to ssx or on PATH; \
                 set {HELPER_ENV} to its location)"
            )));
        };
        let png = frame
            .encode(EncodeOptions { png_fast: true, ..EncodeOptions::new(ImageFormat::Png) })
            .map_err(|e| {
                ServiceError::failed(format!("cannot hand the image to the editor: {e}"))
            })?;
        let dir = tempfile::Builder::new().prefix("ssx-edit-").tempdir()?;
        let input = dir.path().join("input.png");
        let output = dir.path().join("output.png");
        std::fs::write(&input, png)?;

        let helper = helper.to_string_lossy();
        let args = [
            "--output".to_owned(),
            output.to_string_lossy().into_owned(),
            input.to_string_lossy().into_owned(),
        ];
        let out = run_process(
            &ProcessSpec {
                timeout: SESSION_LIMIT,
                capture_stderr: true,
                ..ProcessSpec::new(&helper, &args)
            },
            cancel,
        )?;
        match out.exit_code {
            Some(0) => {}
            Some(EXIT_CANCELLED) => return Ok(EditResult::Cancelled),
            other => {
                return Err(ServiceError::failed(format!(
                    "the image editor failed (exit code {}){}{}",
                    other.map_or_else(|| "none".to_owned(), |c| c.to_string()),
                    if out.stderr.is_empty() { "" } else { ": " },
                    out.stderr
                )));
            }
        }
        let bytes = match std::fs::read(&output) {
            Ok(b) if !b.is_empty() => b,
            Ok(_) => return Ok(EditResult::Cancelled),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(EditResult::Cancelled),
            Err(e) => return Err(e.into()),
        };
        let edited = Frame::decode(&bytes).map_err(|e| {
            ServiceError::failed(format!("the image editor produced an unreadable image: {e}"))
        })?;
        Ok(EditResult::Edited(edited))
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn disable_values_are_recognised() {
        use std::ffi::OsStr;
        for v in ["", "  ", "none", "NONE", "Off", " off "] {
            assert!(super::is_disabled(OsStr::new(v)), "{v:?} should disable the editor");
        }
        for v in ["/usr/bin/ssx-editor-ui", "ssx-editor-ui", "nonexistent"] {
            assert!(!super::is_disabled(OsStr::new(v)), "{v:?} is a path, not a disable switch");
        }
    }

    use std::{os::unix::fs::PermissionsExt, time::Instant};

    use super::*;

    fn helper(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-editor");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn frame() -> Frame {
        Frame::from_rgba8(2, 2, [10, 20, 30, 255].repeat(4)).unwrap()
    }

    // The fake parses `--output Y X` like the real helper would.
    const PARSE: &str = "OUT=$2; IN=$3;";

    #[test]
    fn accepted_edits_come_back_decoded() {
        let d = tempfile::tempdir().unwrap();
        // "Edits" by ignoring the input and writing a different image: a copy of the input
        // is used as a stand-in valid PNG, then verified to be the *same* pixels.
        let h = helper(d.path(), &format!("{PARSE} cp \"$IN\" \"$OUT\""));
        let got = ExternalEditor::with_helper(h).edit(&frame(), &CancelToken::new()).unwrap();
        let EditResult::Edited(f) = got else { panic!("expected an edited image") };
        assert_eq!((f.width(), f.height()), (2, 2));
        assert_eq!(&f.row(0)[..4], &[10, 20, 30, 255], "the helper received the exact pixels");
    }

    #[test]
    fn cancel_is_exit_code_3_or_no_output() {
        let d = tempfile::tempdir().unwrap();
        let by_code = helper(d.path(), "exit 3");
        assert!(matches!(
            ExternalEditor::with_helper(by_code).edit(&frame(), &CancelToken::new()).unwrap(),
            EditResult::Cancelled
        ));
        let no_output = helper(d.path(), "exit 0");
        assert!(matches!(
            ExternalEditor::with_helper(no_output).edit(&frame(), &CancelToken::new()).unwrap(),
            EditResult::Cancelled
        ));
    }

    #[test]
    fn failures_carry_the_helpers_stderr() {
        let d = tempfile::tempdir().unwrap();
        let h = helper(d.path(), "echo 'no GPU found' >&2; exit 1");
        let e = ExternalEditor::with_helper(h).edit(&frame(), &CancelToken::new()).unwrap_err();
        let m = e.to_string();
        assert!(m.contains("exit code 1") && m.contains("no GPU found"), "{m}");

        let garbage = helper(d.path(), &format!("{PARSE} echo not a png > \"$OUT\""));
        let e =
            ExternalEditor::with_helper(garbage).edit(&frame(), &CancelToken::new()).unwrap_err();
        assert!(e.to_string().contains("unreadable image"), "{e}");
    }

    #[test]
    fn the_paths_are_passed_as_separate_arguments() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("argv");
        let h =
            helper(d.path(), &format!("printf '%s\\n' \"$#\" \"$1\" > {}; exit 3", log.display()));
        ExternalEditor::with_helper(h).edit(&frame(), &CancelToken::new()).unwrap();
        let text = std::fs::read_to_string(log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, ["3", "--output"]);
    }

    #[test]
    fn cancelling_kills_the_open_editor() {
        let d = tempfile::tempdir().unwrap();
        let h = helper(d.path(), "sleep 60");
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.cancel();
        });
        let started = Instant::now();
        let e = ExternalEditor::with_helper(h).edit(&frame(), &cancel).unwrap_err();
        t.join().unwrap();
        assert!(e.is_cancelled());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn no_helper_is_unsupported_and_says_where_to_put_it() {
        let e = ExternalEditor::default().edit(&frame(), &CancelToken::new()).unwrap_err();
        assert!(
            matches!(&e, ServiceError::Unsupported(m) if m.contains("ssx-editor-ui") && m.contains("SSX_EDITOR_UI")),
            "{e}"
        );
    }

    #[test]
    fn discovery_prefers_the_environment_then_the_exe_dir() {
        let d = tempfile::tempdir().unwrap();
        let env_helper = helper(d.path(), "exit 3");
        let exe_dir = tempfile::tempdir().unwrap();
        std::fs::copy(&env_helper, exe_dir.path().join(HELPER_NAME)).unwrap();

        let e =
            ExternalEditor::discover_with(Some(env_helper.clone()), Some(exe_dir.path().into()));
        assert_eq!(e.helper(), Some(env_helper.as_path()));
        let e = ExternalEditor::discover_with(
            Some(d.path().join("missing")),
            Some(exe_dir.path().into()),
        );
        assert_eq!(e.helper(), Some(exe_dir.path().join(HELPER_NAME).as_path()));
        let e = ExternalEditor::discover_with(None, Some(d.path().into()));
        // Not next to the exe and (in CI) not on PATH either.
        if find_in_path(HELPER_NAME).is_none() {
            assert!(e.helper().is_none());
        }
    }
}
