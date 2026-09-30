//! Finding helper programs (`ssx-overlay`, `ssx-editor-ui`, `ssx-settings-ui`, `ssx-app`).
//!
//! ssx keeps everything that needs a GPU stack or a windowing loop in separate executables so
//! the CLI and the daemon stay small and a crash in one cannot take the others down. They
//! are found the same way everywhere: an environment variable naming the file, then the
//! directory of the running executable, then `PATH`. Setting the variable to `none`, `off`
//! or an empty string switches the helper off, which keeps tests hermetic even when the helper
//! happens to be built next to the binary under test.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use crate::command::find_in_path;

/// The result of looking for a helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// Found at this path.
    Found(PathBuf),
    /// Switched off through its environment variable.
    Disabled,
    /// Not found anywhere.
    NotFound,
}

impl Discovery {
    /// The path, when found.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Found(p) => Some(p),
            Self::Disabled | Self::NotFound => None,
        }
    }

    /// Consumes the result into an `Option<PathBuf>`.
    pub fn into_path(self) -> Option<PathBuf> {
        match self {
            Self::Found(p) => Some(p),
            Self::Disabled | Self::NotFound => None,
        }
    }

    /// One phrase for `ssx doctor` and log lines.
    pub fn describe(&self, env_var: &str) -> String {
        match self {
            Self::Found(p) => format!("found at {}", p.display()),
            Self::Disabled => format!("disabled ({env_var}=none)"),
            Self::NotFound => {
                format!("not found (next to ssx, on PATH, or named by {env_var})")
            }
        }
    }
}

/// `true` for the values that switch a helper off: empty, `none` or `off` (any case).
pub fn is_disabled(value: &OsStr) -> bool {
    let v = value.to_string_lossy();
    let v = v.trim();
    v.is_empty() || v.eq_ignore_ascii_case("none") || v.eq_ignore_ascii_case("off")
}

/// The executable file name of a helper on this platform (`.exe` on Windows).
pub fn exe_name(base: &str) -> String {
    if cfg!(windows) { format!("{base}.exe") } else { base.to_owned() }
}

/// Looks for the helper `base` using the process environment.
pub fn discover(base: &str, env_var: &str) -> Discovery {
    discover_with(
        &exe_name(base),
        std::env::var_os(env_var),
        std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)),
        find_in_path,
    )
}

/// [`discover`] with injectable inputs: the value of the environment variable, the
/// directory of the running executable and the `PATH` lookup.
pub fn discover_with(
    file_name: &str,
    env_value: Option<OsString>,
    exe_dir: Option<PathBuf>,
    in_path: impl Fn(&str) -> Option<PathBuf>,
) -> Discovery {
    if let Some(v) = &env_value {
        if is_disabled(v) {
            return Discovery::Disabled;
        }
        let p = PathBuf::from(v);
        if p.is_file() {
            return Discovery::Found(p);
        }
        // A wrong explicit setting falls through to the normal search rather than failing:
        // the caller logs what it found.
    }
    exe_dir
        .map(|d| d.join(file_name))
        .filter(|p| p.is_file())
        .or_else(|| in_path(file_name))
        .map_or(Discovery::NotFound, Discovery::Found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn the_variable_wins_then_the_exe_dir_then_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        let from_env = touch(a.path(), "custom-name");
        let beside = touch(b.path(), "helper");
        let on_path = touch(c.path(), "helper");
        let path = |_: &str| Some(on_path.clone());

        let d = discover_with(
            "helper",
            Some(from_env.clone().into_os_string()),
            Some(b.path().into()),
            path,
        );
        assert_eq!(d, Discovery::Found(from_env));
        let d = discover_with("helper", None, Some(b.path().into()), path);
        assert_eq!(d, Discovery::Found(beside));
        let d = discover_with("helper", None, Some(a.path().into()), path);
        assert_eq!(d, Discovery::Found(on_path));
        assert_eq!(discover_with("helper", None, None, |_| None), Discovery::NotFound);
    }

    #[test]
    fn off_values_disable_and_a_wrong_path_falls_through() {
        let b = tempfile::tempdir().unwrap();
        let beside = touch(b.path(), "helper");
        for v in ["", "  ", "none", "NONE", "Off", " off "] {
            let d = discover_with("helper", Some(v.into()), Some(b.path().into()), |_| None);
            assert_eq!(d, Discovery::Disabled, "{v:?}");
            assert!(d.path().is_none());
        }
        let d = discover_with(
            "helper",
            Some("/definitely/not/here".into()),
            Some(b.path().into()),
            |_| None,
        );
        assert_eq!(d, Discovery::Found(beside.clone()));
        assert_eq!(d.into_path(), Some(beside));
    }

    #[test]
    fn descriptions_name_the_variable() {
        assert!(Discovery::NotFound.describe("SSX_X").contains("SSX_X"));
        assert!(Discovery::Disabled.describe("SSX_X").contains("disabled"));
        assert!(Discovery::Found("/a/b".into()).describe("SSX_X").contains("/a/b"));
        assert_eq!(exe_name("x"), if cfg!(windows) { "x.exe" } else { "x" });
    }
}
