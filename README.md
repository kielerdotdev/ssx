# ssx

A ShareX-class screenshot, annotation, recording and upload tool written in Rust.
Windows first, then Linux (X11 and Wayland: GNOME, KDE, sway, Hyprland), macOS later.

* **Capture** fullscreen, monitor, window or a region picked on a frozen-screen overlay.
* **HDR → SDR on Windows** with a tone-mapper that leaves ordinary desktop content
  byte-identical to an SDR screenshot (see [HDR](#hdr)).
* **Editor** with the ShareX tool set (shapes, arrows, text, balloons, numbered steps, blur,
  pixelate, magnify, spotlight, highlighter, stickers, crop/cut-out, effects), undo/redo and a
  GPU-accelerated canvas.
* **Screen recording** to MP4/WebM/GIF with system audio and microphone, hardware encoders first.
* **Upload** through ShareX `.sxcu` custom uploaders, Imgur, S3-compatible storage and more,
  with post-screenshot / post-file / post-video workflows, history and right-click integration.

> **Status: pre-release.** Everything below is implemented and tested in a Linux sandbox. Large
> parts have **not** run on real Windows, macOS, GNOME, KDE or Hyprland yet. The
> [verification matrix](#verification-status) says exactly what has been proven and how.

## Try it

```sh
cargo build --release                      # needs the system libraries listed in .github/workflows/ci.yml
ssx doctor                                 # what backend, monitors, HDR state, helpers were found
ssx capture fullscreen -o shot.png
ssx capture region                         # interactive overlay
ssx capture region --rect 100,100,800,600 --upload
ssx post-file -- a.png b.mp4               # what the right-click entries call
ssx record --seconds 10 -o clip.mp4
ssx daemon start                           # tray icon, hotkeys, batching right-click uploads
ssx hotkeys print --target sway            # compositor keybinding snippets (sway/Hyprland/GNOME/KDE)
ssx shell install --dry-run                # right-click entries for your file manager
ssx-settings-ui                            # settings, workflows, uploaders, history
```

## Architecture

```
                 ┌────────── ssx-app (tray daemon) ──────────┐   ssx-cli (`ssx`)
 hotkeys ───────▶│ supervisor · coalescer · tray · hot-reload │◀── IPC (ssx-ipc)
                 └───────────────┬───────────────────────────┘
                                 ▼
 ssx-core ── workflow engine (capture → edit → save/copy → upload → after-upload) ── history (SQLite)
     │                 │                  │
     ▼                 ▼                  ▼
 ssx-services    helpers (own processes: crash-isolated)      ssx-upload (.sxcu, Imgur, S3, OAuth)
     │            ├─ ssx-overlay       region selection
     ▼            ├─ ssx-editor-ui     egui editor  ── ssx-editor (scene graph) ── ssx-imgfx
 ssx-platform     └─ ssx-settings-ui   egui settings/history
     │  per-monitor HDR→SDR, then stitch
     ▼
 ssx-capture (trait) ◀── ssx-capture-win · -x11 · -wayland (wlroots) · -portal (GNOME/KDE)
 ssx-hdr (CPU reference tone-mapper) ── ssx-gpu (wgpu shaders, parity-tested against it)
 ssx-record (frame sources, audio, ffmpeg/gifski encoders)   ssx-hotkeys   ssx-shell
```

Design rules that shape the code:

* **Backends return native frames** (float scRGB on an HDR display). `ssx-platform` tone-maps
  *each monitor* and only then stitches, so mixed HDR + SDR setups are correct.
* **UIs run as helper processes** (overlay, editor, settings) so a crash or a Wayland quirk
  can never take the tray daemon down.
* **Everything that touches the OS sits behind a trait with a test double**, and pure logic is
  kept windowing-free so it can be unit- and property-tested.
* Nothing edits your compositor or shell config silently; generators print, `--apply` is opt-in.

Per-crate READMEs explain each component; start with `crates/ssx-core/README.md`.
Engineering rules are in [`docs/engineering-standards.md`](docs/engineering-standards.md) and
the original design in [`PLAN.md`](PLAN.md).

## HDR

On an HDR Windows display the desktop is composed as linear float scRGB where SDR white sits at
your "SDR content brightness" (for example 200 nits), not at 1.0. Capturing that as 8-bit clips
everything above ~40 % brightness. ssx captures the float frame, scales SDR white to 1.0, gamut-maps
negatives, rolls off highlights on luminance (hue preserved), applies the sRGB curve and dithers.

* **Default = "faithful"**: every pixel at or below SDR white (all normal UI) is **byte-identical**
  to an SDR screenshot, at any brightness setting; highlights keep their colour but lose detail,
  because SDR white is also the top of 8-bit output.
* **"Preserve highlights"** lowers the knee so highlight detail survives, at the cost of UI white
  landing slightly below 255. Operators: Reinhard-extended, BT.2390, ACES fit, clip.
* The settings window previews all of this on a synthetic HDR test scene.
* macOS already delivers SDR from ScreenCaptureKit; Linux HDR capture is out of scope for now.

## Verification status

"Live" = ran against the real thing in the Linux dev sandbox. "Mock" = against a scripted stand-in.
"Compile" = type-checked/linted for the target only.

| Area | Status |
|---|---|
| Core, workflows, settings, history, filename patterns | Live (hundreds of tests) |
| Uploads: `.sxcu` engine, S3 SigV4 (AWS test vectors), retry, OAuth/PKCE | Live against local mock servers; **never against real hosts** |
| HDR tone-mapper (CPU) and GPU shaders | Live; GPU parity verified on software Vulkan (lavapipe) only, **no real GPU** |
| Editor engine + editor window + settings window | Live under Xvfb and headless sway, golden-image tests |
| Region overlay | Live: X11 (Xvfb) and sway (layer-shell); GNOME/KDE/Hyprland **fixtures only** |
| X11 capture | Live (Xvfb) |
| sway capture | Live (headless sway, cross-checked against `grim`); Hyprland **fixtures only** |
| GNOME / KDE capture (portal, KWin) | **Mock D-Bus only** — no real session yet |
| Recording: X11, wlroots, PipeWire audio, encoders | Live; portal ScreenCast against a mock portal + real PipeWire |
| Tray daemon, IPC, batching, hot-reload | Live under Xvfb and sway; tray against a mock StatusNotifier watcher |
| Hotkey generators: sway, GNOME (gsettings) | Live; Hyprland and KDE **from documentation** |
| File-manager integrations | Golden files + real `sh`/`dash`/`bash`/GLib argument passing; **no real file manager** |
| **Windows** (capture, WGC/DDA/GDI, HDR, overlay, tray, registry verbs, WASAPI) | **Compile + lint only — never run** |
| **macOS** | Not implemented (compiles as stubs) |
| **Static FFmpeg embedding** | Scripts and CI job written, **never run** (sandbox cannot fetch FFmpeg sources) |

The manual checklists for everything marked fixtures/mock/compile are in the READMEs of
`ssx-app`, `ssx-overlay`, `ssx-capture-portal`, `ssx-capture-win` and `ssx-record`.

## Licence

GPL-3.0-or-later. Third-party notices: each crate's `THIRD_PARTY.md` where fonts or data are
bundled (the editor bundles Liberation Sans, OFL-1.1).
