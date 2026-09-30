//! Runs the generated nautilus-python extension under real Python against stub `gi` modules
//! and checks the menu items and the argv passed to `subprocess.Popen`. Skips without python3.
#![cfg(unix)]

mod common;

use std::fs;
use std::process::Command;

use common::Sandbox;
use ssx_shell::linux::nautilus::{Nautilus, NautilusVariant};
use ssx_shell::{Integration, Platform};

const STUB_GI_INIT: &str = "def require_version(name, version):\n    if version == '4.0' and STUB_ONLY_3:\n        raise ValueError('no 4.0')\nSTUB_ONLY_3 = False\n";

const STUB_REPO: &str = r"
class GObject:
    class GObject:
        pass

class Nautilus:
    class MenuProvider:
        pass

    class MenuItem:
        def __init__(self, name, label, tip):
            self.name, self.label, self.tip = name, label, tip
            self.handlers = []

        def connect(self, signal, callback, *args):
            self.handlers.append((signal, callback, args))

        def activate(self):
            for signal, callback, args in self.handlers:
                callback(self, *args)
";

const DRIVER: &str = r#"
import importlib.util, json, sys
sys.path.insert(0, sys.argv[1])
spec = importlib.util.spec_from_file_location("ssxext", sys.argv[2])
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

launched = []
class FakePopen:
    def __init__(self, argv, **kwargs):
        launched.append([argv, sorted(kwargs)])
m.subprocess.Popen = FakePopen

class Loc:
    def __init__(self, p): self.p = p
    def get_path(self): return self.p

class F:
    def __init__(self, path, mime="application/octet-stream", scheme="file", is_dir=False):
        self.path, self.mime, self.scheme, self.dir = path, mime, scheme, is_dir
    def get_uri_scheme(self): return self.scheme
    def get_location(self): return Loc(self.path)
    def get_name(self): return self.path.rsplit("/", 1)[-1]
    def get_mime_type(self): return self.mime
    def is_directory(self): return self.dir

HOSTILE = ["/tmp/a b.png", "/tmp/it's -x.png", "/tmp/$(id) `id`.PNG", "/tmp/new\nline.png"]
scenarios = {
    "images": [F(p, "image/png") for p in HOSTILE],
    "one_image": [F(HOSTILE[2], "image/png")],
    "one_video": [F("/v/clip.MKV", "video/x-matroska")],
    "text": [F("/t/notes.txt", "text/plain")],
    "extless_image_by_mime": [F("/t/scan", "image/jpeg")],
    "directory": [F("/t/dir", "inode/directory", is_dir=True)],
    "remote": [F("/net/x.png", "image/png", scheme="smb")],
    "empty": [],
}
provider = m.SsxMenuProvider()
result = {}
for name, files in scenarios.items():
    # Nautilus 4 signature (files) and the 3.x one (window, files) must both work.
    items = provider.get_file_items(files)
    items_old = provider.get_file_items(None, files)
    assert [i.label for i in items] == [i.label for i in items_old], name
    launched.clear()
    for it in items:
        it.activate()
    result[name] = {"labels": [i.label for i in items], "launched": launched[:]}
assert provider.get_background_items(None, None) == []
print(json.dumps(result))
"#;

fn python() -> Option<String> {
    ["python3", "python"]
        .iter()
        .find(|p| Command::new(p).arg("--version").output().is_ok_and(|o| o.status.success()))
        .map(|p| (*p).to_owned())
}

#[test]
fn extension_source_compiles_and_behaves_with_hostile_paths() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 is not installed");
        return;
    };
    let mut s = Sandbox::new("/opt/it's a $HOME \"dir\"/ssx", Platform::Linux);
    s.ctx.ssx_exe = "/opt/it's a $HOME \"dir\"/日本/ssx".into();
    Nautilus::with_variant(NautilusVariant::Extension).install(&s.ctx).expect("install");
    let ext = s.ctx.data_home.join("nautilus-python/extensions/ssx-shell.py");

    let compiled = Command::new(&py).args(["-m", "py_compile"]).arg(&ext).output().expect("run");
    assert!(compiled.status.success(), "py_compile: {}", String::from_utf8_lossy(&compiled.stderr));

    let stubs = s.tmp.path().join("stubs/gi");
    fs::create_dir_all(&stubs).expect("mkdir");
    fs::write(stubs.join("__init__.py"), STUB_GI_INIT).expect("write");
    fs::write(stubs.join("repository.py"), STUB_REPO).expect("write");
    let driver = s.tmp.path().join("driver.py");
    fs::write(&driver, DRIVER).expect("write");

    let out = Command::new(&py)
        .arg(&driver)
        .arg(s.tmp.path().join("stubs"))
        .arg(&ext)
        .output()
        .expect("run driver");
    assert!(out.status.success(), "driver failed: {}", String::from_utf8_lossy(&out.stderr));
    let json = String::from_utf8_lossy(&out.stdout).into_owned();
    // Tiny structural checks without a JSON dependency: parse with python again for exactness.
    let check = Command::new(&py).arg("-c").arg(CHECK).arg(&json).output().expect("run check");
    assert!(check.status.success(), "{}\n{}", String::from_utf8_lossy(&check.stderr), json);
}

const CHECK: &str = r#"
import json, sys
r = json.loads(sys.argv[1])
exe = "/opt/it's a $HOME \"dir\"/日本/ssx"
hostile = ["/tmp/a b.png", "/tmp/it's -x.png", "/tmp/$(id) `id`.PNG", "/tmp/new\nline.png"]

assert r["images"]["labels"] == ["Upload with ssx"], r["images"]["labels"]          # edit needs a single file
assert r["images"]["launched"] == [[[exe, "post-file", "--"] + hostile, ["close_fds", "start_new_session"]]]
assert r["one_image"]["labels"] == ["Upload with ssx", "Edit image with ssx"]
assert r["one_image"]["launched"][1][0] == [exe, "edit", "--", hostile[2]]
assert r["one_video"]["labels"] == ["Upload with ssx", "Upload video with ssx"]
assert r["text"]["labels"] == ["Upload with ssx"]
assert r["extless_image_by_mime"]["labels"] == ["Upload with ssx", "Edit image with ssx"]
assert r["directory"]["labels"] == ["Upload with ssx"]
assert r["remote"]["labels"] == [] and r["empty"]["labels"] == []
"#;

#[test]
fn extension_falls_back_to_nautilus_3_when_4_is_unavailable() {
    let Some(py) = python() else {
        eprintln!("skipping: python3 is not installed");
        return;
    };
    let s = Sandbox::linux();
    Nautilus::with_variant(NautilusVariant::Extension).install(&s.ctx).expect("install");
    let ext = s.ctx.data_home.join("nautilus-python/extensions/ssx-shell.py");
    let stubs = s.tmp.path().join("stubs/gi");
    fs::create_dir_all(&stubs).expect("mkdir");
    fs::write(
        stubs.join("__init__.py"),
        STUB_GI_INIT.replace("STUB_ONLY_3 = False", "STUB_ONLY_3 = True"),
    )
    .expect("write");
    fs::write(stubs.join("repository.py"), STUB_REPO).expect("write");
    let script = "import importlib.util, sys\nsys.path.insert(0, sys.argv[1])\nspec = importlib.util.spec_from_file_location('e', sys.argv[2])\nm = importlib.util.module_from_spec(spec)\nspec.loader.exec_module(m)\nprint('loaded')\n";
    let out = Command::new(py)
        .args(["-c", script])
        .arg(s.tmp.path().join("stubs"))
        .arg(&ext)
        .output()
        .expect("run");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
