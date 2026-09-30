# Statically linked FFmpeg for `ssx-record`

> **Status: UNVERIFIED.** Everything in this directory (and the `static*` cargo features that
> go with it) was written from the build-system documentation of FFmpeg, ffmpeg-sys-next 9.0
> and the codec libraries. The development sandbox had no network access to FFmpeg's or the
> codec libraries' source repositories, so **no static build has ever been run**. What *has*
> been verified is the default `system` feature (dynamically linked FFmpeg 6.1 from the
> distribution): all encoder and session tests run against it. A CI job has to prove the rest;
> this file says exactly what that job must do.

## Why static

`ssx` ships as a desktop binary. Distribution FFmpeg builds differ (missing `libx264`, no
`libsvtav1`, old versions), and Windows/macOS have no system FFmpeg at all. A static FFmpeg
pins the encoder set and version. The price is licensing and build time (below).

## Cargo features

| Feature | What it does | Build needs |
|---|---|---|
| `system` (default) | links the distribution's `libav*` dynamically through pkg-config | `libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libswresample-dev` |
| `static` | `ffmpeg-sys-next` **clones FFmpeg `release/9.0` from GitHub at build time**, configures it (GPL, libx264, libx265, libvpx, libopus, PIC) and links it statically | network access to `github.com/FFmpeg/FFmpeg`; `nasm`, `make`, `git`, `pkg-config`; **static** codec libraries (`libx264.a`, `libx265.a`, `libvpx.a`, `libopus.a`) visible to pkg-config |
| `static-prebuilt` | links an FFmpeg you built yourself (`build-ffmpeg.sh`, or vcpkg) statically; nothing is compiled by cargo | `FFMPEG_DIR` (and/or `PKG_CONFIG_PATH`), `PKG_CONFIG_ALL_STATIC=1` |
| `static-hw-linux` / `-windows` / `-macos` | `static` plus the hardware-encoder configure switches of `ffmpeg-sys-next` (`build-vaapi`, `build-nvenc`, `build-amf`, `build-lib-d3d11va`, `build-videotoolbox`, ...) | as `static`, plus the platform SDK bits below |

`static*` features must be combined with `--no-default-features` (they replace `system`).
Keep `gif`, `audio`, `gpu` and `portal` as needed:

```sh
cargo build --release -p ssx-record --no-default-features \
    --features "static-prebuilt,gif,audio,gpu,portal,gpl"
```

The `gpl` feature only controls whether ssx *offers* libx264/libx265; the linked FFmpeg's
configure flags decide what exists. `ffmpeg-next`'s `build-license-gpl` (used by `static`)
must match: a GPL FFmpeg linked without `gpl` merely never selects x264.

## What the CI job must do

### 1. Linux (Ubuntu 22.04/24.04 runner)

```sh
sudo apt-get update
sudo apt-get install -y nasm yasm pkg-config cmake git build-essential \
    libx264-dev libx265-dev libvpx-dev libopus-dev \
    libva-dev libdrm-dev \
    libpipewire-0.3-dev libasound2-dev libpulse-dev libwayland-dev libx11-dev libxcb1-dev  # rest of ssx
# route A: cargo builds FFmpeg itself (needs the Debian -dev packages' static .a files)
cargo build --release -p ssx-record --no-default-features \
    --features "static-hw-linux,gif,audio,gpu,portal,gpl"
# route B: reproducible prefix, cached by the workflow
crates/ssx-record/build/build-ffmpeg.sh "$HOME/ffmpeg-static"
FFMPEG_DIR=$HOME/ffmpeg-static PKG_CONFIG_PATH=$HOME/ffmpeg-static/lib/pkgconfig \
PKG_CONFIG_ALL_STATIC=1 \
cargo build --release -p ssx-record --no-default-features \
    --features "static-prebuilt,gif,audio,gpu,portal,gpl"
```

Things to check after the first run:

* `ldd target/release/... | grep -E 'libav|libx264|libvpx|libopus'` must print nothing.
  `libva.so.2` / `libva-drm.so.2` stay dynamic on purpose (VA-API loads the distribution's
  driver stack); NVENC is `dlopen`ed at run time.
* `FFMPEG_MARCH` / the `build-portable` feature: `ffmpeg-sys-next` defaults to
  `-march=native`, which produces binaries that crash with `SIGILL` on older CPUs. **Set
  `FFMPEG_MARCH=x86-64-v2`** (or `armv8-a`) for release builds. `build-ffmpeg.sh` never uses
  `-march=native`.
* `FFMPEG_DIR` builds on Debian-like systems occasionally need `lib/x86_64-linux-gnu`
  additions to `PKG_CONFIG_PATH`.
* Run the crate's test-suite once against the static build:
  `cargo test -p ssx-record --no-default-features --features "static-prebuilt,gif,audio,gpu,portal,gpl"`.
  The encoder-selection tests print which encoders the build provides.

### 2. Windows (`windows-latest`, MSVC toolchain)

FFmpeg's configure is happiest in MSYS2, but the simplest supported route is **vcpkg**:

```powershell
git clone https://github.com/microsoft/vcpkg $env:USERPROFILE\vcpkg
& $env:USERPROFILE\vcpkg\bootstrap-vcpkg.bat
# static libraries, dynamic CRT (matches Rust's default MSVC target)
& $env:USERPROFILE\vcpkg\vcpkg install "ffmpeg[x264,x265,vpx,opus,nvcodec,amf,avcodec,avformat,swscale,swresample,gpl]:x64-windows-static-md"
$env:VCPKG_ROOT = "$env:USERPROFILE\vcpkg"
$env:VCPKGRS_TRIPLET = "x64-windows-static-md"
cargo build --release -p ssx-record --no-default-features --features "static-prebuilt,gif,audio,gpu,gpl"
```

`ffmpeg-sys-next` finds the vcpkg package through the `vcpkg` crate (`VCPKGRS_TRIPLET`) and
adds the system libraries vcpkg does not report (`ole32`, `secur32`, `ws2_32`, `bcrypt`,
`user32`). The Windows hardware encoders (NVENC, AMF, QSV through D3D11) are `dlopen`ed
from the GPU driver at run time, so the binary starts on machines without them. The
alternative, `static-hw-windows`, makes cargo drive FFmpeg's configure from an MSYS2 shell
(`--toolchain=msvc`), which also needs `nasm` and `pkg-config` on `PATH` and the `x264`,
`x265`, `vpx`, `opus` static libraries installed for the MSVC toolchain.

The Windows job must additionally run `cargo clippy --target x86_64-pc-windows-msvc`-style
checks natively and the `#[ignore]`d WGC test
(`cargo test -p ssx-record -- --ignored records_the_primary_monitor`) on an interactive
runner: the Windows capture and audio-loopback glue have **never run on Windows**.

### 3. macOS (`macos-14`)

```sh
brew install nasm pkg-config x264 x265 libvpx opus
cargo build --release -p ssx-record --no-default-features \
    --features "static-hw-macos,gif,audio,gpu,gpl"
```

There is no macOS frame source in this crate yet (ScreenCaptureKit), so macOS builds cover
encoding and GIF only; VideoToolbox is the intended H.264/HEVC path.

## Licensing consequences

* FFmpeg with `--enable-gpl` (needed for libx264/libx265) makes the *whole binary* GPL.
  ssx is GPL-3.0-or-later, which is compatible. Building **without** `--enable-gpl` yields an
  LGPL FFmpeg; then hardware encoders, `libvpx`, SVT-AV1 and the native `mpeg4`/`aac`
  encoders remain, and the H.264 software path is FFmpeg's `libopenh264` if built in.
  `build-ffmpeg.sh` supports this with `WITH_GPL=0`.
* `gifski` (GIF output) is **AGPL-3.0-or-later**.
* Static linking of LGPL libraries (FFmpeg without GPL) requires shipping the object files
  or the source for relinking; dynamic linking (`system`) sidesteps that.

## Size and time

A cold `static` build compiles FFmpeg (several minutes on 4 cores, `--disable-programs`
keeps the output small; expect roughly 15-25 MB added to the binary with x264/x265/vpx/opus).
Cache `$HOME/ffmpeg-static` (route B) or `target/release/build/ffmpeg-sys-next-*` between CI
runs.

`ci-static-ffmpeg.yml` in this directory is a ready-to-adapt GitHub Actions workflow with
the Linux and Windows jobs above.
