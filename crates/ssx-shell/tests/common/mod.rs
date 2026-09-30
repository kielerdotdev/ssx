//! Shared helpers for the integration tests: sandboxed contexts, tree snapshots, golden files.
#![allow(dead_code)] // each test binary uses a different subset

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ssx_shell::{Context, Platform, RecordingRunner};

/// A temp root with a sandboxed [`Context`]; nothing outside it is ever touched.
pub struct Sandbox {
    pub tmp: tempfile::TempDir,
    pub ctx: Context,
    pub runner: Arc<RecordingRunner>,
}

impl Sandbox {
    pub fn new(exe: &str, platform: Platform) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let runner = Arc::new(RecordingRunner::default());
        let mut ctx = Context::sandboxed(tmp.path(), exe);
        ctx.platform = platform;
        ctx.runner = runner.clone();
        fs::create_dir_all(&ctx.home).expect("home");
        fs::create_dir_all(tmp.path().join("bin")).expect("bin");
        Self { tmp, ctx, runner }
    }

    pub fn linux() -> Self {
        Self::new("/usr/bin/ssx", Platform::Linux)
    }

    pub fn home(&self) -> &Path {
        &self.ctx.home
    }

    /// Puts an executable named `name` on the sandbox `PATH`.
    pub fn add_binary(&self, name: &str) {
        let p = self.tmp.path().join("bin").join(name);
        fs::write(&p, "#!/bin/sh\n").expect("write binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    /// Creates a file (and parents) under the home directory.
    pub fn write_home(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.home().join(rel);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(&p, content).expect("write");
        p
    }

    pub fn snapshot(&self) -> Vec<Entry> {
        snapshot(&self.ctx.home)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Dir { path: String, mode: u32 },
    File { path: String, mode: u32, bytes: Vec<u8> },
}

pub fn snapshot(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|a, b| key(a).cmp(key(b)));
    out
}

fn key(e: &Entry) -> &str {
    match e {
        Entry::Dir { path, .. } | Entry::File { path, .. } => path,
    }
}

fn mode_of(p: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(p).map_or(0, |m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        0
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) {
    for e in fs::read_dir(dir).expect("read_dir") {
        let e = e.expect("entry");
        let p = e.path();
        let rel = p.strip_prefix(root).expect("prefix").to_string_lossy().replace('\\', "/");
        if e.file_type().expect("type").is_dir() {
            out.push(Entry::Dir { path: rel, mode: mode_of(&p) });
            walk(root, &p, out);
        } else {
            out.push(Entry::File {
                path: rel,
                mode: mode_of(&p),
                bytes: fs::read(&p).expect("read"),
            });
        }
    }
}

/// Compares `actual` with `tests/golden/<name>`; `UPDATE_GOLDEN=1` rewrites the file.
pub fn assert_golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("missing golden {} ({e}); run with UPDATE_GOLDEN=1 to create it", path.display())
    });
    assert_eq!(
        actual, expected,
        "golden mismatch for {name}; if intended, rerun with UPDATE_GOLDEN=1 and review the diff"
    );
}

pub fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}
