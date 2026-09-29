// Shared HDR -> SDR maths. A line-by-line port of `ssx_hdr::params` / `srgb` / `dither`;
// keep the two in sync (see README "Parity"). This file has no entry point and is
// concatenated in front of the shader that uses it (WGSL has no `#include`).

// Mirror of `ssx_hdr::GpuParams` (48 bytes). Checked by the layout tests.
struct Params {
    scale: f32,
    knee: f32,
    peak: f32,
    headroom: f32,
    mode: u32,
    dither: u32,
    width: f32,
    pad: f32,
    c: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;

const MODE_CHANNEL_CLIP: u32 = 0u;
const MODE_HUE_CLIP: u32 = 1u;
const MODE_REINHARD: u32 = 2u;
const MODE_HERMITE: u32 = 3u;
const MODE_ACES: u32 = 4u;

// Scales a raw scRGB channel to SDR-relative light and sanitises non-finite values:
// NaN -> 0, +Inf -> max(peak, 1), -Inf -> 0. WGSL has no isnan/isinf and drivers may
// compile with fast-math, so the classification looks at the IEEE bits.
fn scale_channel(v: f32) -> f32 {
    let x = v * params.scale;
    let b = bitcast<u32>(x);
    if ((b & 0x7f800000u) == 0x7f800000u) {
        if ((b & 0x007fffffu) != 0u || (b >> 31u) == 1u) {
            return 0.0;
        }
        return max(params.peak, 1.0);
    }
    return x;
}

fn luminance(x: vec3<f32>) -> f32 {
    return 0.2126 * x.x + 0.7152 * x.y + 0.0722 * x.z;
}

fn gamut_map(x: vec3<f32>) -> vec3<f32> {
    let mn = min(min(x.x, x.y), x.z);
    if (mn >= 0.0) {
        return x;
    }
    let y = luminance(x);
    if (y <= 0.0) {
        return vec3<f32>(0.0);
    }
    let t = y / (y - mn);
    return max(vec3<f32>(y) + t * (x - vec3<f32>(y)), vec3<f32>(0.0));
}

fn rolloff(m: f32) -> f32 {
    let k = params.knee;
    if (params.mode == MODE_CHANNEL_CLIP || params.mode == MODE_HUE_CLIP) {
        return min(m, 1.0);
    }
    if (m <= k) {
        return m;
    }
    if (m >= params.peak) {
        return 1.0;
    }
    let d = m - k;
    var f: f32;
    if (params.mode == MODE_REINHARD) {
        let v = d / params.headroom;
        f = k + params.headroom * (v * (1.0 + v * params.c.y) / (1.0 + v));
    } else if (params.mode == MODE_HERMITE) {
        let t = d / params.width;
        let a = params.c.x;
        var s: f32;
        if (t < 0.5) {
            s = t * (1.0 - t) * (1.0 - t) * a + t * t * (3.0 - 2.0 * t);
        } else {
            let u = 1.0 - t;
            s = 1.0 - u * u * ((3.0 - a) + (a - 2.0) * u);
        }
        f = k + params.headroom * s;
    } else if (params.mode == MODE_ACES) {
        let z = params.c.x + params.c.y * d;
        let n = z * (2.51 * z + 0.03) / (z * (2.43 * z + 0.59) + 0.14);
        f = k + params.c.z * (n - params.c.w);
    } else {
        // Linear shoulder.
        f = k + params.headroom * (d / params.width);
    }
    return clamp(f, k, 1.0);
}

// `ssx_hdr::tonemap_scaled`: input already scaled and finite.
fn tonemap_scaled(scaled: vec3<f32>) -> vec3<f32> {
    let x = gamut_map(scaled);
    if (params.mode == MODE_CHANNEL_CLIP) {
        return clamp(x, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    let m = max(max(x.x, x.y), x.z);
    if (m <= params.knee) {
        return clamp(x, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    let f = rolloff(m);
    let s = f / m;
    // The brightest channel is set to `f` exactly, like the CPU reference.
    return vec3<f32>(
        select(clamp(x.x * s, 0.0, 1.0), f, x.x == m),
        select(clamp(x.y * s, 0.0, 1.0), f, x.y == m),
        select(clamp(x.z * s, 0.0, 1.0), f, x.z == m),
    );
}

// Piecewise sRGB OETF (IEC 61966-2-1); input in [0, 1].
fn srgb_oetf(x: f32) -> f32 {
    if (x <= 0.0031308) {
        return 12.92 * x;
    }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

// Truncating fract for non-negative values (matches `dither::fract_pos`).
fn fract_pos(f: f32) -> f32 {
    return f - f32(i32(f));
}

// Interleaved gradient noise (Jimenez 2014) of the frame-global pixel coordinate.
fn dither_noise(x: u32, y: u32) -> f32 {
    let f = fract_pos(0.06711056 * f32(x) + 0.00583715 * f32(y));
    return fract_pos(52.982918 * f);
}

// `dither::quantize`: values within 0.15 code of an integer are rounded, never dithered.
fn quantize(v0: f32, noise: f32) -> u32 {
    let v = clamp(v0, 0.0, 255.0);
    let r = u32(v + 0.5);
    let d = u32(v + noise);
    var out = d;
    if (abs(v - f32(r)) <= 0.15) {
        out = r;
    }
    return min(out, 255u);
}

fn encode_channel(linear: f32, noise: f32) -> u32 {
    return quantize(255.0 * srgb_oetf(clamp(linear, 0.0, 1.0)), noise);
}

// Whole pipeline for one pixel: raw scRGB in, 8-bit sRGB codes out. `gxy` is the
// frame-global pixel coordinate (the dither must not depend on tiling).
fn tonemap_codes(px: vec3<f32>, gxy: vec2<u32>) -> vec3<u32> {
    let scaled = vec3<f32>(scale_channel(px.x), scale_channel(px.y), scale_channel(px.z));
    let lin = tonemap_scaled(scaled);
    var noise = 0.5;
    if (params.dither != 0u) {
        noise = dither_noise(gxy.x, gxy.y);
    }
    return vec3<u32>(
        encode_channel(lin.x, noise),
        encode_channel(lin.y, noise),
        encode_channel(lin.z, noise),
    );
}
