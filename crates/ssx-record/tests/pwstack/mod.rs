//! Test support shared by the PipeWire tests: a private D-Bus session bus and a private,
//! headless `pipewire` + `wireplumber` pair (no sound card, no desktop needed).
#![allow(dead_code)] // each test binary uses a different subset

use std::{
    io::{BufRead, BufReader},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

// ---- private bus + pipewire ----------------------------------------------------------------

pub struct Bus {
    child: Child,
    pub address: String,
    dir: tempfile::TempDir,
}

impl Bus {
    pub fn start() -> Option<Bus> {
        let dir = tempfile::Builder::new().prefix("ssx-bus").tempdir().ok()?;
        let socket = dir.path().join("bus");
        let config = format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
             <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
            socket.display()
        );
        let cfg = dir.path().join("bus.conf");
        std::fs::write(&cfg, config).ok()?;
        let mut child = match Command::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address=1")
            .arg(format!("--config-file={}", cfg.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run dbus-daemon ({e})");
                return None;
            }
        };
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        Some(Bus { child, address: line.trim().to_owned(), dir })
    }

    pub fn service(&self, name: &str) -> zbus::connection::Builder<'static> {
        zbus::connection::Builder::address(self.address.as_str())
            .expect("address")
            .name(name.to_owned())
            .expect("name")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = &self.dir;
    }
}

pub struct PipeWire {
    pw: Child,
    wp: Child,
    pub dir: tempfile::TempDir,
}

impl PipeWire {
    pub fn start(bus: &Bus) -> Option<PipeWire> {
        for prog in ["pipewire", "wireplumber"] {
            if Command::new(prog)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err()
            {
                eprintln!("SKIP: `{prog}` is not installed");
                return None;
            }
        }
        let dir = tempfile::Builder::new().prefix("ssx-pw").tempdir().ok()?;
        let env = |c: &mut Command| {
            c.env("XDG_RUNTIME_DIR", dir.path())
                .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
                .env_remove("PIPEWIRE_REMOTE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        };
        let mut c = Command::new("pipewire");
        env(&mut c);
        let pw = c.spawn().ok()?;
        let socket = dir.path().join("pipewire-0");
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut c = Command::new("wireplumber");
        env(&mut c);
        let wp = c.spawn().ok()?;
        // wireplumber needs a moment to start its policy scripts.
        std::thread::sleep(Duration::from_millis(2500));
        Some(PipeWire { pw, wp, dir })
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("pipewire-0")
    }
}

impl Drop for PipeWire {
    fn drop(&mut self) {
        let _ = self.wp.kill();
        let _ = self.wp.wait();
        let _ = self.pw.kill();
        let _ = self.pw.wait();
    }
}

pub fn connect_fd(socket: &Path) -> OwnedFd {
    OwnedFd::from(UnixStream::connect(socket).expect("connect to the private pipewire"))
}
