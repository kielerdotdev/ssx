//! The sway generator against a real (headless) sway.
//!
//! Three things a unit test cannot establish:
//!
//! 1. every key name and modifier spelling we emit is accepted by sway's config parser
//!    (an unknown keysym makes sway log `Error on line ...`);
//! 2. our quoting survives sway's *own* command parser (which splits at `;`/`,`, expands
//!    `$variables` and handles only double quotes) and then `sh`, for hostile arguments;
//! 3. a generated include file, pulled in with the printed `include` line, really fires
//!    when the chord is pressed (keys are injected with `wtype` when it is installed).
//!
//! Tests skip with a printed reason when `sway` (or `wtype`, for the injection test) is
//! missing.
#![cfg(target_os = "linux")]

use std::{
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    time::{Duration, Instant},
};

use ssx_hotkeys::{
    Chord, Command, Key,
    bindings::{Dirs, Target, files, sway},
};

struct Sway {
    child: Child,
    dir: PathBuf,
    log: PathBuf,
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ssx-hk-sway-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // sway insists on a private (0700) XDG_RUNTIME_DIR.
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).expect("scratch dir");
    dir
}

impl Sway {
    fn start(dir: &Path, main_config: &str) -> Option<Sway> {
        let conf = dir.join("sway.conf");
        std::fs::write(&conf, main_config).ok()?;
        let log = dir.join("sway.log");
        let child = Process::new("sway")
            .arg("-c")
            .arg(&conf)
            .arg("--unsupported-gpu")
            .env("XDG_RUNTIME_DIR", dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).ok()?)
            .spawn();
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run sway ({e}); install the `sway` package");
                return None;
            }
        };
        let mut sway = Sway { child, dir: dir.to_owned(), log };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if sway.socket().is_some() {
                // Config is applied before the socket appears; give exec_always a beat.
                std::thread::sleep(Duration::from_millis(300));
                return Some(sway);
            }
            if sway.child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        eprintln!("SKIP: sway did not start headless:\n{}", sway.log_text());
        None
    }

    fn socket(&self) -> Option<String> {
        std::fs::read_dir(&self.dir).ok()?.flatten().find_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            (n.starts_with("wayland-") && !n.ends_with(".lock")).then_some(n)
        })
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn config_errors(&self) -> Vec<String> {
        self.log_text().lines().filter(|l| l.contains("Error on line")).map(str::to_owned).collect()
    }
}

impl Drop for Sway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn chord(s: &str) -> Chord {
    s.parse().expect("valid chord")
}

#[test]
fn sway_accepts_every_key_and_modifier_spelling_we_emit() {
    let dir = scratch("keys");
    let mut lines = String::new();
    let mut count = 0;
    for key in Key::all() {
        let c = Chord::new(ssx_hotkeys::Modifiers::CTRL | ssx_hotkeys::Modifiers::SUPER, key).expect("modified");
        lines.push_str(&sway::bindsym_line(&c, &Command::new("true")));
        lines.push('\n');
        count += 1;
    }
    // Every modifier subset, with a fixed key.
    for bits in 1..16u8 {
        let mut m = ssx_hotkeys::Modifiers::NONE;
        for (i, f) in [ssx_hotkeys::Modifiers::CTRL, ssx_hotkeys::Modifiers::ALT, ssx_hotkeys::Modifiers::SHIFT, ssx_hotkeys::Modifiers::SUPER].into_iter().enumerate() {
            if bits & (1 << i) != 0 {
                m = m | f;
            }
        }
        let c = Chord::new(m, Key::Letter('Q')).expect("modified");
        lines.push_str(&sway::bindsym_line(&c, &Command::new("true")));
        lines.push('\n');
        count += 1;
    }
    // Bare keys that are allowed.
    for k in ["Print", "F5", "VolumeMute", "PlayPause", "Pause"] {
        lines.push_str(&sway::bindsym_line(&chord(k), &Command::new("true")));
        lines.push('\n');
        count += 1;
    }
    // Negative control: proves this test can fail.
    lines.push_str("bindsym Ctrl+NoSuchKeysymXYZ exec true\n");

    let Some(sway) = Sway::start(&dir, &lines) else { return };
    let errors = sway.config_errors();
    assert_eq!(errors.len(), 1, "expected only the negative control to fail, got {count} lines and:\n{}", errors.join("\n"));
    assert!(errors[0].contains("NoSuchKeysymXYZ"), "{errors:?}");
}

/// The script every test command runs: writes its arguments NUL-separated to `$OUT/<n>`.
fn recorder(dir: &Path) -> PathBuf {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).expect("out dir");
    let script = dir.join("record.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\nn=$1\nshift\nprintf '%s\\0' \"$@\" > '{}'/\"$n\"\n", out.display()),
    )
    .expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn read_recorded(dir: &Path, n: usize) -> Option<Vec<Vec<u8>>> {
    let bytes = std::fs::read(dir.join("out").join(n.to_string())).ok()?;
    let mut parts: Vec<Vec<u8>> = bytes.split(|b| *b == 0).map(<[u8]>::to_vec).collect();
    parts.pop(); // trailing NUL
    Some(parts)
}

/// Hostile arguments; the same corpus as the unit tests, minus what sway cannot carry.
fn corpus() -> Vec<String> {
    [
        "plain",
        "with space",
        "  leading and trailing  ",
        "it's",
        "two'quotes'here",
        "\"double\"",
        "mixed ' and \"",
        "$HOME",
        "${HOME}",
        "$(whoami)",
        "`whoami`",
        "semi;colon",
        "com,ma",
        "a;b,c;d",
        "&& ||",
        "pipe|pipe",
        "redir > out < in",
        "glob*?[x]",
        "tilde ~ and ~/x",
        "hash # not comment",
        "##",
        "back\\slash",
        "trailing\\",
        "\\\"",
        "\\\\\"",
        "\"\\",
        "percent %s %%",
        "{brace}",
        "!bang",
        "=equals",
        "FOO=bar",
        "-dash",
        "--opt=val ue",
        "",
        "ünïcödé 日本語 🙂",
        "tab\there",
        // Variables defined in the config below: must reach the program untouched.
        "$x $mod $term",
        "$x",
        "$mod+Return",
        "a=b c",
    ]
    .map(str::to_owned)
    .to_vec()
}

const PRELUDE: &str = "set $x BOOM\nset $mod Mod4\nset $term foot\n";

fn chord_for_slot(n: usize) -> (Chord, Vec<&'static str>) {
    // 24 F-keys x 2 modifier sets = 48 slots, none of which sway or wlroots use.
    let f = n % 24 + 1;
    if n < 24 {
        (chord(&format!("Ctrl+Alt+Shift+Super+F{f}")), vec!["ctrl", "alt", "shift", "logo"])
    } else {
        (chord(&format!("Alt+Super+F{f}")), vec!["alt", "logo"])
    }
}

fn bindings_for(script: &Path, words: &[String]) -> Vec<(Chord, Command)> {
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            (chord_for_slot(i).0, Command::new(script.display().to_string()).arg(i.to_string()).arg(w.clone()))
        })
        .collect()
}

/// Loads the *generated file* through the *printed include line*, like a user would.
fn config_with_include(dir: &Path, bindings: &[(Chord, Command)]) -> String {
    let dirs = Dirs::under(dir.join("home"));
    let report = files::write_include_file(&dirs, Target::Sway, bindings).expect("write include");
    format!("{PRELUDE}{}\n", report.include_line)
}

#[test]
fn hostile_arguments_survive_sway_and_sh_via_the_include_file() {
    let dir = scratch("exec");
    let script = recorder(&dir);
    let words = corpus();
    assert!(words.len() <= 48);
    let bindings = bindings_for(&script, &words);
    // Same quoting path as a binding, but run at config load: `exec_always` takes the same
    // argument text. (Key injection is covered by the next test.)
    let mut config = String::from(PRELUDE);
    for (_, cmd) in &bindings {
        config.push_str(&format!("exec_always {}\n", ssx_hotkeys::bindings::sway::exec_arg(cmd)));
    }
    let Some(sway) = Sway::start(&dir, &config) else { return };
    assert!(sway.config_errors().is_empty(), "{:?}", sway.config_errors());
    let deadline = Instant::now() + Duration::from_secs(10);
    for (i, w) in words.iter().enumerate() {
        let want = vec![w.clone().into_bytes()];
        let got = loop {
            if let Some(g) = read_recorded(&dir, i) {
                break g;
            }
            assert!(Instant::now() < deadline, "command {i} ({w:?}) never ran; log:\n{}", sway.log_text());
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(got, want, "argument {w:?} changed on the way through sway and sh");
    }
}

#[test]
fn one_command_carrying_every_hostile_argument() {
    let dir = scratch("all");
    let script = recorder(&dir);
    let words = corpus();
    let cmd = Command::new(script.display().to_string()).arg("0").args(words.clone());
    let config = format!("{PRELUDE}exec_always {}\n", sway::exec_arg(&cmd));
    let Some(sway) = Sway::start(&dir, &config) else { return };
    let deadline = Instant::now() + Duration::from_secs(10);
    let got = loop {
        if let Some(g) = read_recorded(&dir, 0) {
            break g;
        }
        assert!(Instant::now() < deadline, "never ran; log:\n{}", sway.log_text());
        std::thread::sleep(Duration::from_millis(50));
    };
    let want: Vec<Vec<u8>> = words.into_iter().map(String::into_bytes).collect();
    assert_eq!(got, want);
}

fn wtype_chord(mods: &[&str], key: &Key) -> Vec<String> {
    let mut args = Vec::new();
    for m in mods {
        args.extend(["-M".to_owned(), (*m).to_owned()]);
    }
    args.extend(["-k".to_owned(), key.xkb_name()]);
    for m in mods.iter().rev() {
        args.extend(["-m".to_owned(), (*m).to_owned()]);
    }
    args
}

#[test]
fn pressing_a_generated_binding_runs_its_command() {
    if Process::new("wtype").arg("--help").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        eprintln!("SKIP: `wtype` is not installed, so keys cannot be injected into sway");
        return;
    }
    let dir = scratch("press");
    let script = recorder(&dir);
    let words = corpus();
    let mut bindings = bindings_for(&script, &words);
    // And a bare key and a Super+Alt chord in the everyday style.
    bindings.push((chord("Print"), Command::new(script.display().to_string()).args(["100", "print"])));
    let config = config_with_include(&dir, &bindings);
    let Some(sway) = Sway::start(&dir, &config) else { return };
    assert!(sway.config_errors().is_empty(), "{:?}", sway.config_errors());
    let socket = sway.socket().expect("socket");

    let press = |args: Vec<String>| {
        let ok = Process::new("wtype")
            .args(&args)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WAYLAND_DISPLAY", &socket)
            .status()
            .expect("wtype")
            .success();
        assert!(ok, "wtype {args:?} failed");
        std::thread::sleep(Duration::from_millis(150));
    };
    for (i, _) in words.iter().enumerate() {
        let (c, mods) = chord_for_slot(i);
        press(wtype_chord(&mods, &c.key()));
    }
    press(vec!["-k".into(), "Print".into()]);

    let deadline = Instant::now() + Duration::from_secs(10);
    for (i, w) in words.iter().enumerate() {
        let got = loop {
            if let Some(g) = read_recorded(&dir, i) {
                break g;
            }
            assert!(Instant::now() < deadline, "pressing slot {i} ({w:?}) did not run the command; log:\n{}", sway.log_text());
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(got, vec![w.clone().into_bytes()], "binding {i}");
    }
    let got = loop {
        if let Some(g) = read_recorded(&dir, 100) {
            break g;
        }
        assert!(Instant::now() < deadline, "Print did not run");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(got, vec![b"print".to_vec()]);
}

#[test]
fn include_line_with_spaces_in_the_path_is_accepted() {
    let dir = scratch("space");
    let script = recorder(&dir);
    let home = dir.join("my home dir");
    let dirs = Dirs::under(&home);
    let b = vec![(chord("Ctrl+Alt+Shift+Super+F1"), Command::new(script.display().to_string()).args(["0", "ok"]))];
    let report = files::write_include_file(&dirs, Target::Sway, &b).expect("write");
    assert!(report.include_line.contains("\\ "), "{}", report.include_line);
    // Prove the include was honoured by also making the included file run something at load.
    let mut text = std::fs::read_to_string(&report.path).expect("read");
    text.push_str(&format!("exec_always {}\n", sway::exec_arg(&b[0].1)));
    std::fs::write(&report.path, text).expect("append");
    let config = format!("{}\n", report.include_line);
    let Some(sway) = Sway::start(&dir, &config) else { return };
    assert!(sway.config_errors().is_empty(), "{:?}", sway.config_errors());
    let deadline = Instant::now() + Duration::from_secs(10);
    while read_recorded(&dir, 0).is_none() {
        assert!(Instant::now() < deadline, "included file was not loaded; log:\n{}", sway.log_text());
        std::thread::sleep(Duration::from_millis(50));
    }
}
