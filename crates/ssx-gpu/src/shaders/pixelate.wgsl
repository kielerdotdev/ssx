// Block-average pixelate. One invocation per block: integer sums, so the result is
// bit-identical to the CPU reference. Blocks at the right/bottom edge are clipped.

struct P {
    // width, height, block size, unused
    dims: vec4<u32>,
}

@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var dst: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let b = p.dims.z;
    let x0 = gid.x * b;
    let y0 = gid.y * b;
    if (x0 >= p.dims.x || y0 >= p.dims.y) {
        return;
    }
    let x1 = min(x0 + b, p.dims.x);
    let y1 = min(y0 + b, p.dims.y);
    var sum = vec4<u32>(0u);
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            let c = textureLoad(src, vec2<i32>(i32(x), i32(y)), 0);
            sum += vec4<u32>(vec4<f32>(c * 255.0 + vec4<f32>(0.5)));
        }
    }
    let n = (x1 - x0) * (y1 - y0);
    let avg = (sum + vec4<u32>(n / 2u)) / vec4<u32>(n);
    let out = vec4<f32>(avg) / 255.0;
    for (var y = y0; y < y1; y++) {
        for (var x = x0; x < x1; x++) {
            textureStore(dst, vec2<i32>(i32(x), i32(y)), out);
        }
    }
}
