//! Takes a screenshot on a GNOME or KDE Wayland session and saves it as a PNG.
//!
//! ```text
//! cargo run -p ssx-capture-portal --example shot -- [OPTIONS] [OUT.png]
//!
//!   --list             print the detected strategy, capabilities and monitors, then exit
//!   --monitor <ID>     capture one monitor (connector name from --list)
//!   --interactive      let the desktop show its own picker (portal; can select a window)
//!   --cursor           include the cursor (KWin only; the portal cannot)
//!   --kwin-entry       print the .desktop entry that authorises this binary with KWin
//! ```
//!
//! With no option the whole desktop is captured. Output defaults to `shot.png`.

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ssx_capture::{CaptureBackend, CaptureOptions};
    use ssx_capture_portal::{PortalCapture, kwin_desktop_entry};

    let mut args = std::env::args().skip(1);
    let (mut list, mut interactive, mut cursor, mut entry) = (false, false, false, false);
    let mut monitor: Option<String> = None;
    let mut out = String::from("shot.png");
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => list = true,
            "--interactive" => interactive = true,
            "--cursor" => cursor = true,
            "--kwin-entry" => entry = true,
            "--monitor" => monitor = Some(args.next().ok_or("--monitor needs an id")?),
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}").into());
            }
            path => path.clone_into(&mut out),
        }
    }

    if entry {
        let exe = std::env::current_exe()?;
        print!("{}", kwin_desktop_entry("ssx", &exe.display().to_string())?);
        return Ok(());
    }

    let cap = PortalCapture::detect()?;
    println!("strategy: {} ({:?})", cap.name(), cap.strategy());
    if list {
        println!("capabilities: {:?}", cap.capabilities());
        match cap.monitors() {
            Ok(monitors) => {
                for m in monitors {
                    println!(
                        "monitor {:?} {:?} rect={:?} scale={} primary={}",
                        m.id, m.name, m.rect, m.scale_factor, m.primary
                    );
                }
            }
            Err(e) => println!("monitors: {e}"),
        }
        return Ok(());
    }

    let opts = CaptureOptions { include_cursor: cursor };
    let frame = if interactive {
        cap.capture_interactive()?
    } else if let Some(id) = monitor {
        cap.capture_monitor(&id, &opts)?
    } else {
        cap.capture_desktop(&opts)?
    };
    frame.save(&out)?;
    println!(
        "saved {out}: {}x{} origin={:?} scale={}",
        frame.width(),
        frame.height(),
        frame.origin,
        frame.scale_factor
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("this example only runs on Linux");
}
