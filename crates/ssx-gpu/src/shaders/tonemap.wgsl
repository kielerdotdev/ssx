// HDR (Rgba16Float scRGB) -> SDR (Rgba8Unorm holding sRGB codes), one pixel per invocation.
// Prefixed by tonemap_common.wgsl (binding 0 = Params).

struct Tile {
    // Frame-global coordinate of this tile's top-left pixel (dither phase).
    origin: vec2<u32>,
    // Tile size in pixels.
    size: vec2<u32>,
}

@group(0) @binding(1) var<uniform> tile: Tile;
@group(0) @binding(2) var src: texture_2d<f32>;
@group(0) @binding(3) var dst: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= tile.size.x || gid.y >= tile.size.y) {
        return;
    }
    let px = textureLoad(src, vec2<i32>(gid.xy), 0);
    let codes = tonemap_codes(px.xyz, tile.origin + gid.xy);
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(vec3<f32>(codes) / 255.0, 1.0));
}
