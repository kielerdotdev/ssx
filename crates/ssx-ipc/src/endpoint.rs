//! Where the socket/pipe and lock file live, and how that location is secured.
//!
//! Design: the single-instance decision is made with an OS advisory lock on a lock file
//! (`flock` / `LockFileEx`), *not* by "does the socket exist". The kernel drops the lock when
//! the holder dies, so a crashed instance can never wedge the next start: whoever wins the lock
//! is by definition the only live instance and may delete any leftover socket file. The socket
//! is then only the rendezvous for messages.

use std::fs;
use std::path::{Path, PathBuf};

use interprocess::local_socket::Name;

use crate::error::{Error, Result};

/// Longest socket path we accept (`sun_path` is 104 bytes on macOS, 108 on Linux).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 100;

/// A per-user directory that holds the lock file and (on Unix) the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    dir: PathBuf,
}

impl Location {
    /// The default location for the current user.
    ///
    /// Unix: `$XDG_RUNTIME_DIR/ssx`, or `<tmp>/ssx-<uid>` when that is unset. Windows:
    /// `%LOCALAPPDATA%\ssx\run`.
    pub fn user_default() -> Result<Self> {
        Ok(Self { dir: default_dir()? })
    }

    /// Use an explicit directory (created private if missing). Intended for tests and for
    /// embedding applications that manage their own runtime dir.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn endpoint(&self, app_id: &str) -> Result<Endpoint> {
        validate_app_id(app_id)?;
        ensure_private_dir(&self.dir)?;
        Endpoint::new(&self.dir, app_id)
    }
}

pub(crate) fn validate_app_id(app_id: &str) -> Result<()> {
    let ok = (1..=48).contains(&app_id.len())
        && !app_id.starts_with('.')
        && app_id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok { Ok(()) } else { Err(Error::InvalidAppId(app_id.to_owned())) }
}

/// Resolved rendezvous point for one application id.
#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    pub(crate) lock_path: PathBuf,
    #[cfg(unix)]
    pub(crate) socket_path: PathBuf,
    #[cfg(windows)]
    pub(crate) pipe_name: String,
}

impl Endpoint {
    #[cfg(unix)]
    fn new(dir: &Path, app_id: &str) -> Result<Self> {
        let socket_path = dir.join(format!("{app_id}.sock"));
        let len = socket_path.as_os_str().len();
        if len > MAX_SOCKET_PATH {
            return Err(Error::PathTooLong { path: socket_path, len, limit: MAX_SOCKET_PATH });
        }
        Ok(Self { lock_path: dir.join(format!("{app_id}.lock")), socket_path })
    }

    #[cfg(windows)]
    #[allow(clippy::unnecessary_wraps)] // same signature as the Unix variant, which can fail
    fn new(dir: &Path, app_id: &str) -> Result<Self> {
        // Pipe names are machine-global, so they must be unique per user. The per-user
        // directory is unique per user, hence hash it into the name.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in dir.to_string_lossy().to_lowercase().bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        Ok(Self {
            lock_path: dir.join(format!("{app_id}.lock")),
            pipe_name: format!("ssx-{app_id}-{h:016x}"),
        })
    }

    /// The `interprocess` name for this endpoint.
    pub(crate) fn name(&self) -> std::io::Result<Name<'static>> {
        #[cfg(unix)]
        {
            use interprocess::local_socket::{GenericFilePath, ToFsName};
            self.socket_path.as_path().to_fs_name::<GenericFilePath>().map(Name::into_owned)
        }
        #[cfg(windows)]
        {
            use interprocess::local_socket::{GenericNamespaced, ToNsName};
            self.pipe_name.as_str().to_ns_name::<GenericNamespaced>().map(Name::into_owned)
        }
    }

    /// Human-readable description for logs.
    pub(crate) fn describe(&self) -> String {
        #[cfg(unix)]
        {
            self.socket_path.display().to_string()
        }
        #[cfg(windows)]
        {
            format!(r"\\.\pipe\{}", self.pipe_name)
        }
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)] // same signature as the Windows variant, which can fail
fn default_dir() -> Result<PathBuf> {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let rt = PathBuf::from(rt);
        if rt.is_absolute() && rt.is_dir() {
            return Ok(rt.join("ssx"));
        }
    }
    let uid = rustix::process::geteuid().as_raw();
    Ok(std::env::temp_dir().join(format!("ssx-{uid}")))
}

#[cfg(windows)]
fn default_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| Error::NoRuntimeDir("%LOCALAPPDATA% is not set".into()))?;
    Ok(base.join("ssx").join("run"))
}

/// Creates `dir` private (0700) if missing and verifies it is safe to use.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    let insecure = |reason: &str| Error::InsecureDirectory {
        path: dir.to_path_buf(),
        reason: reason.to_owned(),
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(Error::io("creating runtime directory"))?;
    // symlink_metadata: a symlink planted at the path must not be followed.
    let meta = fs::symlink_metadata(dir).map_err(Error::io("inspecting runtime directory"))?;
    if !meta.file_type().is_dir() {
        return Err(insecure("not a real directory (symlink?)"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(insecure("owned by another user"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(insecure("accessible by group/others (expected mode 0700)"));
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_private_dir(dir: &Path) -> Result<()> {
    // %LOCALAPPDATA% inherits an ACL limited to the user, SYSTEM and administrators.
    fs::create_dir_all(dir).map_err(Error::io("creating runtime directory"))
}

/// Checks that an existing socket path (if any) is a socket owned by the current user.
#[cfg(unix)]
pub(crate) fn check_socket_owner(path: &Path) -> Result<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(Error::io("inspecting socket")(e)),
    };
    let foreign = |reason: &str| Error::ForeignEndpoint {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    };
    if !meta.file_type().is_socket() {
        return Err(foreign("exists but is not a socket"));
    }
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(foreign("socket is owned by another user"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_validation() {
        for ok in ["ssx", "a", "my-app_1.2", &"x".repeat(48)] {
            assert!(validate_app_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".hidden", "a/b", "a b", "a\\b", "../x", "a\0b", "ünï", &"x".repeat(49)] {
            assert!(validate_app_id(bad).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_group_accessible_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run");
        fs::create_dir(&dir).expect("mkdir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = Location::in_dir(&dir).endpoint("ssx").expect_err("must reject");
        assert!(matches!(err, Error::InsecureDirectory { .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        fs::create_dir(&real).expect("mkdir");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let err = Location::in_dir(&link).endpoint("ssx").expect_err("must reject");
        assert!(matches!(err, Error::InsecureDirectory { .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_overlong_socket_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("d".repeat(120));
        let err = Location::in_dir(&dir).endpoint("ssx").expect_err("must reject");
        assert!(matches!(err, Error::PathTooLong { .. } | Error::Io { .. }), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn creates_dir_with_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("a").join("b");
        Location::in_dir(&dir).endpoint("ssx").expect("ok");
        let mode = fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
