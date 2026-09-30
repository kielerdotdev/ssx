# ssx-gpu

The wgpu compute pipeline of ssx: HDR to SDR tonemapping, video-frame conversion
(NV12 / I420) and the small effect shaders of the editor. One WGSL source per operation,
run through wgpu on every OS (Vulkan, Metal, DX12; naga compiles the WGSL), headless, no
window or surface involved.

| Type | Job |
|---|---|
| `GpuContext` | Adapter selection, limits, error scopes, device-loss recovery. `Send + Sync`, `GpuContext::global()` for a lazily created shared instance. |
| `GpuTonemapper` | `Rgba16F` scRGB `Frame` to `Rgba8` sRGB `Frame`, same maths as `ssx-hdr`. `tonemap_texture` / `TonemapPass` keep frames on the GPU. |
| `YuvConverter` | `Rgba8`/`Bgra8` to NV12/I420, HDR tonemap + NV12/I420 in **one dispatch**, NV12/I420 back to RGBA. `YuvPass` keeps frames on the GPU. |
| `GpuFx` | Gaussian blur and pixelate of a region, bilinear/Lanczos-3 resize. |
| `ssx_gpu::yuv::cpu`, `ssx_gpu::fx::cpu` | CPU reference implementations (used by the tests, usable as a fallback). |

```rust
let ctx = ssx_gpu::GpuContext::global()?;             // or GpuContext::new(&GpuOptions{..})
let tonemapper = ssx_gpu::GpuTonemapper::new(ctx)?;
let sdr = tonemapper.tonemap(&hdr_frame, &ssx_hdr::TonemapSettings::default())?;
```

## Adapter selection

Policy: discrete GPU, then integrated, then virtual/other, then software (llvmpipe,
WARP). Vulkan/Metal/DX12 first, GL only if none of them yields an adapter. Overrides:

| Variable | Meaning |
|---|---|
| `SSX_GPU_BACKEND` | `vulkan`, `dx12`, `metal`, `gl`, `auto` (falls back to wgpu's `WGPU_BACKEND`) |
| `SSX_GPU_ADAPTER` | case-insensitive substring of the adapter name, or `#N` (N-th adapter of the ranked list) |
| `SSX_GPU_FALLBACK=1` | only software adapters (`force_fallback_adapter`) |

`GpuOptions` has the same knobs plus `prefer_low_power`. No adapter gives
`GpuError::NoAdapter`; nothing in the crate panics on GPU failure (every operation runs in
out-of-memory/validation/internal error scopes that become `GpuError`, and wgpu's default
panic-on-uncaptured-error is replaced by a logging handler).

No optional features are requested. In particular **`shader-f16` is not needed**: HDR data
is uploaded as `Rgba16Float` textures and the texture unit converts it to `f32`. Limits
start from the downlevel defaults and are raised to the adapter's values only for texture
size, buffer size and dispatch size.

A lost device is detected through wgpu's callback; the next call recreates the device (new
`generation`), and `GpuTonemapper`/`YuvConverter`/`GpuFx` rebuild their cached objects and
retry the call once.

## Parity with `ssx-hdr` (the tolerance statement)

The shader (`src/shaders/tonemap_common.wgsl`) is a line-by-line port of `ssx_hdr::params`,
`srgb` and `dither`, including the "within 0.15 code of an integer is rounded, never
dithered" dead zone, and non-finite handling done on the IEEE bits (WGSL has no
`isnan`/`isinf`, and drivers may compile with fast-math). The uniform struct matches
`ssx_hdr::GpuParams` (checked against `offset_of!` and against the layout naga computes
from the WGSL).

**Contract**

1. **SDR-representable content is byte-exact**: every pixel whose value is an 8-bit sRGB
   colour (as stored in a half-float frame, at any `sdr_white_nits`) comes out
   *identical* to the CPU reference and to the original 8-bit value, for all operators,
   dither on or off. 0 differences allowed.
2. **Everything else is within one code value** of the CPU reference per channel
   (never two).
3. **At most 0.01 % of pixels differ at all** (the tests fail above that, with a floor of
   three pixels for tiny samples).

Reasons for the residual one-code differences: `pow` (`exp2(log2)` on GPUs, correctly
rounded `powf` on the CPU) and fused multiply-add contraction move a value across a
rounding or dither threshold in a handful of pixels. RGB to YUV and pixelate use integer
arithmetic and are bit-exact; blur and resize share the CPU's weight tables and differ only
by float accumulation order.

**Measured** (software Vulkan, llvmpipe LLVM 20.1.2; real GPUs have less precise `pow` and
were **not** available, so re-run `cargo test -p ssx-gpu -- --nocapture` on hardware):

| Test | Pixels | Differ from CPU | Max diff |
|---|---|---|---|
| SDR ramp, 8 white levels x 4 operators x dither on/off x grey/R/G/B | 65 536 | **0** | 0 |
| SDR-exact pixels mixed into HDR noise (dither on, 4 operators) | 40 936 exact | **0** | 0 |
| HDR gradients to 10x white, 5 knee/peak/exposure sets, 3 white levels | 576 960 | 3 (0.0005 %) | 1 |
| Random colours incl. negatives, 5 ranges, 4 operators, dither on/off | 1 310 720 | 10 (0.0008 %) | 1 |
| Tiny/odd/padded sizes (1x1 .. 257x257, 6 stride paddings) | 1 354 932 | 47 (0.0035 %) | 1 |
| All 8-bit-representable NaN/Inf/extreme half-float combinations | 16 000 | 0 | 0 |
| Frame wider than `max_texture_dimension_2d` (16421x3, tiled) | 49 263 | 0 | 0 |
| Tiled (down to 1x1 tiles) vs untiled | 26 593 | **0 (byte-identical)** | 0 |
| RGB to NV12/I420, every matrix/range/siting, odd sizes, padded strides, RGBA and BGRA | all | **0 (bit-exact)** | 0 |
| Fused tonemap + YUV vs CPU tonemap then convert | 42 120 bytes | 0 | 0 |
| Pixelate | all | **0 (bit-exact)** | 0 |
| Gaussian blur (sigma 0.4 .. 30, regions incl. 1x1, 1xN) | ~300 000 | <= 1 pixel per test | 1 |
| Bilinear / Lanczos-3 resize | up to 80 000 | up to 0.85 % (bilinear upscale of a gradient, rounding at exact .5) | 1 |

The two determinism guarantees are tested too: the same input twice, from four
threads concurrently and through a freshly created tonemapper gives identical bytes; and
tiling never changes the result because the dither noise is a function of the
frame-global pixel coordinate.

## Behaviour notes

* **Upload.** `Queue::write_texture` is given the frame's own stride as `bytes_per_row`
  (it has no 256-byte requirement and repacks internally), so padded capture buffers are
  uploaded without an intermediate copy and padding bytes are never read. Readback goes
  through a staging buffer whose rows are padded to 256 bytes
  (`COPY_BYTES_PER_ROW_ALIGNMENT`) and un-padded while copying into the output `Frame`.
* **Tiling.** Tonemap tiles are as large as `max_texture_dimension_2d` and a 128 MiB staging
  budget allow (`TileLimits` lowers them, e.g. for tests). 8K needs one tile on typical
  GPUs; frames wider than the texture limit are split into columns as well as rows.
  YUV conversion splits into bands of even height (bands are independent because chroma
  only spans rows `2cy, 2cy+1`); a frame wider than the texture limit is a
  `GpuError::TooLarge`. The effects do not tile (regions are interaction-sized).
* **Caching.** Pipelines are built once per device generation. Textures, staging and
  uniform buffers and bind groups are cached per size class (dimensions rounded up to 64,
  four sets kept, taken out of the cache while in use so concurrent calls do not block).
* **NV12/I420.** Planes are tightly packed at the *coded* size, i.e. odd dimensions are
  rounded up to even by replicating the last column/row (the visible size is kept in
  `YuvFrame`). BT.709 limited range by default, BT.601 and full range selectable.
  `ChromaSiting::Center` (default) is the plain 2x2 average; `ChromaSiting::Left`
  (co-sited with the even luma column, `[1 2 1]` horizontal filter, the MPEG-2/H.264
  default) is available; tell the encoder which one you used (`chroma_sample_location`),
  because players assume `left` when nothing is signalled.
* **Effects** work on the encoded sRGB values channel by channel (alpha included) and use
  only the pixels of the region (edges are clamped), so nothing outside a redaction leaks
  into it.

## Integration: the recording path stays on the GPU

For a recording pipeline no frame should touch the CPU between capture and encoder input:

1. The capture backend delivers frames as GPU textures (or, until zero-copy import exists,
   uploads them once into an `Rgba16Float` / `Bgra8Unorm` texture that is *reused*).
2. Create the passes once:
   * HDR desktop: `YuvConverter::create_pass(&view, YuvInput::HdrScRgb, size, &opts)`,
     then per frame `pass.set_tonemap(..)` (only when the settings or the SDR white level
     change), `pass.set_region(size, [0, 0])`, `pass.record(&mut encoder, size)`. Tonemap
     and NV12 conversion are one dispatch pair; nothing is allocated per frame.
   * SDR desktop: the same with `YuvInput::Bgra8`/`Rgba8`.
   * Preview of the tonemapped frame (overlay, thumbnails): `GpuTonemapper::create_pass`
     + `TonemapPass::record` into an `Rgba8Unorm` storage texture that egui/wgpu can
     sample directly.
3. `pass.output()` is a `STORAGE | COPY_SRC` buffer holding the planes (`PlaneLayout`
   gives strides and offsets). Copy it into a `MAP_READ` ring of staging buffers with
   `copy_buffer_to_buffer` in the same command buffer and map the previous frame's buffer
   while the GPU works on the next one (the convenience `YuvConverter::convert` does this
   synchronously). Hand the mapped bytes to the encoder (`ffmpeg` software or hardware
   upload path).

`GpuTonemapper::tonemap_texture(input, output, ..)` is the one-shot texture to texture
form for callers that do not want to manage a pass.

### Plan: zero-copy import of D3D11 shared textures on Windows (documentation only)

Not implemented; needs a Windows machine and `unsafe` (this crate is
`#![forbid(unsafe_code)]`, so the import lives in a separate small crate).

1. WGC / Desktop Duplication produce `ID3D11Texture2D` (`R16G16B16A16_FLOAT`) on a D3D11
   device. Copy the frame with `CopyResource` into a ring of textures created with
   `D3D11_RESOURCE_MISC_SHARED_NTHANDLE | SHARED` on the **same adapter** as wgpu's DX12
   device (compare LUIDs: `ID3D12Device::GetAdapterLuid` through `Adapter::as_hal`).
2. `IDXGIResource1::CreateSharedHandle` gives an NT handle per ring slot, created once.
   In the wgpu device (`Device::as_hal::<Dx12>`): `ID3D12Device::OpenSharedHandle` gives an
   `ID3D12Resource`; wrap it with wgpu-hal's `dx12::Device::texture_from_raw`
   (verify the exact signature for the pinned wgpu version) and turn it into a
   `wgpu::Texture` with `Device::create_texture_from_hal::<Dx12>` (usage
   `TEXTURE_BINDING`). Each slot becomes one `YuvPass` / `TonemapPass` input, created once.
3. Synchronisation: a shared fence (`ID3D11Device5::CreateFence(SHARED)` opened as an
   `ID3D12Fence`). The capture thread signals value *n* after the copy
   (`ID3D11DeviceContext4::Signal`); before submitting the pass the wgpu side must wait for
   *n* on its queue (`ID3D12CommandQueue::Wait` through `Queue::as_hal`), since wgpu has no
   API for external semaphores. Keyed mutexes are the fallback where fences are missing.
4. The shader output is a storage buffer, not an NV12 texture (WGSL cannot write NV12
   planes; R8 storage textures are not baseline). For NVENC/AMF/QSV via `d3d11va` a second
   import is needed: a shared `NV12` D3D11 texture that a buffer-to-plane copy fills. If
   that is not available in wgpu, the fallback of PLAN.md section 2 applies: a native
   D3D11 compute shader for the last step, or the mapped-buffer feed above.
5. macOS (IOSurface to Metal texture via `texture_from_raw`) and Linux (dma-buf through
   `VK_EXT_external_memory_dma_buf`) follow the same pattern with their own fence types.

## Testing

```
cargo test -p ssx-gpu                       # parity + validation tests
cargo test -p ssx-gpu -- --nocapture        # prints the measured numbers
cargo test --release -p ssx-gpu -- --ignored --nocapture   # benchmarks, 8K/16K frames
```

Shader validation (naga, no device) and all layout tests run everywhere. Tests that need
a device skip with a printed reason when no adapter exists, unless `SSX_GPU_REQUIRE=1`,
which CI sets so a broken runner cannot skip silently. On Linux CI install
`mesa-vulkan-drivers` (lavapipe) and run with `WGPU_BACKEND=vulkan`.

## Benchmarks (llvmpipe, software Vulkan, 4 cores; real GPUs are far faster)

BENCH_PLACEHOLDER

## Known gaps

* Not verified on real GPU hardware or on Windows/macOS at runtime (only compiled for the
  Windows target); parity numbers above are llvmpipe's.
* Zero-copy import (above) is documentation only.
* Effects do not tile; YUV cannot split a frame wider than the texture limit.
* NV12/I420 to RGBA uses nearest-neighbour chroma (a test/preview helper).
