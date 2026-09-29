# ssx — a cross-platform ShareX alternative in Rust

Status: **plan / not yet implemented** · Targets: **Windows 10 2004+/11, macOS, Linux (X11 + Wayland)** · Repo was empty when this was written.

Revision 2: adds Linux + macOS, replaces the Windows-only D3D11 pipeline with a portable GPU design, and adds a prior-art survey (§3).

## 1. Goals

1. **Capture**: region, fullscreen, window, monitor, scrolling, with a ShareX-style frozen-frame region overlay.
2. **Image editor**: parity with the ShareX editor toolbar (§6.1), rendered on the GPU, identical on all three OSes.
3. **Screen recording**: MP4 (H.264/HEVC/AV1) and GIF, with system audio and mic. GPU encode, zero-copy where the platform allows.
4. **HDR → SDR screenshots on Windows** that look like what the user saw (§5). macOS gets this from the OS; Linux is later.
5. **Embedded libraries**: ffmpeg and everything else is linked in or shipped beside the binary. There is no "install ffmpeg first" step.
6. **File-manager right-click integration** on every OS and ShareX's workflow: *post screenshot*, *post file*, *post video*, with destinations, after-capture tasks and after-upload tasks.
7. **Linux**: first-class on both **X11** and **Wayland** (GNOME, KDE, wlroots compositors such as sway/Hyprland).

Non-goals for v1: a plugin system, a ShareX config importer beyond `.sxcu` custom uploaders, Linux HDR capture.

## 2. Key decisions (and why)

| Area | Decision | Reason |
|---|---|---|
| Architecture | **Portable core + per-OS backends behind traits** (`CaptureBackend`, `HotkeyBackend`, `ShellIntegration`, `AudioBackend`, `Encoder`). CI builds and tests all three OSes from M0 | Keeps the editor, workflows and uploaders 100% shared. Only capture, hotkeys, tray glue, audio and shell integration are per-OS. |
| Capture, Windows | **Windows Graphics Capture (WGC)** primary; DXGI Desktop Duplication (`IDXGIOutput5::DuplicateOutput1`) fallback; GDI `BitBlt` last resort. Own thin wrapper on the `windows` crate | WGC handles windows, monitors and float HDR formats. Third-party crates don't expose the HDR path. |
| Capture, macOS | **ScreenCaptureKit** (`SCStream` for video, `SCScreenshotManager` for stills where available). Candidate binding: the `screencapturekit` crate, else thin `objc2` wrappers | The supported modern API. Legacy `CGWindowList` capture is deprecated. |
| Capture, Linux Wayland | Layered: **xdg-desktop-portal** (`Screenshot`, `ScreenCast` + PipeWire via `ashpd`/`pipewire`) as the universal path. Optional direct paths on wlroots: `ext-image-copy-capture-v1`, falling back to `wlr-screencopy` (via `libwayshot`-style code) | The portal works on GNOME, KDE and wlroots. Direct protocols give a prompt-free, faster path on compositors that offer them. |
| Capture, Linux X11 | `x11rb` (XCB) `GetImage` via **MIT-SHM**, XRandR for monitors, XComposite for windows; PipeWire not needed | Fast and simple. No permission prompts. |
| GPU pipeline | **One tonemap/convert shader authored in WGSL**, run with `wgpu` on every OS (compiled by naga to HLSL/MSL/SPIR-V). Screenshots take a CPU-upload path first (works everywhere). Zero-copy import is added per platform behind a flag: D3D11 shared NT handle → D3D12, IOSurface → Metal, dma-buf → Vulkan | One shader source to maintain and test. Cap's recording stack already does GPU frame conversion with WGSL shaders in Rust (§3). Zero-copy import is the risky part, so it is a **M0 spike with a fallback**: on Windows, a native D3D11 shader feeding ffmpeg `d3d11va` directly. |
| UI | `winit` + `wgpu` + `egui` for editor/settings/history; tray via `tray-icon` (StatusNotifierItem on Linux); region overlay is a raw per-monitor topmost window (Linux Wayland: `wlr-layer-shell` via `smithay-client-toolkit` where supported, else a fullscreen toplevel showing the frozen frame). **M0 spike**: egui vs Slint | Same editor binary on every OS. The frozen-frame technique means the overlay needs no compositor-specific tricks on GNOME. |
| Annotation rendering | Scene graph of vector objects, drawn with `vello` (GPU) or `tiny-skia` (CPU) — **decide in M0**. Blur, pixelate, magnify and spotlight are wgpu shaders. Export renders the same scene offscreen | Non-destructive editing, undo/redo, identical on-screen and exported output. |
| ffmpeg | **Statically linked, in-process** via `ffmpeg-next` / `ffmpeg-sys-next` against static builds we produce in CI, per OS. No `ffmpeg` binary shelling out | Meets "embed all libraries". In-process gives frame-accurate control and zero-copy hw frames. |
| Encoders | Windows: NVENC, AMF, QSV, Media Foundation. macOS: **VideoToolbox**. Linux: **VAAPI**, NVENC, QSV. Software fallback everywhere: x264/x265/SVT-AV1 | Every GPU vendor on every OS, plus a CPU path. |
| Global hotkeys | Windows/macOS/X11: `global-hotkey` crate. Wayland: **GlobalShortcuts portal** where present, else **CLI-driven** (`ssx capture --region`) so users bind keys in their compositor config | Wayland forbids apps from grabbing keys on their own. The portal isn't implemented everywhere (§9 risks). |
| Licence | **GPL-3.0** (proposed) | Matches ShareX, permits static x264/x265. **Needs your call — §12.** |
| Config / data | TOML settings, SQLite history (with thumbnails), secrets in the OS keyring (Windows Credential Manager, macOS Keychain, Secret Service on Linux via the `keyring` crate) | Human-editable settings, queryable history, no plaintext tokens. |
| Naming / assets | Own name, icons and branding; clean-room from ShareX docs and behaviour | ShareX is GPL C#. There's no code to reuse and no reason to copy its branding. |

Caveat on "embed everything": hardware encoders depend on **system components** that can't be embedded. Those are the NVENC, AMF and QSV/oneVPL runtimes and VAAPI drivers (`libva` + vendor driver) that ship with GPU drivers, plus VideoToolbox, which is part of macOS. We embed ffmpeg and load these at runtime. The software fallback covers machines without them.

## 3. Prior art: open-source Rust tools to learn from

Check each project's licence and maintenance state at the time we adopt anything. Several of these are small projects and their status changes.

| Project | What it is | What we take from it |
|---|---|---|
| [Cap](https://github.com/CapSoftware/cap) | Rust + Tauri screen recorder. Its recording pipeline is Rust crates for capture, audio mixing, **GPU frame conversion with WGSL shaders**, muxing and encoding | Closest match to our recording architecture. Read it before designing `ssx-record`. |
| [scap](https://github.com/CapSoftware/scap) | Cross-platform capture crate: ScreenCaptureKit (macOS), Windows Graphics Capture (Windows), PipeWire (Linux). One third-party comparison found it **unmaintained** with build breakage in its PipeWire dependency chain | Reference for how to structure a three-OS capture API and the target/permission model. Probably fork or reimplement instead of depending on it. |
| [xcap](https://github.com/nashaofu/xcap) | Screen capture for Windows, macOS and Linux (X11 + Wayland); screenshots and WIP video. Same comparison notes its frame type **lacks stride, pixel format and timestamps** | Good fallback for stills and monitor/window enumeration. Our own frame type must carry stride, format, colour space and timestamp. |
| [Satty](https://github.com/Satty-org/Satty) | Screenshot annotation tool in Rust, inspired by Swappy and Flameshot; GTK/OpenGL, fullscreen annotation, post-shot crop, wlroots-oriented | UX reference for a lean annotation tool set and output actions (copy, save, pipe). GTK is a poor fit for a native Windows/macOS UI, so no code reuse. |
| [ferrishot / peashot](https://github.com/nik-rev/ferrishot) | Rust screenshot app: drag-select region, fully keyboard-driven selection, save, and upload with link + QR code | Keyboard-first region selection and the QR-after-upload flow. |
| [wayshot / libwayshot](https://github.com/waycrate/wayshot) | Rust screenshot tool and crate for wlroots compositors (`zwlr_screencopy_v1`), with an optional **EGL zero-copy** path | Direct wlroots capture backend and zero-copy import design. |
| [foamshot](https://github.com/Thirdwinter/foamshot) | Rust Wayland screenshot utility | A second, small example of Wayland overlay and region selection code. |
| [ashpd](https://github.com/bilelmoussaoui/ashpd) | Rust bindings for xdg-desktop-portal: `Screenshot`, `ScreenCast`, `GlobalShortcuts` | Our Linux portal layer. Depend on it. |
| [screencapturekit](https://crates.io/crates/screencapturekit) | Rust bindings for ScreenCaptureKit | Candidate macOS dependency. Evaluate against thin `objc2` wrappers in M0. |
| [global-hotkey](https://github.com/tauri-apps/global-hotkey) | Global hotkeys for Windows, macOS and Linux X11 | Depend on it for those three. Wayland goes through ashpd. |
| [hdrfix](https://github.com/bvibber/hdrfix), [RustDesk HDR PR](https://github.com/rustdesk/rustdesk/pull/16033) | HDR→SDR tonemapping; scRGB, SDR white level and `DuplicateOutput1` in a shipping app | Windows HDR reference (§5). Compare our output against them. |
| Flameshot, Swappy (C++/C, not Rust) | The tools Satty modelled itself on | Editor UX inspiration only. |

**Buy vs build**: build our own `ssx-capture` trait and frame type. Use `ashpd`, `pipewire`, `x11rb`, `global-hotkey`, `keyring`, `wgpu`, `egui`, `winit` off the shelf. Write the Windows WGC/HDR layer ourselves. Decide on `screencapturekit` vs thin `objc2` wrappers in M0.

## 4. Workspace layout

```
ssx/
├─ Cargo.toml                  # workspace
├─ crates/
│  ├─ ssx-core/                # settings, workflow engine, tasks, filename patterns, history (portable)
│  ├─ ssx-capture/             # Frame type + CaptureBackend trait
│  │   └─ backends: windows_wgc, windows_dda, macos_sck, linux_x11, linux_portal, linux_wlr
│  ├─ ssx-gpu/                 # wgpu context, WGSL shaders (tonemap, NV12/BGRA convert, blur, pixelate), zero-copy import per OS
│  ├─ ssx-hdr/                 # HDR detection, SDR-white-level query (Windows), CPU reference tonemap
│  ├─ ssx-record/              # frame pump, audio backends (WASAPI / CoreAudio+SCK / PipeWire), ffmpeg mux/encode, GIF
│  ├─ ssx-editor/              # scene graph, tools, effects, undo/redo, export
│  ├─ ssx-upload/              # uploader trait, custom-uploader (.sxcu) engine, built-in providers, OAuth
│  ├─ ssx-hotkeys/             # global-hotkey + Wayland portal + CLI fallback
│  ├─ ssx-shell/               # per-OS file-manager integration (see §8); Windows COM DLL lives here
│  ├─ ssx-ipc/                 # single-instance + IPC (named pipe on Windows, unix socket elsewhere)
│  ├─ ssx-app/                 # tray app, overlay, settings/history UI
│  └─ ssx-cli/                 # `ssx capture|record|upload|edit`
├─ third_party/ffmpeg/         # per-OS build scripts + pinned versions (no vendored binaries)
├─ packaging/                  # WiX/MSIX (Win), .app/.dmg (mac), AppImage/deb/rpm/Flatpak (Linux)
├─ xtask/                      # build ffmpeg, package, sign, run HDR test matrix
└─ .github/workflows/          # windows-latest, macos-latest, ubuntu-latest (X11 via Xvfb; Wayland via a headless compositor)
```

## 5. HDR → SDR screenshots

### 5.1 Per-platform stance
| OS | Approach |
|---|---|
| **Windows** | **We do the tonemap** (§5.2). Windows gives us linear float scRGB and no SDR conversion of its own for this path. This is the hard requirement. |
| **macOS** | **Ask the OS.** ScreenCaptureKit's `SCStreamConfiguration.captureDynamicRange` defaults to SDR, so the default capture is already SDR. HDR outputs (`HDRLocalDisplay` / `HDRCanonicalDisplay`) are opt-in, macOS 15+, per WWDC24 [Capture HDR content with ScreenCaptureKit](https://developer.apple.com/videos/play/wwdc2024/10088/). Our shader is only needed for an optional "keep HDR original" export. Verify current HDR recording limits on the target macOS. |
| **Linux** | Not in v1. Compositor HDR support and its PipeWire capture path are still maturing. Capture as SDR, and revisit the shared shader once the portal/PipeWire side exposes float or PQ frames. |

### 5.2 Windows pipeline
When Windows HDR is on, the compositor works in linear **scRGB** (`R16G16B16A16_FLOAT`, Rec.709 primaries, **1.0 = 80 nits**, values >1 are highlights, negatives are wide-gamut). SDR content sits at the user's **"SDR content brightness"** level (typically ~200 nits ≈ scRGB 2.5), not at 1.0. Legacy capture paths return `B8G8R8A8` by clipping, so anything above roughly 40% linear lands on white and the image looks wrong. The RustDesk PR describes the same failure and fix.

1. **Detect** per monitor: `DisplayConfigGetDeviceInfo` with `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` (HDR active?) and `DISPLAYCONFIG_SDR_WHITE_LEVEL` (nits = raw/1000 × 80).
2. **Capture FP16**: WGC with `DirectXPixelFormat::R16G16B16A16Float` on HDR monitors, else `B8G8R8A8`. DDA fallback via `DuplicateOutput1` with format list `[R16G16B16A16_FLOAT, B8G8R8A8_UNORM]`.
3. **Tonemap on GPU** (the shared WGSL shader in `ssx-gpu`):
   1. Scale so `SDR white → 1.0` (`x / (sdr_white_nits / 80)`).
   2. Gamut-map Rec.709/scRGB negatives via a Rec.2020 intermediate. No hard channel clipping.
   3. Highlight roll-off on **luminance / max-channel** to preserve hue. Selectable operator: `Clip` (exact SDR look, blows highlights), `Reinhard-extended` (default candidate), `BT.2390 EETF`, `ACES-fit`. Knee and peak are user-tunable.
   4. Encode with the **piecewise sRGB OETF**, not a 2.2 gamma.
   5. **Dither** (blue-noise or ordered) before 8-bit quantisation to avoid banding.
4. **Mixed setups**: monitors are converted independently (HDR one tonemapped, SDR one passed through), then composited for multi-monitor region capture.
5. **Optional "also keep HDR original"**: 16-bit float `.jxr` or AVIF/PNG-16 next to the SDR file (off by default).
6. **Recording**: the same shader runs per frame before NV12 conversion, so HDR desktops record as SDR. HDR10 HEVC recording is a post-v1 stretch goal.
7. **Region overlay** shows the *tonemapped* frozen frame so the selection matches the saved result.

### 5.3 Verification
- A **CPU reference tonemap** in `ssx-hdr`, unit-tested against golden values (SDR white → 1.0, 0 → 0, 4× white → rolled off, negative channels).
- The WGSL shader is checked against the CPU reference in CI on software adapters (WARP on Windows, llvmpipe/lavapipe on Linux) using synthetic scRGB gradients. Because it's one WGSL source, this also guards macOS/Linux shader use.
- Manual matrix on real hardware: HDR on/off, SDR-white slider at min/mid/max, mixed HDR+SDR monitors, 10-bit panels, Auto HDR games, HDR video playback window.
- Compare output against [hdrfix](https://github.com/bvibber/hdrfix) and [HDR_Screenshot_tool_for_windows](https://github.com/MagestiUA/HDR_Screenshot_tool_for_windows).

## 6. ShareX-parity feature map

### 6.1 Editor toolbar (from your screenshot; verify each tool against ShareX when building)
| Group | Tools |
|---|---|
| Region / crop | Rectangle, ellipse, freeform region select |
| Select | Select & move / resize / rotate |
| Shapes | Rectangle, ellipse, freehand, line, arrow (incl. freehand arrow) |
| Text | Text, text with outline/background, speech balloon, step number (auto-incrementing) |
| Callouts | Magnify, spotlight |
| Insert | Image from file/clipboard/screen, emoji/sticker, cursor stamp |
| Obscure | Blur, pixelate, eraser, highlight marker, grid/hatch fill |
| Global | Crop, cut-out, canvas/background, image effects, flip/rotate/resize; per-object colour, border, shadow, font; undo/redo; zoom/pan; copy/save/upload/pin |
| Toolbar | Dark, dockable, dropdowns for tool options — same layout as the screenshot |

### 6.2 Capture and recording
- **Capture**: region (rect / ellipse / freeform / last region), fullscreen, active monitor, active window, window picker, scrolling capture, cursor toggle, delay, screen color picker, ruler. OCR (Windows.Media.Ocr / Vision on macOS / Tesseract on Linux) is a later phase.
- **Recording**: region/window/monitor, cursor + click highlight, system audio + mic, FPS and quality presets, start/stop/pause hotkeys, MP4 and GIF, encoder auto-pick per OS (§2).

### 6.3 Platform support matrix
| Feature | Windows | macOS | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| Fullscreen / monitor capture | WGC / DDA | ScreenCaptureKit | XCB + SHM | Portal, or direct on wlroots |
| Region overlay | Topmost window | Borderless window | Override-redirect window | layer-shell (wlroots/KDE), fullscreen toplevel with frozen frame elsewhere (GNOME) |
| Window capture | Yes (WGC) | Yes | Yes (XComposite) | Portal picker, or `ext-image-capture-source` toplevels where the compositor supports them |
| Recording | WGC + WASAPI | SCK + audio | XCB/SHM frames + PulseAudio/PipeWire | ScreenCast portal + PipeWire |
| Global hotkeys | Yes | Yes | Yes | GlobalShortcuts portal where present, else CLI + compositor binding |
| Tray | Yes | Menu bar | StatusNotifierItem | StatusNotifierItem |
| File-manager menu | Explorer (§8) | Finder Quick Actions | Nautilus/Dolphin/Thunar/Nemo | Same as X11 |
| HDR→SDR | ssx tonemap | OS-provided SDR | n/a in v1 | n/a in v1 |

## 7. Post screenshot / post file / post video (workflow engine)

ShareX's model is a pipeline. We replicate it with three typed entry points sharing one engine in `ssx-core`.

```
        ┌─ Post screenshot ─┐
Input ──┼─ Post file ───────┼─▶ After-capture tasks ─▶ Upload (destination) ─▶ After-upload tasks
        └─ Post video ──────┘   (edit, copy, save,      (image / text /          (copy URL, open URL,
                                 pin, OCR, upload)        file / video)            shorten, QR, notify)
```

- **Post screenshot**: capture → optional editor → copy/save → upload via *image uploader*.
- **Post file**: tray menu, CLI, drag-drop, clipboard, URL, folder (zip), or file-manager right-click → *file uploader*. Images picked up this way can optionally route through the editor first.
- **Post video**: recording stops → optional trim/convert → *video destination*. ShareX sends video through its file uploader. We add a separate **video destination override**.
- **Destinations** (ShareX-style): image uploader, text uploader, file uploader, video uploader, URL shortener, URL sharing service.
- **Tasks are per-workflow and hotkey-bindable** ("Capture region → edit → upload → copy URL" as one hotkey, or one CLI call).
- **Filename patterns** (`%y-%mo-%d_%h-%mi-%s`, `%t` window title, random tokens), per-type folders, format/quality (PNG, JPG, WebP, AVIF), auto-downscale for size limits.
- **History**: SQLite + thumbnails, re-upload, copy URL, delete-URL where the provider supports it.

## 8. File-manager right-click integration

Common design: every OS integration is a **thin shim that forwards the selected paths + an action id** (`upload`, `edit`, `upload-video`) to the running app over `ssx-ipc`, launching it first if needed. A batch of selected files reaches the app in one call. No logic lives in the shim.

**Windows** — two layers, since Windows 11 hides classic verbs under "Show more options":
1. *Classic verbs (M3, no signing)*: `HKCU\Software\Classes\*\shell\ssx.*` and `SystemFileAssociations\image\shell`. Entries: **Upload with ssx**, **Edit image with ssx** (images), **Upload as video with ssx** (video extensions), optional **Pin to screen**.
2. *Windows 11 top-level menu (M7)*: `ssx-shell` is a COM DLL implementing [`IExplorerCommand`](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/Shell/struct.IExplorerCommand.html) (one call with the full `IShellItemArray`), registered through a **sparse MSIX package** so the unpackaged app gets package identity ([Microsoft guidance](https://blogs.windows.com/windowsdeveloper/2021/07/19/extending-the-context-menu-and-share-dialog-in-windows-11/)). Needs a code-signed package. Reference: [windows11-context-rs](https://github.com/SalahaldinBilal/windows11-context-rs).
3. The DLL stays tiny so a bug can't take Explorer down.

**macOS**: **Quick Actions** (Finder Services workflows in `~/Library/Services`, shown in the right-click "Quick Actions" submenu) first, since they need no extension bundle. A **Finder Sync** or **Share extension** (an app-extension bundle inside the signed `.app`) comes later for a top-level menu item.

**Linux**: install per-file-manager entries at install time or first run:
- **Nautilus (GNOME Files)**: scripts in `~/.local/share/nautilus/scripts`, or a Python/`nautilus-python` extension for a proper menu item.
- **Dolphin (KDE)**: a `.desktop` service menu in `~/.local/share/kio/servicemenus`.
- **Thunar**: custom actions (`uca.xml`). **Nemo**: `.nemo_action` files.
- **Fallback for any desktop**: a `.desktop` file with the right `MimeType` so "Open With → ssx" works.
- **Flatpak** builds are sandboxed and can't install file-manager hooks the same way. The Flatpak would rely on "Open With" and portals, so file-manager menus are a native package (deb/rpm/AppImage) feature.

Open item: confirm ShareX's exact right-click entry set against its source before locking the menu list.

## 9. ffmpeg embedding plan (per OS)

1. `xtask ffmpeg` builds pinned FFmpeg (pin the version at M0) as **static libs** with the encoders for the target:
   - **Windows**: `d3d11va`, `dxva2`, NVENC (nv-codec-headers), AMF, `libvpl` (QSV).
   - **macOS**: VideoToolbox (system framework).
   - **Linux**: VAAPI (`libva` loaded at runtime), NVENC, QSV.
   - **All**: `libx264`/`libx265` (GPL), `libsvtav1`, native AAC. GIF uses the `gifski` crate for quality.
2. **Windows is the riskiest build.** Static linking of `ffmpeg-sys-next` on Windows is known to be fiddly ([rust-ffmpeg#233](https://github.com/zmwangx/rust-ffmpeg/issues/233)). **M0 must prove a static build links and encodes on `windows-latest`** before recording work starts. Linux and macOS are usually simpler. On Linux, keep `libva` and the NVENC runtime dynamic.
3. Cache built libs as CI artifacts keyed on version + config hash. Developers pull them with `xtask`.
4. Ship `THIRD_PARTY_LICENSES` generated by `cargo-about`, plus ffmpeg's configure line (GPL/LGPL compliance).
5. Binary size budget: ≤ 40 MB installed. Strip unused codecs and demuxers.

## 10. Milestones

Developer note: the core, editor, uploaders and workflow engine build and test on any OS, **including the Linux dev container this repo is developed in**. Windows capture, HDR and shell work need `windows-latest` CI plus a real Windows machine (ideally with an HDR display). macOS work needs a Mac or `macos-latest`. Local cross-checks: `cargo check --target x86_64-pc-windows-msvc` with `cargo-xwin`.

| M | Deliverable | Exit criteria |
|---|---|---|
| **M0 – Spikes (2–3 wks)** | Workspace + 3-OS CI. **Static ffmpeg links and encodes an NV12 clip** on all three runners. **wgpu zero-copy import** feasibility (D3D11→D3D12, IOSurface→Metal, dma-buf→Vulkan) vs fallback. **Windows WGC FP16** HDR capture. **Linux**: portal Screenshot + ScreenCast on GNOME and KDE, wlroots direct capture, X11 SHM. **macOS**: ScreenCaptureKit stills + stream (`screencapturekit` crate vs `objc2`). Editor renderer choice (vello vs tiny-skia; egui vs Slint) | Written go/no-go per spike in `docs/spikes/`. |
| **M1 – Windows capture + HDR** | `ssx-capture` (Windows), `ssx-gpu`, `ssx-hdr`, tray app, hotkeys, region overlay, fullscreen/window capture, save PNG/JPG, clipboard | HDR and SDR screenshots match what's on screen; CPU/GPU tonemap parity tests pass; correct on mixed-DPI multi-monitor. |
| **M2 – Editor** | Scene graph, all §6.1 tools, undo/redo, export, toolbar matching the screenshot | Every tool works; exported PNG equals the on-screen render; 4K edits stay at 60 fps. Runs on all three OSes. |
| **M3 – Upload + workflows** | Workflow engine, `.sxcu` engine, Imgur/S3/SFTP, after-capture/after-upload tasks, history, `ssx-cli`, Windows classic Explorer verbs | Hotkey → capture → edit → upload → URL in clipboard; 10 real `.sxcu` files import and work; multi-select right-click upload works. |
| **M4 – Windows recording** | WGC → tonemap → NV12 → NVENC/AMF/QSV/MF/x264, WASAPI audio, MP4 + GIF, post-video workflow | 1080p60 / 4K30 with <5% dropped frames and low CPU on NVIDIA, AMD, Intel; A/V sync ±40 ms over 30 min; HDR desktops record with correct tone. |
| **M5 – Linux** | X11 backend, Wayland portal backend, wlroots direct backend, region overlay (layer-shell + fallback), hotkeys (X11 / portal / CLI), tray, file-manager entries, PipeWire recording with VAAPI/NVENC | Works on GNOME (Wayland), KDE (Wayland + X11), and sway or Hyprland. Screenshot → edit → upload works from a compositor keybinding calling the CLI. |
| **M6 – macOS** | ScreenCaptureKit screenshots + recording with VideoToolbox, permission flow, menu-bar item, hotkeys, Quick Actions | Signed and notarised `.dmg`; screenshot + recording work on Apple Silicon and Intel; SDR output correct on an HDR-capable display. |
| **M7 – Polish + packaging** | Scrolling capture, color picker, ruler, pin-to-screen, OCR, settings UI, installers (MSIX/WiX, dmg, AppImage/deb/rpm/Flatpak), auto-update, **Win11 `IExplorerCommand` menu + signing** | Clean-machine install → first capture in <60 s on each OS. |
| **M8 – Later** | HDR10 HEVC recording, ARM64 Windows, Linux HDR capture, plugin API | — |

## 11. Risks

- **HDR look is subjective.** Mitigation: selectable operators, a "clip to SDR white" mode, side-by-side test images.
- **Static ffmpeg on Windows** is the biggest build risk (M0 gate).
- **wgpu zero-copy import** may not work cleanly on every backend. Fallback is a CPU-upload path, plus a native D3D11 shader on Windows for recording.
- **Wayland fragmentation.** No universal API for window lists, overlays or hotkeys. GNOME doesn't support `wlr-layer-shell`. The portal may show a picker or permission prompt on each capture unless a restore token is kept. `xdg-desktop-portal-gtk` desktops (XFCE, MATE, Cinnamon, LXQt) have no GlobalShortcuts portal, so users there need CLI + compositor bindings. Global-shortcut key **release** events reportedly don't arrive, which affects push-to-hold features.
- **Wayland scaling and cursor**: fractional scaling and cursor overlay differ per compositor, so a test matrix is required.
- **macOS**: Screen Recording (TCC) permission flow, and signing + notarisation need an Apple Developer account. Verify how often current macOS re-prompts for screen-recording permission and whether the `SCContentSharingPicker`/`SCScreenshotManager` paths avoid it. HDR capture is Apple-Silicon-only.
- **WGC on Windows**: the yellow border and cursor handling. Borderless capture (`IsBorderRequired = false`) needs Windows 11 and may need an access request. Verify in the spike.
- **Signing costs**: Windows (Win11 menu, installer) and macOS (notarisation) both need paid certificates.
- **Testing limits**: I can't exercise Windows or macOS APIs from this Linux container. Development there needs CI runners and real machines.
- **DRM and anti-cheat windows** return black frames under WGC and ScreenCaptureKit. Expected. Document it.
- **Third-party crate churn**: several relevant crates are small or have gone stale (§3). Pin versions and keep our trait boundary so backends can be swapped.

## 12. Open questions for you

1. **Licence**: GPL-3.0 (simplest, allows x264/x265) or permissive/LGPL (fewer software-encoding options)?
2. **Order of platforms**: Windows first (as planned), then Linux, then macOS. Do you want macOS before Linux?
3. **Linux targets**: which desktops do you actually use? That decides whether GNOME, KDE or wlroots gets tested first.
4. **macOS minimum version**: the plan assumes a recent macOS for ScreenCaptureKit. Do you need to support older releases?
5. **Name/branding**: keep `ssx`?
6. **UI toolkit**: fine with egui as the default, pending the M0 spike?
7. **Code signing**: do you have or will you get Windows and Apple certificates? They gate the Win11 menu, a smooth installer and macOS distribution.
8. **Which uploaders matter most** beyond Imgur/S3/SFTP?
9. **HDR default**: "Reinhard-extended" (looks good, alters highlights) or "Clip to SDR white" (matches SDR apps exactly)?

## 13. Sources
- HDR: [RustDesk HDR tonemap PR](https://github.com/rustdesk/rustdesk/pull/16033), [hdrfix](https://github.com/bvibber/hdrfix), [HDR_Screenshot_tool_for_windows](https://github.com/MagestiUA/HDR_Screenshot_tool_for_windows), [WWDC24: Capture HDR content with ScreenCaptureKit](https://developer.apple.com/videos/play/wwdc2024/10088/), [screencapturekit crate](https://crates.io/crates/screencapturekit)
- Rust capture/screenshot tools: [Cap](https://github.com/CapSoftware/cap), [scap](https://github.com/CapSoftware/scap), [xcap](https://github.com/nashaofu/xcap), [Satty](https://github.com/Satty-org/Satty), [ferrishot](https://github.com/nik-rev/ferrishot), [wayshot](https://github.com/waycrate/wayshot), [foamshot](https://github.com/Thirdwinter/foamshot)
- Wayland: [ext-image-copy-capture merge (Phoronix)](https://www.phoronix.com/news/Wayland-Merges-Screen-Capture), [wlr-screencopy protocol](https://wayland.app/protocols/wlr-screencopy-unstable-v1), [xdg-desktop-portal-generic](https://github.com/avranju/xdg-desktop-portal-generic), [ashpd GlobalShortcuts](https://bilelmoussaoui.github.io/ashpd/ashpd/desktop/global_shortcuts/index.html), [Wayland global shortcuts learnings](https://github.com/aaddrick/claude-desktop-debian/blob/main/docs/learnings/wayland-global-shortcuts-portal.md), [global-hotkey](https://docs.rs/global-hotkey)
- ShareX: [ShareX](https://github.com/sharex/sharex), [custom uploader docs](https://getsharex.com/docs/custom-uploader), [CustomUploaders](https://github.com/ShareX/CustomUploaders)
- ffmpeg in Rust: [static linking discussion](https://users.rust-lang.org/t/static-linking-ffmpeg-bindings/105083), [rust-ffmpeg#233](https://github.com/zmwangx/rust-ffmpeg/issues/233), [rust-ffmpeg-cli (in-process NVENC)](https://github.com/PlanetLOF/rust-ffmpeg-cli)
- Windows shell: [Extending the Context Menu in Windows 11](https://blogs.windows.com/windowsdeveloper/2021/07/19/extending-the-context-menu-and-share-dialog-in-windows-11/), [IExplorerCommand (windows-rs)](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/Shell/struct.IExplorerCommand.html), [windows11-context-rs](https://github.com/SalahaldinBilal/windows11-context-rs)
