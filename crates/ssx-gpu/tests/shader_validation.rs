//! Device-less checks of the WGSL: parse and validate every shader with `naga` (so this
//! runs on every CI runner, GPU or not) and verify that the `Params` uniform struct has
//! exactly the layout of `ssx_hdr::GpuParams`.

use std::mem::{offset_of, size_of};

use naga::{TypeInner, valid};
use ssx_gpu::shaders;
use ssx_hdr::GpuParams;

fn parse(name: &str, src: &str) -> naga::Module {
    match naga::front::wgsl::parse_str(src) {
        Ok(m) => m,
        Err(e) => panic!("{name}: WGSL parse error:\n{}", e.emit_to_string(src)),
    }
}

#[test]
fn all_shaders_parse_and_validate() {
    for (name, src, entries) in shaders::ALL {
        let module = parse(name, src);
        // Baseline capabilities only: nothing the shaders use may need an optional feature.
        let mut v =
            valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::empty());
        if let Err(e) = v.validate(&module) {
            panic!("{name}: validation error: {}", e.emit_to_string(src));
        }
        for entry in *entries {
            assert!(
                module.entry_points.iter().any(|e| e.name == *entry),
                "{name}: missing entry point `{entry}`"
            );
        }
        assert!(
            module.entry_points.iter().all(|e| e.stage == naga::ShaderStage::Compute),
            "{name}: only compute entry points expected"
        );
    }
}

/// `(name, offset)` of every member and the total span of the WGSL struct `name`.
fn wgsl_struct(module: &naga::Module, name: &str) -> (Vec<(String, u32)>, u32) {
    for (_, ty) in module.types.iter() {
        if ty.name.as_deref() == Some(name)
            && let TypeInner::Struct { members, span } = &ty.inner
        {
            let m =
                members.iter().map(|m| (m.name.clone().unwrap_or_default(), m.offset)).collect();
            return (m, *span);
        }
    }
    panic!("struct {name} not found");
}

#[test]
fn layout_matches_ssx_hdr_gpu_params() {
    let expected: [(&str, usize); 9] = [
        ("scale", offset_of!(GpuParams, scale)),
        ("knee", offset_of!(GpuParams, knee)),
        ("peak", offset_of!(GpuParams, peak)),
        ("headroom", offset_of!(GpuParams, headroom)),
        ("mode", offset_of!(GpuParams, mode)),
        ("dither", offset_of!(GpuParams, dither)),
        ("width", offset_of!(GpuParams, width)),
        ("pad", offset_of!(GpuParams, pad)),
        ("c", offset_of!(GpuParams, c)),
    ];
    // Both the plain tonemap module and the fused tonemap+YUV module embed the struct.
    for (name, src) in
        [("tonemap", shaders::TONEMAP_WGSL), ("tonemap_to_yuv", shaders::TONEMAP_TO_YUV_WGSL)]
    {
        let module = parse(name, src);
        let (members, span) = wgsl_struct(&module, "Params");
        assert_eq!(span as usize, size_of::<GpuParams>(), "{name}: struct size");
        assert_eq!(members.len(), expected.len(), "{name}: member count");
        for ((wname, woff), (rname, roff)) in members.iter().zip(expected) {
            assert_eq!(wname, rname, "{name}: member order");
            assert_eq!(*woff as usize, roff, "{name}: offset of `{wname}`");
        }
    }
}

#[test]
fn gpu_params_has_the_documented_layout() {
    assert_eq!(size_of::<GpuParams>(), 48);
    assert_eq!(std::mem::align_of::<GpuParams>(), 16);
    assert_eq!(offset_of!(GpuParams, c), 32);
}
