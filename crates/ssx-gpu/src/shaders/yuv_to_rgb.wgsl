// NV12 / I420 planes (storage buffer of u32 words) -> Rgba8Unorm storage texture.
// Integer maths identical to `yuv::cpu::yuv_pixel`; chroma is upsampled by replication.

struct P {
    // visible w, h, coded w, coded h
    dims: vec4<u32>,
    // y_offset, u_offset (NV12: the UV plane), v_offset, layout (0 = NV12, 1 = I420)
    offs: vec4<u32>,
    // y_stride, chroma_stride (bytes), unused, unused
    strides: vec4<u32>,
    // y_scale, rv, gu, gv (Q16)
    inv_a: vec4<i32>,
    // bu (Q16), y_offset (code), unused, unused
    inv_b: vec4<i32>,
}

@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var<storage, read> planes: array<u32>;
@group(0) @binding(2) var dst: texture_storage_2d<rgba8unorm, write>;

fn byte_at(i: u32) -> i32 {
    return i32((planes[i >> 2u] >> ((i & 3u) * 8u)) & 0xffu);
}

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= p.dims.x || gid.y >= p.dims.y) {
        return;
    }
    let cx = gid.x / 2u;
    let cy = min(gid.y / 2u, p.dims.w / 2u - 1u);
    let yy = byte_at(p.offs.x + gid.y * p.strides.x + gid.x);
    var u: i32;
    var v: i32;
    if (p.offs.w == 0u) {
        let o = p.offs.y + cy * p.strides.y + cx * 2u;
        u = byte_at(o);
        v = byte_at(o + 1u);
    } else {
        let o = cy * p.strides.y + cx;
        u = byte_at(p.offs.y + o);
        v = byte_at(p.offs.z + o);
    }
    let y = p.inv_a.x * (yy - p.inv_b.y);
    let du = u - 128;
    let dv = v - 128;
    let r = (y + p.inv_a.y * dv + 32768) >> 16u;
    let g = (y + p.inv_a.z * du + p.inv_a.w * dv + 32768) >> 16u;
    let b = (y + p.inv_b.x * du + 32768) >> 16u;
    let c = vec3<f32>(vec3<i32>(clamp(r, 0, 255), clamp(g, 0, 255), clamp(b, 0, 255)));
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(c / 255.0, 1.0));
}
