# ssx — a ShareX alternative in Rust

Status: **plan / not yet implemented** · Target: Windows 10 2004+ / Windows 11 (x64, later ARM64) · Repo was empty when this was written.

## 1. Goals

1. **Capture**: region, fullscreen, window, monitor, scrolling, with a ShareX-style frozen-frame region overlay.
2. **Image editor**: parity with the ShareX editor toolbar (see §5), rendered on the GPU.
3. **Screen recording**: MP4 (H.264/HEVC/AV1) and GIF, with system audio and mic. GPU encode, zero-copy where possible.
4. **HDR → SDR screenshots on Windows** that look like what the user saw, not the washed-out or blown-out output a naive capture gives.
5. **Embedded libraries**: ffmpeg and everything else is linked into the binary or shipped beside it. There is no "install ffmpeg first" step.
6. **Explorer right-click integration** and ShareX's post-capture workflow: *post screenshot*, *post file*, *post video*, with destinations, after-capture tasks and after-upload tasks.

Non-goals for v1: Linux/macOS UI, a plugin system, a ShareX config importer beyond `.sxcu` custom uploaders.

## 2. Key decisions (and why)

| Area | Decision | Reason |
|---|---|---|
| Platform | Windows-first; core crates stay portable | HDR, WGC and shell extensions are Windows APIs. A portable core keeps Linux/macOS open. |
| Windows APIs | `windows` crate (windows-rs), own thin wrappers | First-party bindings cover WGC, D3D11, DXGI, WASAPI, `IExplorerCommand`. Third-party capture crates (`windows-capture`, `scap`, `xcap`) can serve as reference but don't expose the HDR/float path or D3D11 frame access we need. Check licences before borrowing code. |
| Capture | **Windows Graphics Capture (WGC)** primary; DXGI Desktop Duplication (`IDXGIOutput5::DuplicateOutput1`) fallback; GDI `BitBlt` last resort | WGC handles windows, monitors and HDR formats, and gives D3D11 textures. DDA is a good fallback for full monitors. |
| GPU pipeline | **D3D11 end to end for capture → tonemap → convert → encode**. `wgpu` only for the editor canvas and effects | Sharing one D3D11 device lets a WGC texture go through a tonemap shader and NV12 conversion straight into an ffmpeg `d3d11va` hw-frames context with no CPU readback. Importing D3D11 textures into wgpu's D3D12 backend is possible but adds risk for no v1 benefit. |
| Screenshot path | GPU tonemap → readback once to RGBA8 → editor/save/upload | Screenshots are small and infrequent, so a single readback is fine. |
| UI | `winit` + `wgpu` + `egui` for editor/settings/history; tray via `tray-icon`; region overlay is a raw winit+wgpu topmost window per monitor | egui is the fastest route to a dense tool-based UI. The overlay needs low latency and per-monitor DPI control, so it stays hand-rolled. **Spike in M0**: compare egui against Slint for the editor. |
| Annotation rendering | Scene graph of vector objects, drawn with `vello` (GPU) or `tiny-skia` (CPU) — **decide in M0 spike**. Blur, pixelate, magnify and spotlight are wgpu shaders. Export renders the same scene offscreen | One scene graph gives non-destructive editing, undo/redo and identical on-screen and exported output. |
| ffmpeg | **Statically linked, in-process** via `ffmpeg-next` / `ffmpeg-sys-next` against a prebuilt static build we produce in CI (vcpkg triplet `x64-windows-static-md` or a custom MSVC/MSYS2 build). No `ffmpeg.exe` shelling out | Meets "embed all libraries". In-process gives frame-accurate control, zero-copy hw frames and no console flashes. |
| Encoders | HW: NVENC, AMF, QSV (via ffmpeg). Fallback: Media Foundation H.264 (via windows-rs) and software x264/x265/SVT-AV1 | Covers every GPU vendor and machines with no usable GPU encoder. |
| Licence | **GPL-3.0** (proposed) | Matches ShareX and permits static x264/x265. If you'd rather ship LGPL-only, we drop x264/x265, keep HW encoders plus openh264/SVT-AV1, and must ship relinkable objects. **Needs your call — see §11.** |
| Config / data | TOML settings, SQLite history (with thumbnails), secrets in Windows Credential Manager (DPAPI) | Human-editable settings, queryable history, no plaintext tokens. |
| Naming / assets | Own name, icons and branding; clean-room from ShareX docs and behaviour | ShareX is GPL C#. There's no code to reuse, and no reason to copy its branding. |

Honest caveat on "embed everything": NVENC (`nvEncodeAPI64.dll`), AMF (`amfrt64.dll`) and QSV/oneVPL runtimes ship with **GPU drivers** and are loaded dynamically at runtime. We embed ffmpeg and the headers, but not the driver-side encoder. The software/Media Foundation fallback covers machines without them.

## 3. Workspace layout

```
ssx/
├─ Cargo.toml                  # workspace
├─ crates/
│  ├─ ssx-core/                # settings, workflow engine, tasks, filename patterns, history (portable)
│  ├─ ssx-capture/             # WGC / DDA / GDI backends, monitor + window enumeration, cursor
│  ├─ ssx-hdr/                 # HDR detection, SDR-white-level query, tonemap shaders (HLSL + CPU reference)
│  ├─ ssx-record/              # frame pump, WASAPI audio, ffmpeg mux/encode, GIF
│  ├─ ssx-editor/              # scene graph, tools, effects, undo/redo, export
│  ├─ ssx-upload/              # uploader trait, custom-uploader (.sxcu) engine, built-in providers, OAuth
│  ├─ ssx-shell/               # cdylib COM DLL: IExplorerCommand (Win11) + classic verb registration
│  ├─ ssx-ipc/                 # single-instance + named-pipe protocol (app ⇄ shell ext ⇄ CLI)
│  ├─ ssx-app/                 # tray app, hotkeys, overlay, settings/history UI
│  └─ ssx-cli/                 # `ssx capture|record|upload|edit`
├─ third_party/ffmpeg/         # build scripts + pinned versions (not vendored binaries)
├─ installer/                  # WiX/MSIX, sparse package manifest, signing config
├─ xtask/                      # build ffmpeg, package, sign, run HDR test matrix
└─ .github/workflows/          # windows-latest CI
```

## 4. HDR → SDR screenshots (the hard requirement)

### 4.1 Background
When Windows HDR is on, the compositor works in linear **scRGB** (`R16G16B16A16_FLOAT`, Rec.709 primaries, **1.0 = 80 nits**, values >1 are highlights, negatives are wide-gamut). SDR content sits at the user's **"SDR content brightness"** level (typically ~200 nits ≈ scRGB 2.5), not at 1.0. Legacy capture paths return `B8G8R8A8` by clipping, so anything above roughly 40% linear lands on white and the image looks wrong. RustDesk's [HDR tonemap PR](https://github.com/rustdesk/rustdesk/pull/16033) describes the same failure and the same fix.

### 4.2 Pipeline
1. **Detect** per monitor: `DisplayConfigGetDeviceInfo` with `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` (is HDR/advanced colour active) and `DISPLAYCONFIG_SDR_WHITE_LEVEL` (nits = raw/1000 × 80).
2. **Capture FP16**: WGC with `DirectXPixelFormat::R16G16B16A16Float` on HDR monitors, else `B8G8R8A8`. DDA fallback via `DuplicateOutput1` with the format list `[R16G16B16A16_FLOAT, B8G8R8A8_UNORM]`.
3. **Tonemap on GPU** (one D3D11 pixel/compute shader, also reused for recording):
   1. Scale so `SDR white → 1.0` (`x / (sdr_white_nits / 80)`).
   2. Gamut-map Rec.709/scRGB negatives: soft-clip via Rec.2020 intermediate, no hard channel clipping.
   3. Highlight roll-off on **luminance / max-channel** to preserve hue. Selectable operator: `Clip` (exact SDR look, blows highlights), `Reinhard-extended` (default), `BT.2390 EETF`, `ACES-fit`. Knee and peak are user-tunable.
   4. Encode with the **piecewise sRGB OETF** (not a 2.2 gamma).
   5. **Dither** (blue-noise / ordered) before quantising to 8-bit to avoid banding on gradients.
4. **Mixed setups**: monitors are converted independently (HDR one tonemapped, SDR one passed through), then composited for multi-monitor region capture.
5. **Optional "also keep HDR original"**: write a 16-bit float `.jxr` or AVIF/PNG-16 next to the SDR file (off by default).
6. **Recording**: same shader runs per frame before NV12 conversion, so HDR desktops record as SDR. HDR10 HEVC recording is a post-v1 stretch goal.
7. **Region overlay**: shows the *tonemapped* frozen frame so the selection matches the saved result.

### 4.3 Verification strategy
- Keep a **CPU reference implementation** of the tonemap in `ssx-hdr` and unit-test it against golden values (SDR white → 1.0, 0 → 0, 4× white → rolled-off, negative channels).
- Test the shader vs the CPU reference in CI on the **WARP** software adapter with synthetic scRGB gradients (ramp, colour bars, near-clipping patch).
- Manual test matrix on real hardware: HDR on/off, SDR-white slider at min/mid/max, mixed HDR+SDR monitors, 10-bit panels, Auto HDR games, HDR video playback window.
- Prior art to compare our output against: [hdrfix](https://github.com/bvibber/hdrfix) (Rust, HDR→SDR tonemap) and [MagestiUA/HDR_Screenshot_tool_for_windows](https://github.com/MagestiUA/HDR_Screenshot_tool_for_windows).

## 5. ShareX-parity feature map

### 5.1 Editor toolbar (from your screenshot; verify each tool against ShareX when building)
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

### 5.2 Capture
Region (rect / ellipse / freeform / last region), fullscreen, active monitor, active window, window picker, scrolling capture, cursor inclusion toggle, delay, screen color picker, ruler. OCR (Windows.Media.Ocr) is a later phase.

### 5.3 Screen recording
Region/window/monitor, cursor + click highlight, system audio (WASAPI loopback) + mic, FPS and quality presets, hotkey start/stop/pause, MP4 and GIF, encoder auto-pick (NVENC → AMF → QSV → Media Foundation → x264).

## 6. Post screenshot / post file / post video (workflow engine)

ShareX's model is a pipeline; we replicate it with three typed entry points that share one engine in `ssx-core`.

```
        ┌─ Post screenshot ─┐
Input ──┼─ Post file ───────┼─▶ After-capture tasks ─▶ Upload (destination) ─▶ After-upload tasks
        └─ Post video ──────┘   (edit, copy, save,      (image / text /          (copy URL, open URL,
                                 pin, OCR, upload)        file / video)            shorten, QR, notify)
```

- **Post screenshot**: capture → optional editor → copy/save → upload via *image uploader*.
- **Post file**: from tray menu, CLI, drag-drop, clipboard, URL, folder (zip), or Explorer right-click → *file uploader*. Images picked up this way can optionally route through the editor first.
- **Post video**: recording stops → optional trim/convert → *video destination*. ShareX sends video through its file uploader. We'll add a separate **video destination override** so videos can go somewhere else than other files.
- **Destinations** (ShareX-style): image uploader, text uploader, file uploader, video uploader, URL shortener, URL sharing service.
- **Tasks are per-workflow and hotkey-bindable** ("Capture region → edit → upload → copy URL" as one hotkey).
- **Filename patterns** (`%y-%mo-%d_%h-%mi-%s`, `%t` window title, random tokens), per-type folders, format/quality (PNG, JPG, WebP, AVIF), auto-downscale for size limits.
- **History**: SQLite + thumbnails, re-upload, copy URL, delete-URL where the provider supports it.

## 7. Uploaders

1. **Custom uploader engine compatible with ShareX `.sxcu`** ([spec](https://getsharex.com/docs/custom-uploader)): request method/URL/headers/args/body types, `FileFormName`, and response parsing syntax (`{json:…}`, `{xml:…}`, `{regex:…}`, `{header:…}`, `{response}`), plus URL/thumbnail/deletion URL and error message templates. Importing existing `.sxcu` files (double-click, drag-drop) instantly covers hundreds of hosts. Reference collection: [ShareX/CustomUploaders](https://github.com/ShareX/CustomUploaders).
2. **Built-ins (v1)**: Imgur, S3-compatible (S3/R2/MinIO/B2), SFTP/FTP, Dropbox, Google Drive, OneDrive, generic HTTP POST. Built-ins use OAuth (loopback PKCE) where the provider supports it.
3. Uploads run on `tokio` with progress, cancel, retry with backoff, and a queue window. Secrets go to Credential Manager.

## 8. Explorer right-click integration

Two layers, since Windows 11 hides classic verbs under "Show more options":

1. **Classic verbs (M3, no signing needed)**: register under `HKCU\Software\Classes\*\shell\ssx.*` and `SystemFileAssociations\image\shell`. Entries: **Upload with ssx**, **Edit image with ssx** (images only), **Upload as video with ssx** (video extensions), **Pin to screen** (optional). Multi-select is handled by an `ssx-ipc` client that batches files into one running instance, avoiding N processes.
2. **Windows 11 top-level menu (M6)**: `ssx-shell` is a COM DLL implementing [`IExplorerCommand`](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/Shell/struct.IExplorerCommand.html) (single call with the full `IShellItemArray`), registered through a **sparse MSIX package** so an unpackaged app gets package identity ([Microsoft guidance](https://blogs.windows.com/windowsdeveloper/2021/07/19/extending-the-context-menu-and-share-dialog-in-windows-11/)). Requires a code-signed package. Reference: [windows11-context-rs](https://github.com/SalahaldinBilal/windows11-context-rs).
3. Installer registers/unregisters both. Portable mode skips shell integration and shows a "register" button in settings.
4. The DLL stays tiny and does no I/O beyond sending the file list over the named pipe. If the app isn't running, it launches it. This keeps a crash in the app from taking Explorer down.

Open item: confirm ShareX's exact right-click entry set (upload, edit image, and any others) against its source before locking the menu list.

## 9. ffmpeg embedding plan

1. `xtask ffmpeg` builds pinned FFmpeg (currently 7.x/8.x — pin at M0) as **static libs** with: `d3d11va`, `dxva2`, `nvenc` (nv-codec-headers), `amf`, `libvpl` (QSV), `libx264`/`libx265` (if GPL), `libsvtav1`, AAC (native encoder or fdk excluded for licensing), `libgifski`-free (we use `gifski` crate for GIF quality).
2. Build environment: MSVC toolchain + MSYS2 for configure, or vcpkg overlay ports. Windows static linking of `ffmpeg-sys-next` is known to be fiddly ([rust-ffmpeg#233](https://github.com/zmwangx/rust-ffmpeg/issues/233)), so **M0 must prove a static build links and encodes on `windows-latest` before any other recording work.**
3. Cache the built libs as a CI artifact keyed on version + config hash; local devs download it via `xtask`.
4. Ship a `THIRD_PARTY_LICENSES` file generated by `cargo-about` plus ffmpeg's configure line (needed for GPL/LGPL compliance).
5. Binary size budget: ≤ 40 MB installed. Strip unused codecs/demuxers aggressively.

## 10. Milestones

| M | Deliverable | Exit criteria |
|---|---|---|
| **M0 – Spikes (1–2 wks)** | Workspace + CI. Static ffmpeg links and encodes an NV12 test clip on `windows-latest`. WGC FP16 capture of an HDR monitor. Editor renderer choice (vello vs tiny-skia; egui vs Slint). | Each spike has a written go/no-go in `docs/spikes/`. |
| **M1 – Capture + HDR** | `ssx-capture`, `ssx-hdr`, tray app, hotkeys, region overlay, fullscreen/window capture, save PNG/JPG, clipboard | HDR and SDR screenshots visually match what's on screen; tonemap CPU/GPU parity tests pass; runs correctly on mixed-DPI multi-monitor. |
| **M2 – Editor** | Scene graph, all §5.1 tools, undo/redo, export, editor toolbar matching the screenshot | Every tool works; exported PNG equals on-screen render; 4K image edits stay at 60 fps. |
| **M3 – Upload + workflows** | Workflow engine, `.sxcu` engine, Imgur/S3/SFTP, after-capture/after-upload tasks, history, `ssx-cli`, classic Explorer verbs | Hotkey → capture → edit → upload → URL in clipboard; import of 10 real `.sxcu` files works; right-click upload on multi-select works. |
| **M4 – Recording** | WGC → tonemap → NV12 → NVENC/AMF/QSV/MF/x264, WASAPI audio, MP4 + GIF, post-video workflow | 1080p60 / 4K30 recording with < 5% dropped frames and low CPU on NVIDIA, AMD and Intel; audio in sync (±40 ms) over a 30 min recording; HDR desktops record with correct tone. |
| **M5 – Polish** | Scrolling capture, color picker, ruler, pin-to-screen, OCR, settings UI, installer, auto-update | Clean-VM install → first capture in under 60 s. |
| **M6 – Win11 menu** | `ssx-shell` `IExplorerCommand` + sparse package + signing | Entries appear in the top-level Win11 menu on a clean machine; multi-select works. |
| **M7 – Later** | HDR10 HEVC recording, ARM64, Linux/macOS backends, plugin API | — |

## 11. Risks and open questions

**Risks**
- **HDR look is subjective.** Mitigation: selectable operators, a "clip to SDR white" mode, and side-by-side test images.
- **Static ffmpeg on Windows** is the biggest build risk (M0 gate).
- **WGC yellow border and cursor handling**: borderless capture (`IsBorderRequired = false`) needs Windows 11 and may need an access request; on Windows 10 use DDA for monitors. Verify in the M0 spike.
- **Signing**: the Win11 menu and installer need a certificate. Azure Trusted Signing or a standard code-signing cert costs money.
- **I can't run Windows APIs in this Linux container.** Development and testing must go through `windows-latest` CI plus real Windows machines (including an HDR display). We can `cargo check --target x86_64-pc-windows-msvc` locally with `cargo-xwin`.
- **Anti-cheat and DRM windows** return black frames under WGC. That's expected and should be documented.

**Questions for you**
1. **Licence**: GPL-3.0 (simplest; allows x264/x265) or permissive/LGPL (more limited software-encoding options)?
2. **Name/branding**: keep `ssx`?
3. **UI toolkit**: fine with egui as the default, pending the M0 spike?
4. **Code signing**: do you have, or will you get, a certificate? It gates M6 and a frictionless installer.
5. **Which uploaders matter most** to you (beyond Imgur/S3/SFTP)? That reorders §7.
6. **HDR default**: "Reinhard-extended" (looks good, alters highlights) or "Clip to SDR white" (matches SDR apps exactly) as the out-of-the-box operator?

## 12. Sources
- [RustDesk — HDR tonemap PR (scRGB, SDR white level, DuplicateOutput1)](https://github.com/rustdesk/rustdesk/pull/16033)
- [hdrfix — HDR→SDR tonemapping tool in Rust](https://github.com/bvibber/hdrfix)
- [HDR_Screenshot_tool_for_windows](https://github.com/MagestiUA/HDR_Screenshot_tool_for_windows)
- [ShareX](https://github.com/sharex/sharex), [custom uploader docs](https://getsharex.com/docs/custom-uploader), [CustomUploaders](https://github.com/ShareX/CustomUploaders)
- [Static linking ffmpeg bindings (Rust forum)](https://users.rust-lang.org/t/static-linking-ffmpeg-bindings/105083), [rust-ffmpeg#233](https://github.com/zmwangx/rust-ffmpeg/issues/233), [rust-ffmpeg-cli (in-process NVENC)](https://github.com/PlanetLOF/rust-ffmpeg-cli)
- [Extending the Context Menu and Share Dialog in Windows 11](https://blogs.windows.com/windowsdeveloper/2021/07/19/extending-the-context-menu-and-share-dialog-in-windows-11/), [IExplorerCommand (windows-rs)](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/UI/Shell/struct.IExplorerCommand.html), [windows11-context-rs](https://github.com/SalahaldinBilal/windows11-context-rs)
