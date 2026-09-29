//! Saves a screenshot with the Wayland backend.
//!
//! ```text
//! cargo run -p ssx-capture-wayland --example shot -- desktop out.png
//! cargo run -p ssx-capture-wayland --example shot -- monitor DP-1 out.png
//! cargo run -p ssx-capture-wayland --example shot -- region 100,100,640,480 out.png
//! cargo run -p ssx-capture-wayland --example shot -- window <id> out.png
//! cargo run -p ssx-capture-wayland --example shot -- list
//! ```
//!
//! Add `--cursor` anywhere to include the pointer. Uses `$WAYLAND_DISPLAY` and, for
//! windows, `$SWAYSOCK` / `$HYPRLAND_INSTANCE_SIGNATURE`.

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ssx_capture::{CaptureBackend, CaptureOptions};
    use ssx_types::Rect;

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let include_cursor = args.iter().any(|a| a == "--cursor");
    args.retain(|a| a != "--cursor");
    let opts = CaptureOptions { include_cursor };

    let cap = ssx_capture_wayland::WaylandCapture::detect()?;
    eprintln!("backend: {} ({:?})", cap.name(), cap.capabilities());

    let usage =
        "usage: shot desktop OUT | monitor ID OUT | region X,Y,W,H OUT | window ID OUT | list";
    let frame = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["list"] => {
            for m in cap.monitors()? {
                println!(
                    "monitor {} rect={:?} scale={} refresh={:?} primary={}",
                    m.id, m.rect, m.scale_factor, m.refresh_hz, m.primary
                );
            }
            if cap.capabilities().enumerate_windows {
                for w in cap.windows()? {
                    println!(
                        "window {} {:?} app={:?} rect={:?} focused={} hidden={}",
                        w.id, w.title, w.app_name, w.rect, w.focused, w.minimized
                    );
                }
            }
            return Ok(());
        }
        ["desktop", out] => (cap.capture_desktop(&opts)?, *out),
        ["monitor", id, out] => (cap.capture_monitor(id, &opts)?, *out),
        ["window", id, out] => (cap.capture_window(id, &opts)?, *out),
        ["region", r, out] => {
            let n: Vec<i64> = r.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            let [x, y, w, h] = n.as_slice() else { return Err(usage.into()) };
            let rect = Rect::new(*x as i32, *y as i32, *w as u32, *h as u32);
            (cap.capture_region(rect, &opts)?, *out)
        }
        _ => return Err(usage.into()),
    };
    let (frame, out) = frame;
    frame.save(out)?;
    eprintln!("saved {out}: {frame:?}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the Wayland backend is only available on Linux");
}
