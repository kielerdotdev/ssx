#!/usr/bin/env bash
# Builds a static FFmpeg (plus the codec libraries ssx-record wants) into a prefix that
# `--features static-prebuilt` links against.
#
#   UNVERIFIED: written against the FFmpeg/x264/opus/libvpx/SVT-AV1 build systems from
#   their documentation. The development sandbox had no network access to clone the
#   sources, so no line of this script has been run. See README.md in this directory.
#
# Usage:
#   build/build-ffmpeg.sh [PREFIX]            # default PREFIX: $PWD/ffmpeg-static
#
# Environment (all optional):
#   JOBS            parallel jobs                        (default: nproc)
#   FFMPEG_REF      FFmpeg git ref                       (default: release/8.0)
#   X264_REF        x264 ref                             (default: stable)
#   OPUS_REF        opus ref                             (default: v1.5.2)
#   VPX_REF         libvpx ref                           (default: v1.15.0)
#   SVTAV1_REF      SVT-AV1 ref                          (default: v3.0.2)
#   NVCODEC_REF     nv-codec-headers ref                 (default: n13.0.19.0)
#   WITH_GPL=0      leave out libx264 (LGPL-only FFmpeg: hardware, native mpeg4, libvpx, SVT-AV1)
#   WITH_AV1=0      leave out SVT-AV1
#   WITH_HW=0       leave out VA-API / NVENC (Linux), D3D11VA/AMF/NVENC (Windows), VideoToolbox (macOS)
#   EXTRA_CFLAGS    e.g. "-march=x86-64-v2" for a portable binary (default: none; never -march=native)
#
# Windows: run from an MSYS2 MINGW64 shell *with the MSVC environment loaded*
# (`vcvars64.bat` first), so that `--toolchain=msvc` finds cl.exe and link.exe. vcpkg is the
# simpler route on Windows; see README.md.
set -euo pipefail

PREFIX="$(mkdir -p "${1:-$PWD/ffmpeg-static}" && cd "${1:-$PWD/ffmpeg-static}" && pwd)"
JOBS="${JOBS:-$( (command -v nproc >/dev/null && nproc) || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"
FFMPEG_REF="${FFMPEG_REF:-release/8.0}"
X264_REF="${X264_REF:-stable}"
OPUS_REF="${OPUS_REF:-v1.5.2}"
VPX_REF="${VPX_REF:-v1.15.0}"
SVTAV1_REF="${SVTAV1_REF:-v3.0.2}"
NVCODEC_REF="${NVCODEC_REF:-n13.0.19.0}"
WITH_GPL="${WITH_GPL:-1}"
WITH_AV1="${WITH_AV1:-1}"
WITH_HW="${WITH_HW:-1}"
EXTRA_CFLAGS="${EXTRA_CFLAGS:-}"

WORK="${WORK:-$PWD/ffmpeg-static-src}"
mkdir -p "$WORK"
OS="$(uname -s)"

export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
export CFLAGS="-O2 -fPIC $EXTRA_CFLAGS"
export CXXFLAGS="$CFLAGS"

fetch() { # url ref dir
  if [ ! -d "$WORK/$3/.git" ]; then
    git clone --depth 1 --branch "$2" "$1" "$WORK/$3"
  fi
}

build_x264() {
  fetch https://code.videolan.org/videolan/x264.git "$X264_REF" x264
  (cd "$WORK/x264" && ./configure --prefix="$PREFIX" --enable-static --disable-cli --enable-pic \
    && make -j"$JOBS" && make install)
}

build_opus() {
  fetch https://github.com/xiph/opus.git "$OPUS_REF" opus
  (cd "$WORK/opus" && cmake -S . -B build -DCMAKE_INSTALL_PREFIX="$PREFIX" -DBUILD_SHARED_LIBS=OFF \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DOPUS_BUILD_PROGRAMS=OFF -DOPUS_BUILD_TESTING=OFF \
    -DCMAKE_BUILD_TYPE=Release && cmake --build build -j"$JOBS" && cmake --install build)
}

build_vpx() {
  fetch https://chromium.googlesource.com/webm/libvpx "$VPX_REF" libvpx
  (cd "$WORK/libvpx" && ./configure --prefix="$PREFIX" --disable-examples --disable-tools \
    --disable-docs --disable-unit-tests --enable-vp9-highbitdepth --enable-pic \
    --enable-static --disable-shared && make -j"$JOBS" && make install)
}

build_svtav1() {
  fetch https://gitlab.com/AOMediaCodec/SVT-AV1.git "$SVTAV1_REF" svtav1
  (cd "$WORK/svtav1" && cmake -S . -B build -DCMAKE_INSTALL_PREFIX="$PREFIX" -DBUILD_SHARED_LIBS=OFF \
    -DBUILD_APPS=OFF -DBUILD_TESTING=OFF -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DCMAKE_BUILD_TYPE=Release && cmake --build build -j"$JOBS" && cmake --install build)
}

build_nvcodec_headers() { # NVENC is dlopen'ed at run time: only headers are needed
  fetch https://git.videolan.org/git/ffmpeg/nv-codec-headers.git "$NVCODEC_REF" nv-codec-headers
  (cd "$WORK/nv-codec-headers" && make PREFIX="$PREFIX" install)
}

[ "$WITH_GPL" = 1 ] && build_x264
build_opus
build_vpx
[ "$WITH_AV1" = 1 ] && build_svtav1

FF_ARGS=(
  --prefix="$PREFIX"
  --pkg-config-flags=--static
  --extra-cflags="-I$PREFIX/include $CFLAGS"
  --extra-ldflags="-L$PREFIX/lib"
  --disable-shared --enable-static --enable-pic
  --disable-programs --disable-doc --disable-debug
  --enable-libopus --enable-libvpx
)
[ "$WITH_GPL" = 1 ] && FF_ARGS+=(--enable-gpl --enable-libx264)
[ "$WITH_AV1" = 1 ] && FF_ARGS+=(--enable-libsvtav1)

case "$OS" in
  Linux)
    if [ "$WITH_HW" = 1 ]; then
      build_nvcodec_headers
      # libva is loaded dynamically by the system's driver stack; it cannot be linked
      # statically in a useful way. VA-API therefore stays a dynamic dependency of the
      # final binary (libva.so.2 + libva-drm.so.2), NVENC is dlopen'ed (libnvidia-encode).
      FF_ARGS+=(--enable-vaapi --enable-ffnvcodec)
    fi
    ;;
  Darwin)
    [ "$WITH_HW" = 1 ] && FF_ARGS+=(--enable-videotoolbox --enable-audiotoolbox)
    ;;
  MINGW*|MSYS*|CYGWIN*)
    FF_ARGS+=(--toolchain=msvc)
    if [ "$WITH_HW" = 1 ]; then
      build_nvcodec_headers
      FF_ARGS+=(--enable-d3d11va --enable-dxva2 --enable-amf --enable-ffnvcodec)
    fi
    ;;
esac

fetch https://github.com/FFmpeg/FFmpeg.git "$FFMPEG_REF" ffmpeg
(cd "$WORK/ffmpeg" && ./configure "${FF_ARGS[@]}" && make -j"$JOBS" && make install)

cat <<EOF

FFmpeg installed in $PREFIX. Build ssx-record against it:

  export FFMPEG_DIR="$PREFIX"
  export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
  export PKG_CONFIG_ALL_STATIC=1
  cargo build --release -p ssx-record --no-default-features \\
      --features "static-prebuilt,gif,audio,gpu,portal$( [ "$WITH_GPL" = 1 ] && echo ",gpl" )"

EOF
