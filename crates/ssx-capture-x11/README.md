# ssx-capture-x11

X11 / XWayland screen capture for ssx, implementing `ssx_capture::CaptureBackend` on the
pure-Rust [`x11rb`](https://docs.rs/x11rb). No permission prompts, no `unsafe`, no C
libraries. The crate is empty on Windows and macOS.

```rust
use ssx_capture::{CaptureBackend, CaptureOptions};
use ssx_capture_x11::X11Capture;

let cap = X11Capture::connect()?;                       // uses $DISPLAY
let frame = cap.capture_desktop(&CaptureOptions { include_cursor: true })?;
frame.save("shot.png")?;
```

Try it: `cargo run -p ssx-capture-x11 --example shot -- --list`, then `-- --window 0x... out.png`.

## What it does

| Feature | Mechanism |
|---|---|
| Pixels | `GetImage` on the root window. **MIT-SHM** when local; plain banded `GetImage` otherwise |
| Monitors | RandR 1.5 `GetMonitors` -> RandR 1.2 CRTCs -> one monitor covering the root |
| Refresh / rotation | Active CRTC's mode (`dot_clock / (htotal * vtotal)`, interlace/doublescan corrected) |
| Scale factor | Best effort, one global value: XSETTINGS `Xft/DPI` -> `Xft.dpi` (`RESOURCE_MANAGER`) -> `Gdk/WindowScalingFactor` -> 1.0 |
| Cursor | XFixes `GetCursorImage`, premultiplied-alpha blend, hotspot aware, clipped |
| Windows | EWMH: `_NET_CLIENT_LIST_STACKING` (front-to-back), `_NET_WM_NAME`/`WM_NAME`, `WM_CLASS`, `_NET_WM_STATE_HIDDEN`, `_NET_ACTIVE_WINDOW`, `_NET_FRAME_EXTENTS` (rect includes decorations). Without EWMH: the root's mapped children |
| Window capture | XComposite `NameWindowPixmap` when a compositing manager runs, else crop from the root |

### MIT-SHM without SysV

Classic MIT-SHM needs `shmget`/`shmat` (libc, `unsafe`). MIT-SHM 1.2 can attach a file
descriptor instead: we create a `memfd`, pass it with `ShmAttachFd`, let the server write
`ShmGetImage` output into it, and `pread` the result. Pixels skip the socket; safe Rust
only. Linux/Android only (`memfd_create`); TCP displays cannot pass fds so they use the
plain path automatically, as does any server without SHM 1.2. If SHM fails at run time the
backend retries plainly and stops using SHM for that session.

### Pixel formats

Frames are `Bgra8`, sRGB, **always opaque**. Decoded generically from the server's pixmap
format and visual masks: depth 15/16/24/30/32, packed 24 bpp, RGB- or BGR-ordered masks,
big-endian servers (unit-tested with synthetic buffers; no big-endian server was
available). Depth-32 alpha is dropped (a premultiplied window alpha over black). Palette
(PseudoColor/StaticColor) screens return `X11Error::UnsupportedVisual`.

### Large images

Images are fetched in bands of rows (default 4 MiB per plain reply, capped by the server's
maximum request size; 16 MiB per SHM segment; at least one row) so multi-monitor 8K desktops
work. `X11Config::max_chunk_bytes` overrides both.

## Semantics and caveats

* **Coordinates**: the root window is the virtual desktop, origin (0,0); X has no negative
  monitor positions. Gaps between monitors are captured as-is (usually black). Only the
  screen in `DISPLAY` (`:0.1` = screen 1) is captured.
* **`capture_region` clips** to the screen instead of failing (X rejects reads outside the
  drawable); the frame's `rect()` is the intersection. No overlap -> `InvalidRegion`.
* **Window capture without a compositor** crops the window rectangle from the root:
  anything on top of the window is captured too, and minimised/unmapped windows return an
  error. With a compositor the window is read from its off-screen pixmap (occlusion free,
  works when partly off-screen, frame included, position may be negative). `WindowCaptureMode`
  can force either behaviour. Client-side shadows (`_GTK_FRAME_EXTENTS`) are not handled.
* **Scale factor** is informational: X never scales captured pixels. Per-monitor scales
  don't exist on X11.
* **Server restart**: a dead connection yields an error; the next call reconnects.
* `x11rb` 0.14 panics (arithmetic overflow, debug builds) on absurd display numbers such as
  `:59999`; realistic `DISPLAY` values are fine.

## Tests

`cargo test -p ssx-capture-x11` runs unit tests (pixel decoding incl. big-endian/565/555/
30-bit, XSETTINGS/Xft parsing, cursor blending, mode maths) and integration tests against
real `Xvfb` instances started with `-displayfd` (no display collisions). Covered: exact
pixels for root/region/window/monitor on SHM and plain paths, banded capture with 1-row
bands, a 4096x2048 desktop across default band boundaries, depths 15/16/24/30, a depth-32
ARGB window, forced no-SHM/no-Composite/no-RandR servers, user-defined RandR monitors,
XComposite occlusion-free capture (the test acts as a minimal compositor), cursor
compositing with a real ARGB cursor, window listing with fake EWMH properties, error cases
and Xvfb restart. Tests print `SKIP: ...` and pass when `Xvfb` is not installed.
Depth 32 as a *screen* depth is refused by Xvfb, so 32-bit is covered via an ARGB window.
