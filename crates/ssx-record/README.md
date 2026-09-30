# ssx-record

Screen recording for ssx: frame sources, audio, encoders, and the session that ties them
together. Implements `ssx_core::workflow::Recorder` (`adapter::SsxRecorder`).

```text
FrameSource -> capture thread (CfrPacer) -> lossy queue -> convert -> encoder thread -> file
AudioSource -> audio thread (resample, drift, mix) ----------------> (same encoder)
```

## Platforms and what was verified

"Live" means the code ran against the real thing on the Linux development host (no GPU, no
desktop session); "fixture" means a faithful stand-in (mock compositor, mock portal); "compile"
means type-checked and linted only.

| Platform / path | Source | Verification |
|---|---|---|
| Linux X11 (and XWayland) | `source::x11::X11Source`, paced MIT-SHM `GetImage`, XFixes cursor | **live** under Xvfb: a moving box is recorded, decoded and tracked; region crop; 1080p capture rate measured |
| Linux wlroots (sway, Hyprland) | `source::wlroots::WlrootsSource`, own streaming loop over `ext-image-copy-capture-v1` (session per output, damage driven) and `wlr-screencopy` (`copy_with_damage` v2+) | **live** on headless sway 1.9 for `wlr-screencopy` (sway 1.9 has no ext protocol); the **ext** path, formats, transforms, renegotiation, multi-output stitching and failures against the mock compositor of `ssx-capture-wayland` (**fixture**) |
| Linux GNOME / KDE | `source::portal::PortalSource`: xdg-desktop-portal ScreenCast via `ashpd`, restore token persisted, PipeWire stream with memfd SHM buffers | **live** against a private D-Bus + real `pipewire` + `wireplumber` with a **mock** ScreenCast portal and a PipeWire video producer node: full CreateSession/SelectSources/Start/OpenPipeWireRemote flow, restore-token reuse, cancel, missing portal. **Not** run on a real GNOME/KDE; a DMA-BUF-only compositor is reported as `SourceError::UnsupportedBuffer` (code path present, not exercised) |
| Windows | `source::windows::WgcSource`: Windows.Graphics.Capture streaming (free-threaded pool, reused staging texture, HDR float pool) | **compile** + clippy for `x86_64-pc-windows-msvc`; the pure logic (`source::wgc_logic`: formats, pitch removal, pool resize, timestamp mapping, target resolution) is unit-tested everywhere. Never run on Windows |
| macOS | none (ScreenCaptureKit not implemented) | encoding only |
| any | `source::synthetic::SyntheticSource`: colour bars with a burnt-in frame counter (realtime or virtual time, BGRA/RGBA/HDR) | live; the backbone of the session tests |

Audio (`audio` feature, `cpal` 0.18):

| Path | Verification |
|---|---|
| PipeWire host: system loopback (input stream on the default sink) + microphone | **live** on a private headless PipeWire: tones injected into a null sink and a virtual mic appear in the recorded file, mixed, in sync |
| PulseAudio host (monitor sources are plain inputs) | selection logic only |
| WASAPI loopback (an input stream on an output device), CoreAudio aggregate loopback | **compile** only |
| ALSA | no loopback exists; microphone only |

cpal's loopback support (checked in its source): WASAPI (input stream on an output device),
PipeWire (sinks are duplex, `stream.capture.sink` is set), PulseAudio (monitors as inputs),
CoreAudio aggregate taps on macOS 14.6+; ALSA has none. Any audio failure (no device, device
dies mid-recording) degrades to video-only or to the remaining source; it never fails the video.

## Encoders

`Encoder` trait, implemented by FFmpeg (`ffmpeg-next` 9) and GIF (`gifski`).

* Selection is a candidate chain, probed at run time (an encoder is opened and fed five frames)
  and cached: hardware first (NVENC, AMF, QSV, VA-API, VideoToolbox, MediaFoundation by
  platform), then libx264 (`gpl` feature) / libopenh264, then other codecs the container can
  hold (VP9, AV1, HEVC), ending at the always-present native `mpeg4`.
* H.264 default, HEVC and AV1 selectable, VP9 for WebM; AAC / Opus audio; MP4 (faststart or
  fragmented), WebM, MKV; CRF or bitrate; speed presets; keyframe interval.
* Colour: BT.709 limited range everywhere (swscale matrix set explicitly, encoder tagged).
* HDR (`Rgba16F` scRGB) frames are tone-mapped with `ssx-hdr` on the CPU or, when the `gpu`
  feature finds an adapter, by the fused tone-map + NV12 pass of `ssx-gpu`.
* GIF: gifski with fps cap, exact-duplicate skipping and resizing.

Encoders available on the development host (FFmpeg 6.1, software only in the sandbox): `libx264`
(H.264), `libx265`, `libaom-av1`, `librav1e`, `libsvtav1`, `mpeg4`, `libvpx-vp9`, AAC, Opus, and
the VA-API/NVENC/QSV wrappers (listed but unusable without hardware; the probe rejects them).
`cargo test -p ssx-record --test encode_e2e -- --nocapture prints_the_encoders` lists them.
The VA-API upload path (`encode::ffmpeg::hw`) is unsafe FFI that could not be run on hardware.

## Session

`session::RecordingSession` (`start`, `pause`, `resume`, `stats`, `stop`, `abort`):

* one shared monotonic `Clock` for video and audio;
* constant frame rate: the pacer duplicates frames over idle stretches and drops surplus;
* bounded queues; the only place frames are ever dropped is capture -> convert, and only
  cheap duplicates are evicted first; offline sources get backpressure instead;
* exact counters (`slots = encoded + dropped_backpressure + dropped_convert`, and
  `slots = captured - surplus - paused_discarded + duplicated`, both asserted by the tests);
* pause/resume compacts the timeline for video and audio; `max_duration` / `max_bytes` guards;
* `stop` finalises the file, `abort` (and dropping the session) deletes it.

## Performance

See the table at the end of this file (`examples/perf.rs`).

## Examples

```sh
cargo run -p ssx-record --release --example record -- \
    --seconds 10 --fps 30 -o out.mp4 --source x11 --audio
cargo run -p ssx-record --release --example perf -- --encoder libx264 --speed fastest
```

## Tests

```sh
cargo test -p ssx-record
```

Live tests skip with a printed reason when their tools are missing (`Xvfb`, `sway`,
`dbus-daemon`, `pipewire`, `wireplumber`, `pw-cli`, `pw-loopback`, `pw-play`). Files are decoded
back with FFmpeg (`verify::inspect`) and compared with what the counters claim; MP4 layout
(`moov` before `mdat` for faststart) is checked at the box level, and `ffprobe` is used as a
second opinion when present.

## Features

`default = system, gif, gpl, audio, gpu, portal`. `static`, `static-prebuilt` and
`static-hw-*` link FFmpeg statically and are **unverified**: see `build/README.md` for exactly
what a CI job must do. Licences: `gifski` is AGPL-3.0-or-later; libx264/libx265 are GPL
(behind the `gpl` feature); an LGPL-only build is possible (`--no-default-features`).
