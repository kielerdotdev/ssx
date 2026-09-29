// RGB -> YUV 4:2:0 (NV12 or I420) core, integer maths only so the CPU reference in
// `yuv.rs` is bit-exact. The including shader must provide
//     fn fetch_rgb(x: i32, y: i32) -> vec3<i32>
// returning the 8-bit RGB (0..255) of source pixel (x, y) (coordinates already clamped).
//
// Bindings: 1 = YuvParams (uniform), 2 = output planes (storage buffer of u32 words),
// 3 = source texture (declared by the including shader).

struct YuvParams {
    // src_w, src_h (visible size of this band), coded_w, coded_h (rounded up to even)
    dims: vec4<u32>,
    // y_stride, uv_stride (bytes, multiples of 4), y_offset, u_offset (bytes)
    strides: vec4<u32>,
    // v_offset (bytes), layout (0 = NV12, 1 = I420), log2 of the chroma weight sum, unused
    misc: vec4<u32>,
    // Column weights of the chroma filter for source columns 2cx-1, 2cx, 2cx+1.
    taps: vec4<i32>,
    // Q16 coefficients for R, G, B and the bias (including the +0.5 rounding term).
    ky: vec4<i32>,
    ku: vec4<i32>,
    kv: vec4<i32>,
    // Y min/max, chroma min/max (clamping range of the output codes).
    range: vec4<i32>,
}

@group(0) @binding(1) var<uniform> yp: YuvParams;
@group(0) @binding(2) var<storage, read_write> planes: array<u32>;

fn luma_at(x: i32, y: i32) -> u32 {
    let sw = i32(yp.dims.x);
    let sh = i32(yp.dims.y);
    let c = fetch_rgb(clamp(x, 0, sw - 1), clamp(y, 0, sh - 1));
    let v = (yp.ky.x * c.x + yp.ky.y * c.y + yp.ky.z * c.z + yp.ky.w) >> 16u;
    return u32(clamp(v, yp.range.x, yp.range.y));
}

// (U, V) of chroma sample (cx, cy): weighted RGB sum over source rows 2cy, 2cy+1 and
// columns 2cx-1..2cx+1, transformed by the chroma matrix.
fn chroma_at(cx: i32, cy: i32) -> vec2<u32> {
    let sw = i32(yp.dims.x);
    let sh = i32(yp.dims.y);
    var sum = vec3<i32>(0);
    for (var r = 0; r < 2; r++) {
        let py = clamp(2 * cy + r, 0, sh - 1);
        for (var k = 0; k < 3; k++) {
            var w = yp.taps.x;
            if (k == 1) { w = yp.taps.y; }
            if (k == 2) { w = yp.taps.z; }
            if (w != 0) {
                let px = clamp(2 * cx - 1 + k, 0, sw - 1);
                sum += w * fetch_rgb(px, py);
            }
        }
    }
    let shift = yp.misc.z;
    let u = (yp.ku.x * sum.x + yp.ku.y * sum.y + yp.ku.z * sum.z + (yp.ku.w << shift)) >> (16u + shift);
    let v = (yp.kv.x * sum.x + yp.kv.y * sum.y + yp.kv.z * sum.z + (yp.kv.w << shift)) >> (16u + shift);
    return vec2<u32>(u32(clamp(u, yp.range.z, yp.range.w)), u32(clamp(v, yp.range.z, yp.range.w)));
}

// Each invocation writes one u32 word = 4 consecutive Y bytes of one row.
@compute @workgroup_size(8, 8, 1)
fn luma(@builtin(global_invocation_id) gid: vec3<u32>) {
    let words_per_row = yp.strides.x / 4u;
    if (gid.x >= words_per_row || gid.y >= yp.dims.w) {
        return;
    }
    var word = 0u;
    for (var i = 0u; i < 4u; i++) {
        let x = gid.x * 4u + i;
        if (x < yp.dims.z) {
            word |= luma_at(i32(x), i32(gid.y)) << (8u * i);
        }
    }
    planes[(yp.strides.z + gid.y * yp.strides.x) / 4u + gid.x] = word;
}

// NV12: each invocation writes one word = U0 V0 U1 V1 (two chroma samples).
// I420: each invocation writes one word of the U plane and one of the V plane
// (four chroma samples each).
@compute @workgroup_size(8, 8, 1)
fn chroma(@builtin(global_invocation_id) gid: vec3<u32>) {
    let words_per_row = yp.strides.y / 4u;
    let rows = yp.dims.w / 2u;
    if (gid.x >= words_per_row || gid.y >= rows) {
        return;
    }
    let cw = yp.dims.z / 2u;
    if (yp.misc.y == 0u) {
        var word = 0u;
        for (var i = 0u; i < 2u; i++) {
            let cx = gid.x * 2u + i;
            if (cx < cw) {
                let uv = chroma_at(i32(cx), i32(gid.y));
                word |= (uv.x | (uv.y << 8u)) << (16u * i);
            }
        }
        planes[(yp.strides.w + gid.y * yp.strides.y) / 4u + gid.x] = word;
    } else {
        var wu = 0u;
        var wv = 0u;
        for (var i = 0u; i < 4u; i++) {
            let cx = gid.x * 4u + i;
            if (cx < cw) {
                let uv = chroma_at(i32(cx), i32(gid.y));
                wu |= uv.x << (8u * i);
                wv |= uv.y << (8u * i);
            }
        }
        let row = gid.y * yp.strides.y;
        planes[(yp.strides.w + row) / 4u + gid.x] = wu;
        planes[(yp.misc.x + row) / 4u + gid.x] = wv;
    }
}
