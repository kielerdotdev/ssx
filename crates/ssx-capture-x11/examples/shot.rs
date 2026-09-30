//! Takes a screenshot of an X11 (or XWayland) display and saves it as a PNG.
//!
//! ```text
//! cargo run -p ssx-capture-x11 --example shot -- [OPTIONS] [OUT.png]
//!
//!   --list              print features, monitors and windows, then exit
//!   --display <NAME>    X display (default: $DISPLAY)
//!   --monitor <ID>      capture one monitor (name from --list)
//!   --window <ID>       capture one window (id from --list, e.g. 0x1a00003)
//!   --region X,Y,W,H    capture a rectangle of the desktop
//!   --cursor            include the mouse cursor
//!   --no-shm            force the plain GetImage path
//!   --root              crop windows from the root window even if a compositor runs
//! ```
//!
//! With no capture option the whole desktop is captured. Output defaults to `shot.png`.

#[cfg(all(unix, not(target_vendor = "apple")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ssx_capture::{CaptureBackend, CaptureOptions};
    use ssx_capture_x11::{WindowCaptureMode, X11Capture, X11Config};
    use ssx_types::Rect;

    let mut args = std::env::args().skip(1);
    let mut config = X11Config::default();
    let (mut list, mut cursor) = (false, false);
    let (mut monitor, mut window, mut region): (Option<String>, Option<String>, Option<Rect>) =
        (None, None, None);
    let mut out = String::from("shot.png");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => list = true,
            "--cursor" => cursor = true,
            "--no-shm" => config.use_shm = false,
            "--root" => config.window_capture = WindowCaptureMode::Root,
            "--display" => config.display = Some(args.next().ok_or("--display needs a name")?),
            "--monitor" => monitor = Some(args.next().ok_or("--monitor needs an id")?),
            "--window" => window = Some(args.next().ok_or("--window needs an id")?),
            "--region" => {
                let spec = args.next().ok_or("--region needs X,Y,W,H")?;
                let n: Vec<&str> = spec.split(',').collect();
                let [x, y, w, h] = n[..] else { return Err("--region expects X,Y,W,H".into()) };
                region = Some(Rect::new(x.parse()?, y.parse()?, w.parse()?, h.parse()?));
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}").into());
            }
            path => path.clone_into(&mut out),
        }
    }

    let cap = X11Capture::with_config(config)?;
    if list {
        println!("features: {:?}", cap.features()?);
        for m in cap.monitor_details()? {
            println!(
                "monitor {:<12} {:?} scale {} refresh {:?} primary {} rotation {:?} outputs {:?}",
                m.monitor.id,
                m.monitor.rect,
                m.monitor.scale_factor,
                m.monitor.refresh_hz,
                m.monitor.primary,
                m.rotation,
                m.outputs
            );
        }
        for w in cap.windows()? {
            println!(
                "window {:<10} {:?} {:?} {:?}{}{}",
                w.id,
                w.title,
                w.app_name,
                w.rect,
                if w.minimized { " [minimised]" } else { "" },
                if w.focused { " [focused]" } else { "" }
            );
        }
        return Ok(());
    }

    let opts = CaptureOptions { include_cursor: cursor };
    let started = std::time::Instant::now();
    let frame = if let Some(id) = window {
        cap.capture_window(&id, &opts)?
    } else if let Some(id) = monitor {
        cap.capture_monitor(&id, &opts)?
    } else if let Some(r) = region {
        cap.capture_region(r, &opts)?
    } else {
        cap.capture_desktop(&opts)?
    };
    let took = started.elapsed();
    frame.save(&out)?;
    println!(
        "{}x{} at {:?} -> {out} (capture took {took:?})",
        frame.width(),
        frame.height(),
        frame.origin
    );
    Ok(())
}

#[cfg(not(all(unix, not(target_vendor = "apple"))))]
fn main() {
    eprintln!("the X11 backend is not available on this platform");
}
