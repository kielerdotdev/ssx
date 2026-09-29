// One pass of a separable, weight-table driven filter. Used for the Gaussian blur and for
// bilinear / Lanczos resizing: the CPU builds, per output index along the pass axis, the
// first source index and a row of (zero padded) weights; the shader only does the sums, so
// the CPU reference (which uses the same tables) matches to float rounding.
//
// Pass H: 8-bit source texture (values 0..255) -> Rgba32Float intermediate (0..255 floats)
// Pass V: Rgba32Float intermediate -> Rgba8Unorm, rounded to the nearest code.
// Source indices are clamped to the source (edge replication).

struct P {
    // source width, source height (of the pass input), dest width, dest height
    dims: vec4<u32>,
    // taps per output index, unused...
    taps: vec4<u32>,
}

@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var<storage, read> starts: array<i32>;
@group(0) @binding(2) var<storage, read> weights: array<f32>;
@group(0) @binding(3) var src: texture_2d<f32>;
@group(0) @binding(4) var dst_f: texture_storage_2d<rgba32float, write>;
@group(0) @binding(5) var dst_u8: texture_storage_2d<rgba8unorm, write>;

// Horizontal pass: output (x, y) of a (dst_w x src_h) image.
@compute @workgroup_size(8, 8, 1)
fn pass_h(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.dims.z || gid.y >= p.dims.y) {
        return;
    }
    let n = p.taps.x;
    let s = starts[gid.x];
    let last = i32(p.dims.x) - 1;
    var acc = vec4<f32>(0.0);
    for (var k = 0u; k < n; k++) {
        let w = weights[gid.x * n + k];
        let sx = clamp(s + i32(k), 0, last);
        acc += w * (textureLoad(src, vec2<i32>(sx, i32(gid.y)), 0) * 255.0);
    }
    textureStore(dst_f, vec2<i32>(gid.xy), acc);
}

// Vertical pass: output (x, y) of the final (dst_w x dst_h) image.
@compute @workgroup_size(8, 8, 1)
fn pass_v(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.dims.z || gid.y >= p.dims.w) {
        return;
    }
    let n = p.taps.x;
    let s = starts[gid.y];
    let last = i32(p.dims.y) - 1;
    var acc = vec4<f32>(0.0);
    for (var k = 0u; k < n; k++) {
        let w = weights[gid.y * n + k];
        let sy = clamp(s + i32(k), 0, last);
        acc += w * textureLoad(src, vec2<i32>(i32(gid.x), sy), 0);
    }
    let code = clamp(floor(acc + vec4<f32>(0.5)), vec4<f32>(0.0), vec4<f32>(255.0));
    textureStore(dst_u8, vec2<i32>(gid.xy), code / 255.0);
}
