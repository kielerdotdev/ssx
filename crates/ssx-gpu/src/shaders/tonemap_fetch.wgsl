// Source = HDR Rgba16Float scRGB texture; every fetch runs the full tonemap so the YUV
// planes are those of "tonemap, then convert" (same 8-bit codes, same dither). Prefixed by
// tonemap_common.wgsl (binding 0 = Params) and yuv_core.wgsl.

@group(0) @binding(3) var src: texture_2d<f32>;

// Frame-global coordinate of this band's top-left pixel (dither phase).
@group(0) @binding(4) var<uniform> band_origin: vec4<u32>;

fn fetch_rgb(x: i32, y: i32) -> vec3<i32> {
    let p = textureLoad(src, vec2<i32>(x, y), 0);
    let g = vec2<u32>(band_origin.x + u32(x), band_origin.y + u32(y));
    return vec3<i32>(tonemap_codes(p.xyz, g));
}
