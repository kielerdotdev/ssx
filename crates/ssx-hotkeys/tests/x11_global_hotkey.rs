//! The `global-hotkey` backend on a real X server: register a chord, press it with
//! `xdotool` (XTEST), receive the events. Runs under `Xvfb`; skips with a printed reason
//! when `Xvfb` or `xdotool` is missing.
//!
//! This file deliberately contains a single test: `global-hotkey` reports events on a
//! process-wide channel and reads `$DISPLAY` from the process environment.
#![cfg(all(unix, not(target_vendor = "apple")))]

use std::{
    io::{BufRead, BufReader},
    process::{Child, Command as Process, Stdio},
    time::Duration,
};

use ssx_hotkeys::{Chord, GlobalHotkeys, HotkeyError, HotkeyId, HotkeyManager, HotkeyState};

struct Xvfb(Child, String);

impl Xvfb {
    fn start() -> Option<Xvfb> {
        let mut child = match Process::new("Xvfb")
            .args(["-displayfd", "1", "-screen", "0", "640x480x24", "-noreset", "-ac", "-nolisten", "tcp"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run Xvfb ({e}); install the `xvfb` package");
                return None;
            }
        };
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        if line.trim().is_empty() {
            eprintln!("SKIP: Xvfb did not report a display");
            return None;
        }
        Some(Xvfb(child, format!(":{}", line.trim())))
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn xdotool(display: &str, args: &[&str]) -> bool {
    Process::new("xdotool")
        .args(args)
        .env("DISPLAY", display)
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn registered_chord_produces_press_and_release_events() {
    if Process::new("xdotool").arg("version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        eprintln!("SKIP: xdotool is not installed");
        return;
    }
    let Some(xvfb) = Xvfb::start() else { return };
    // SAFETY: this is the only test in the binary and no other thread is running yet, so
    // nothing can read the environment concurrently.
    unsafe { std::env::set_var("DISPLAY", &xvfb.1) };

    let mut mgr = GlobalHotkeys::new().expect("manager");
    let region = HotkeyId::new("capture-region").unwrap();
    let screen = HotkeyId::new("capture-screen").unwrap();
    let chord_a: Chord = "Ctrl+Shift+F9".parse().unwrap();
    let chord_b: Chord = "Alt+Super+r".parse().unwrap();

    mgr.register(region.clone(), chord_a).expect("register region");
    mgr.register(screen.clone(), chord_b).expect("register screen");
    assert_eq!(mgr.registered().len(), 2);

    // Misuse is rejected without touching the server.
    assert!(matches!(mgr.register(region.clone(), "Ctrl+F1".parse().unwrap()), Err(HotkeyError::DuplicateId(_))));
    assert!(matches!(
        mgr.register(HotkeyId::new("other").unwrap(), chord_a),
        Err(HotkeyError::DuplicateChord { existing, .. }) if existing == region
    ));
    assert!(matches!(mgr.unregister(&HotkeyId::new("nope").unwrap()), Err(HotkeyError::UnknownId(_))));

    // Give the grab thread a moment: registration is synchronous, but xdotool starts a new
    // connection each time.
    assert!(xdotool(&xvfb.1, &["key", "ctrl+shift+F9"]), "xdotool key failed");
    let wait = Duration::from_secs(3);
    let first = mgr.events().recv_timeout(wait).expect("press event");
    assert_eq!((first.id.clone(), first.state), (region.clone(), HotkeyState::Pressed));
    let second = mgr.events().recv_timeout(wait).expect("release event");
    assert_eq!((second.id, second.state), (region.clone(), HotkeyState::Released));

    // The other chord, also with NumLock/CapsLock-style extra modifiers handled by the crate.
    assert!(xdotool(&xvfb.1, &["key", "alt+super+r"]));
    let ev = mgr.events().recv_timeout(wait).expect("second chord press");
    assert_eq!((ev.id, ev.state), (screen.clone(), HotkeyState::Pressed));
    let _release = mgr.events().recv_timeout(wait).expect("second chord release");

    // A chord nobody registered produces nothing.
    assert!(xdotool(&xvfb.1, &["key", "ctrl+shift+F10"]));
    assert!(mgr.events().recv_timeout(Duration::from_millis(400)).is_err());

    // After unregistering, the chord is dead.
    mgr.unregister(&region).expect("unregister");
    assert_eq!(mgr.registered().len(), 1);
    assert!(xdotool(&xvfb.1, &["key", "ctrl+shift+F9"]));
    assert!(mgr.events().recv_timeout(Duration::from_millis(400)).is_err(), "event after unregister");

    // ...and can be registered again.
    mgr.register(region.clone(), chord_a).expect("re-register");
    assert!(xdotool(&xvfb.1, &["key", "ctrl+shift+F9"]));
    assert_eq!(mgr.events().recv_timeout(wait).expect("press after re-register").state, HotkeyState::Pressed);
    assert!(mgr.reports_release());
    assert_eq!(mgr.backend().to_string(), "global-hotkey");
}
