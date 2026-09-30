// Source = 8-bit RGBA / BGRA texture (Rgba8Unorm or Bgra8Unorm; `textureLoad` always
// returns channels in r, g, b order). Prefixed by yuv_core.wgsl.

@group(0) @binding(3) var src: texture_2d<f32>;

fn fetch_rgb(x: i32, y: i32) -> vec3<i32> {
    let p = textureLoad(src, vec2<i32>(x, y), 0);
    return vec3<i32>(i32(p.x * 255.0 + 0.5), i32(p.y * 255.0 + 0.5), i32(p.z * 255.0 + 0.5));
}
