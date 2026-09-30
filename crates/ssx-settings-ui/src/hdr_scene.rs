//! The synthetic HDR test scene behind the live preview on the *Capture & HDR* page, the
//! presets, and the readout that proves what the settings do to ordinary UI pixels.
//!
//! The scene is generated procedurally (no assets) as an `Rgba16F` / `ScRgbLinear` frame, the
//! format Windows hands the capture code when HDR is on. Colours are defined *relative to SDR
//! white* (1.0 = the white of a normal window) and scaled by `sdr_white_nits / 80` so that the
//! same scene can be previewed at different "SDR content brightness" levels.
//!
//! It contains what makes tone mapping go wrong:
//!
//! * a luminance **ramp** from black to 8x SDR white and a 9-step **wedge**,
//! * saturated **primaries** at 1x and again at 3x (hue shifts show up here),
//! * **skin tones** and **sky** at 1x and brighter (the two colours people notice first),
//! * a **fake window** made only of 8-bit sRGB colours at or below SDR white: the region the
//!   readout compares byte for byte against what an SDR screenshot would contain,
//! * a **sun** with a glow reaching 8x, sitting on the sky.
//!
//! [`render_all`] tone-maps the scene three ways (the settings being edited, the *Faithful*
//! default, and a hard clip) through `ssx_hdr::to_sdr8`, i.e. the very function the capture
//! path calls.

use half::f16;
use image::RgbaImage;
use ssx_core::settings::{HdrConfig, TonemapOperator};
use ssx_hdr::{HdrError, srgb_eotf, to_sdr8};
use ssx_services::tonemap_settings;
use ssx_types::{ColorSpace, Frame, PixelFormat, Size};

/// Scene width in pixels.
pub const SCENE_W: u32 = 480;
/// Scene height in pixels.
pub const SCENE_H: u32 = 270;

/// A pixel rectangle (`x1`/`y1` exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Left.
    pub x0: u32,
    /// Top.
    pub y0: u32,
    /// Right, exclusive.
    pub x1: u32,
    /// Bottom, exclusive.
    pub y1: u32,
}

impl Region {
    const fn new(x0: u32, y0: u32, x1: u32, y1: u32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// Number of pixels.
    pub const fn area(&self) -> usize {
        ((self.x1 - self.x0) * (self.y1 - self.y0)) as usize
    }

    /// Whether `(x, y)` lies inside.
    pub const fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// The continuous ramp (0 to 8x SDR white).
pub const RAMP: Region = Region::new(0, 0, SCENE_W, 30);
/// The 9-step wedge.
pub const WEDGE: Region = Region::new(0, 30, SCENE_W, 60);
/// The fake window that must stay byte-identical below the knee.
pub const UI_REGION: Region = Region::new(0, 196, 300, 270);
/// The brightest value of the ramp, in multiples of SDR white.
pub const RAMP_MAX: f32 = 8.0;

const WEDGE_STEPS: [f32; 9] = [0.0, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 4.0, 8.0];

/// The SDR-white levels offered in the UI (nits). Windows' slider runs 80 to 480.
pub const SDR_WHITE_CHOICES: [f32; 5] = [80.0, 120.0, 200.0, 300.0, 480.0];
/// The default level (a typical HDR desktop).
pub const DEFAULT_SDR_WHITE: f32 = 200.0;

/// The generated scene: the HDR frame and the 8-bit colours of the UI region.
#[derive(Debug, Clone)]
pub struct Scene {
    /// `Rgba16F`, scRGB linear (1.0 = 80 nits), `sdr_white_nits` set.
    pub frame: Frame,
    /// The 8-bit sRGB colour each pixel of [`UI_REGION`] was defined with (row-major).
    pub ui_reference: Vec<[u8; 3]>,
    /// The SDR-white level the scene was built for.
    pub sdr_white_nits: f32,
}

type Rgb = [f32; 3];

fn lin(c8: [u8; 3]) -> Rgb {
    c8.map(|v| srgb_eotf(f32::from(v) / 255.0))
}

fn scale(c: Rgb, m: f32) -> Rgb {
    c.map(|v| v * m)
}

fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

struct Canvas {
    px: Vec<Rgb>,
}

impl Canvas {
    fn new() -> Self {
        Self { px: vec![[0.0; 3]; (SCENE_W * SCENE_H) as usize] }
    }

    fn set(&mut self, x: u32, y: u32, c: Rgb) {
        if x < SCENE_W && y < SCENE_H {
            self.px[(y * SCENE_W + x) as usize] = c;
        }
    }

    fn fill(&mut self, r: Region, f: impl Fn(u32, u32) -> Rgb) {
        for y in r.y0..r.y1.min(SCENE_H) {
            for x in r.x0..r.x1.min(SCENE_W) {
                self.set(x, y, f(x, y));
            }
        }
    }

    fn rect(&mut self, r: Region, c: Rgb) {
        self.fill(r, |_, _| c);
    }
}

const PRIMARIES: [[u8; 3]; 6] =
    [[255, 0, 0], [255, 255, 0], [0, 255, 0], [0, 255, 255], [0, 0, 255], [255, 0, 255]];
const SKIN: [[u8; 3]; 3] = [[246, 214, 186], [198, 140, 100], [120, 76, 52]];
const SKY: [u8; 3] = [92, 164, 236];

/// Builds the scene for `sdr_white_nits` (values are clamped to 40..=2000).
pub fn build_scene(sdr_white_nits: f32) -> Scene {
    let nits = if sdr_white_nits.is_finite() { sdr_white_nits.clamp(40.0, 2000.0) } else { 200.0 };
    let mut c = Canvas::new();
    // Background: a dark neutral so patches stand out.
    c.rect(Region::new(0, 0, SCENE_W, SCENE_H), lin([24, 25, 28]));

    // Ramp and wedge (grey, relative to SDR white).
    c.fill(RAMP, |x, _| {
        let m = RAMP_MAX * x as f32 / (SCENE_W - 1) as f32;
        [m, m, m]
    });
    let step_w = SCENE_W as f32 / WEDGE_STEPS.len() as f32;
    c.fill(WEDGE, |x, _| {
        let i = ((x as f32 / step_w) as usize).min(WEDGE_STEPS.len() - 1);
        let m = WEDGE_STEPS[i];
        [m, m, m]
    });

    // Primaries at 1x (top) and 3x (bottom).
    for (i, p) in PRIMARIES.iter().enumerate() {
        let x0 = 4 + i as u32 * 79;
        c.rect(Region::new(x0, 66, x0 + 75, 104), lin(*p));
        c.rect(Region::new(x0, 104, x0 + 75, 142), scale(lin(*p), 3.0));
    }

    // Skin tones and sky: eight patches.
    let patch = |i: u32| {
        let x0 = 4 + i * 60;
        Region::new(x0, 150, x0 + 56, 190)
    };
    for (i, s) in SKIN.iter().enumerate() {
        c.rect(patch(i as u32), lin(*s));
    }
    c.rect(patch(3), scale(lin(SKIN[0]), 2.2));
    c.rect(patch(4), lin(SKY));
    c.rect(patch(5), scale(lin(SKY), 2.0));
    c.rect(patch(6), scale(lin(SKY), 4.0));
    let g = patch(7);
    c.fill(g, |x, _| {
        let t = (x - g.x0) as f32 / (g.x1 - g.x0 - 1) as f32;
        scale(lin(SKY), 1.0 + 5.0 * t)
    });

    // The fake window, entirely in 8-bit sRGB colours at or below SDR white.
    let mut ui_reference = vec![[0u8; 3]; UI_REGION.area()];
    let mut ui_px = |x: u32, y: u32, c8: [u8; 3], canvas: &mut Canvas| {
        canvas.set(x, y, lin(c8));
        if UI_REGION.contains(x, y) {
            let i =
                ((y - UI_REGION.y0) * (UI_REGION.x1 - UI_REGION.x0) + (x - UI_REGION.x0)) as usize;
            ui_reference[i] = c8;
        }
    };
    let mut ui_rect = |r: Region, c8: [u8; 3], canvas: &mut Canvas| {
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                ui_px(x, y, c8, canvas);
            }
        }
    };
    ui_rect(UI_REGION, [255, 255, 255], &mut c); // window body: pure SDR white
    ui_rect(Region::new(0, 196, 300, 212), [226, 228, 234], &mut c); // title bar
    ui_rect(Region::new(0, 212, 64, 270), [240, 242, 246], &mut c); // sidebar
    for (i, w) in [150u32, 120, 170, 96].iter().enumerate() {
        let y = 224 + i as u32 * 10;
        ui_rect(Region::new(78, y, 78 + w, y + 4), [58, 60, 66], &mut c); // text lines
    }
    ui_rect(Region::new(78, 254, 138, 266), [0, 120, 212], &mut c); // primary button
    ui_rect(Region::new(146, 254, 206, 266), [225, 225, 230], &mut c); // secondary button
    ui_rect(Region::new(10, 220, 54, 226), [96, 100, 110], &mut c); // sidebar items
    ui_rect(Region::new(10, 234, 54, 240), [96, 100, 110], &mut c);
    ui_rect(Region::new(10, 248, 54, 254), [0, 120, 212], &mut c);

    // A sun (up to 8x) with a glow on the sky.
    let sky_bg = scale(lin(SKY), 0.55);
    let (cx, cy) = (392.0_f32, 233.0_f32);
    c.fill(Region::new(300, 196, SCENE_W, SCENE_H), |x, y| {
        let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
        let glow = (1.0 - d / 70.0).clamp(0.0, 1.0);
        let base = lerp(sky_bg, scale(lin([255, 244, 214]), 1.0), glow.powi(2));
        if d < 22.0 {
            let t = (1.0 - d / 22.0).clamp(0.0, 1.0);
            scale(lin([255, 246, 220]), 1.0 + (RAMP_MAX - 1.0) * t.sqrt())
        } else {
            base
        }
    });

    // To scRGB half floats.
    let k = nits / 80.0;
    let mut data = Vec::with_capacity(c.px.len() * 8);
    for p in &c.px {
        for v in [p[0] * k, p[1] * k, p[2] * k, 1.0] {
            data.extend_from_slice(&f16::from_f32(v).to_le_bytes());
        }
    }
    let mut frame = Frame::from_raw(
        Size::new(SCENE_W, SCENE_H),
        SCENE_W as usize * 8,
        PixelFormat::Rgba16F,
        ColorSpace::ScRgbLinear,
        data,
    )
    .unwrap_or_else(|_| Frame::new(Size::new(1, 1), PixelFormat::Rgba16F, ColorSpace::ScRgbLinear));
    frame.sdr_white_nits = Some(nits);
    Scene { frame, ui_reference, sdr_white_nits: nits }
}

/// Tone-maps the scene with `cfg` through `ssx_hdr::to_sdr8`.
pub fn render(scene: &Scene, cfg: &HdrConfig) -> Result<RgbaImage, HdrError> {
    let out = to_sdr8(&scene.frame, &tonemap_settings(cfg))?;
    out.to_image().map_err(HdrError::Frame)
}

/// What a legacy capture gives: every channel clipped at SDR white, no roll-off, no dither.
pub fn clip_config() -> HdrConfig {
    HdrConfig {
        operator: TonemapOperator::Clip,
        peak: 4.0,
        knee: 1.0,
        dither: false,
        exposure: 0.0,
    }
}

/// The named tone-mapping presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HdrPreset {
    /// The default: SDR content is byte-identical to an SDR screenshot.
    Faithful,
    /// Rolls off from 80 % of SDR white to keep highlight detail; UI white dims a little.
    PreserveHighlights,
    /// Anything else.
    Custom,
}

impl HdrPreset {
    /// The presets in display order.
    pub const ALL: [HdrPreset; 3] =
        [HdrPreset::Faithful, HdrPreset::PreserveHighlights, HdrPreset::Custom];

    /// Name shown on the button.
    pub const fn label(self) -> &'static str {
        match self {
            HdrPreset::Faithful => "Faithful",
            HdrPreset::PreserveHighlights => "Preserve highlights",
            HdrPreset::Custom => "Custom",
        }
    }

    /// One sentence about the trade-off.
    pub const fn blurb(self) -> &'static str {
        match self {
            HdrPreset::Faithful => {
                "Ordinary windows come out exactly like an SDR screenshot. Only content brighter than SDR white is compressed."
            }
            HdrPreset::PreserveHighlights => {
                "Starts compressing at 80 % of SDR white, so bright skies and lamps keep detail. White UI ends up slightly grey."
            }
            HdrPreset::Custom => "Your own operator, peak, knee, exposure and dither.",
        }
    }

    /// The settings this preset stands for (`None` for [`HdrPreset::Custom`]).
    pub fn config(self) -> Option<HdrConfig> {
        match self {
            HdrPreset::Faithful => Some(HdrConfig::faithful()),
            HdrPreset::PreserveHighlights => Some(HdrConfig::preserve_highlights()),
            HdrPreset::Custom => None,
        }
    }

    /// Which preset `cfg` is (within a small tolerance), else `Custom`.
    pub fn classify(cfg: &HdrConfig) -> HdrPreset {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        for p in [HdrPreset::Faithful, HdrPreset::PreserveHighlights] {
            if let Some(c) = p.config()
                && c.operator == cfg.operator
                && c.dither == cfg.dither
                && close(c.peak, cfg.peak)
                && close(c.knee, cfg.knee)
                && close(c.exposure, cfg.exposure)
            {
                return p;
            }
        }
        HdrPreset::Custom
    }
}

/// Human name of an operator.
pub const fn operator_label(op: TonemapOperator) -> &'static str {
    match op {
        TonemapOperator::Clip => "Clip",
        TonemapOperator::ReinhardExtended => "Reinhard (extended)",
        TonemapOperator::Bt2390 => "BT.2390",
        TonemapOperator::AcesFit => "ACES (fit)",
    }
}

/// One sentence about an operator.
pub const fn operator_blurb(op: TonemapOperator) -> &'static str {
    match op {
        TonemapOperator::Clip => "Hard clip at SDR white: the legacy look, highlights blow out.",
        TonemapOperator::ReinhardExtended => {
            "Smooth roll-off with a white point at the peak. The default."
        }
        TonemapOperator::Bt2390 => "The broadcast standard's spline shoulder.",
        TonemapOperator::AcesFit => "Filmic curve; more contrast in the highlights.",
    }
}

/// What the numbers say about one rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct Readout {
    /// Pixels in the fake window.
    pub ui_pixels: usize,
    /// How many are byte-identical to the 8-bit colours they were defined with.
    pub ui_identical: usize,
    /// Largest per-channel difference in the window.
    pub ui_max_deviation: u8,
    /// What pure SDR white (255, 255, 255) became.
    pub white_out: [u8; 3],
    /// Distinct output levels across the part of the ramp brighter than SDR white.
    pub highlight_levels: usize,
    /// Share (0-1) of that part of the ramp that is pure white.
    pub highlight_clipped: f32,
}

impl Readout {
    /// Measures `out` (a rendering of `scene`).
    pub fn measure(scene: &Scene, out: &RgbaImage) -> Readout {
        let mut identical = 0;
        let mut max_dev = 0u8;
        let mut white_out = [255u8; 3];
        let w = UI_REGION.x1 - UI_REGION.x0;
        for y in UI_REGION.y0..UI_REGION.y1 {
            for x in UI_REGION.x0..UI_REGION.x1 {
                let want =
                    scene.ui_reference[((y - UI_REGION.y0) * w + (x - UI_REGION.x0)) as usize];
                let got = out.get_pixel(x, y).0;
                let dev = (0..3).map(|i| want[i].abs_diff(got[i])).max().unwrap_or(0);
                max_dev = max_dev.max(dev);
                if dev == 0 {
                    identical += 1;
                }
                if want == [255, 255, 255] {
                    white_out = [got[0], got[1], got[2]];
                }
            }
        }
        let first_hot = ((SCENE_W - 1) as f32 / RAMP_MAX).ceil() as u32 + 1;
        let mut levels = std::collections::BTreeSet::new();
        let mut clipped = 0;
        let mut total = 0;
        for x in first_hot..SCENE_W {
            let p = out.get_pixel(x, RAMP.y0 + 10).0;
            levels.insert([p[0], p[1], p[2]]);
            total += 1;
            if p[0] >= 254 && p[1] >= 254 && p[2] >= 254 {
                clipped += 1;
            }
        }
        Readout {
            ui_pixels: UI_REGION.area(),
            ui_identical: identical,
            ui_max_deviation: max_dev,
            white_out,
            highlight_levels: levels.len(),
            highlight_clipped: if total == 0 { 0.0 } else { clipped as f32 / total as f32 },
        }
    }

    /// `true` if every pixel of the window is untouched.
    pub fn ui_untouched(&self) -> bool {
        self.ui_identical == self.ui_pixels
    }

    /// How much darker UI white became, in percent of full scale (0 when untouched).
    pub fn white_dimming_percent(&self) -> f32 {
        let out = f32::from(self.white_out.iter().copied().min().unwrap_or(255));
        ((255.0 - out) / 255.0 * 100.0).max(0.0)
    }

    /// The headline sentence for the readout.
    pub fn headline(&self) -> String {
        if self.ui_untouched() {
            format!(
                "UI stays byte-identical: white is still 255 ({} of {} pixels unchanged)",
                self.ui_identical, self.ui_pixels
            )
        } else if self.white_out.iter().all(|c| *c == self.white_out[0]) {
            format!(
                "UI white becomes {} ({:.1} % darker); {} of {} pixels changed, by at most {} levels",
                self.white_out[0],
                self.white_dimming_percent(),
                self.ui_pixels - self.ui_identical,
                self.ui_pixels,
                self.ui_max_deviation
            )
        } else {
            format!(
                "UI white becomes {:?} ({:.1} % darker); {} of {} pixels changed",
                self.white_out,
                self.white_dimming_percent(),
                self.ui_pixels - self.ui_identical,
                self.ui_pixels
            )
        }
    }
}

/// The three renderings the page shows side by side.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// The settings being edited.
    pub current: RgbaImage,
    /// The Faithful preset.
    pub faithful: RgbaImage,
    /// A hard clip.
    pub clipped: RgbaImage,
    /// Numbers for each of the above.
    pub readouts: [Readout; 3],
}

/// Renders `scene` with `cfg`, the faithful preset and a clip.
pub fn render_all(scene: &Scene, cfg: &HdrConfig) -> Result<Rendered, HdrError> {
    let current = render(scene, cfg)?;
    let faithful = render(scene, &HdrConfig::faithful())?;
    let clipped = render(scene, &clip_config())?;
    let readouts = [
        Readout::measure(scene, &current),
        Readout::measure(scene, &faithful),
        Readout::measure(scene, &clipped),
    ];
    Ok(Rendered { current, faithful, clipped, readouts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_is_a_valid_hdr_frame() {
        let s = build_scene(200.0);
        assert_eq!(s.frame.format(), PixelFormat::Rgba16F);
        assert_eq!(s.frame.color_space(), ColorSpace::ScRgbLinear);
        assert_eq!(s.frame.sdr_white_nits, Some(200.0));
        assert_eq!(s.frame.width(), SCENE_W);
        assert_eq!(s.frame.height(), SCENE_H);
        assert_eq!(s.ui_reference.len(), UI_REGION.area());
        assert!(UI_REGION.x1 <= SCENE_W && UI_REGION.y1 <= SCENE_H);
    }

    fn pixel(s: &Scene, x: u32, y: u32) -> [f32; 3] {
        let row = s.frame.row(y);
        let at = |c: usize| {
            let i = (x as usize * 4 + c) * 2;
            f16::from_le_bytes([row[i], row[i + 1]]).to_f32()
        };
        [at(0), at(1), at(2)]
    }

    #[test]
    fn ramp_reaches_eight_times_sdr_white_in_scrgb() {
        let s = build_scene(200.0);
        let last = pixel(&s, SCENE_W - 1, 5);
        // 8x SDR white at 200 nits = 8 * 2.5 in scRGB (1.0 = 80 nits)
        assert!((last[0] - 20.0).abs() < 0.05, "{last:?}");
        let first = pixel(&s, 0, 5);
        assert!(first[0].abs() < 1e-3);
        // SDR white in the window is exactly 200 / 80
        let w = pixel(&s, 250, 205 + 40);
        assert!((w[0] - 2.5).abs() < 0.002 && (w[1] - 2.5).abs() < 0.002, "{w:?}");
    }

    #[test]
    fn scene_scales_with_the_sdr_white_level() {
        let a = pixel(&build_scene(80.0), 250, 245);
        let b = pixel(&build_scene(480.0), 250, 245);
        assert!((a[0] - 1.0).abs() < 1e-3);
        assert!((b[0] - 6.0).abs() < 0.01);
    }

    #[test]
    fn non_finite_levels_are_sanitised() {
        for n in [f32::NAN, f32::INFINITY, -5.0, 0.0, 1e9] {
            let s = build_scene(n);
            let v = s.frame.sdr_white_nits.unwrap();
            assert!(v.is_finite() && (40.0..=2000.0).contains(&v), "{n} -> {v}");
        }
    }

    #[test]
    fn faithful_keeps_the_ui_byte_identical_at_every_sdr_white_level() {
        for nits in SDR_WHITE_CHOICES {
            let s = build_scene(nits);
            let img = render(&s, &HdrConfig::faithful()).unwrap();
            let r = Readout::measure(&s, &img);
            assert!(r.ui_untouched(), "{nits} nits: {}", r.headline());
            assert_eq!(r.white_out, [255, 255, 255]);
            assert_eq!(r.ui_max_deviation, 0);
            assert_eq!(r.white_dimming_percent(), 0.0);
            assert!(r.headline().contains("byte-identical"));
        }
    }

    #[test]
    fn every_operator_keeps_the_ui_identical_when_the_knee_is_one() {
        let s = build_scene(200.0);
        for op in [
            TonemapOperator::Clip,
            TonemapOperator::ReinhardExtended,
            TonemapOperator::Bt2390,
            TonemapOperator::AcesFit,
        ] {
            for dither in [false, true] {
                let cfg = HdrConfig { operator: op, dither, ..HdrConfig::faithful() };
                let r = Readout::measure(&s, &render(&s, &cfg).unwrap());
                assert!(r.ui_untouched(), "{op:?} dither={dither}: {}", r.headline());
            }
        }
    }

    #[test]
    fn a_lower_knee_dims_ui_white_and_says_by_how_much() {
        let s = build_scene(200.0);
        let cfg = HdrConfig::preserve_highlights();
        let r = Readout::measure(&s, &render(&s, &cfg).unwrap());
        assert!(!r.ui_untouched());
        assert!(r.white_out[0] < 255, "{r:?}");
        assert!(r.white_dimming_percent() > 0.0 && r.white_dimming_percent() < 25.0, "{r:?}");
        assert!(r.headline().contains("darker"), "{}", r.headline());
        // the darker the knee, the more it dims
        let lower = HdrConfig { knee: 0.5, ..cfg };
        let r2 = Readout::measure(&s, &render(&s, &lower).unwrap());
        assert!(r2.white_dimming_percent() > r.white_dimming_percent());
    }

    #[test]
    fn preserve_highlights_keeps_more_detail_above_white_than_faithful_and_clip() {
        let s = build_scene(200.0);
        let rendered = render_all(&s, &HdrConfig::preserve_highlights()).unwrap();
        let [current, faithful, clipped] = rendered.readouts;
        assert!(current.highlight_levels > faithful.highlight_levels, "{current:?} {faithful:?}");
        assert!(current.highlight_clipped < clipped.highlight_clipped, "{current:?} {clipped:?}");
        assert!((clipped.highlight_clipped - 1.0).abs() < 1e-6, "clip blows everything out");
        assert!(clipped.ui_untouched());
    }

    #[test]
    fn render_all_uses_the_given_settings_for_the_first_image_only() {
        let s = build_scene(200.0);
        let all = render_all(&s, &HdrConfig::faithful()).unwrap();
        assert_eq!(all.current, all.faithful);
        assert_ne!(all.faithful, all.clipped, "clip differs from the roll-off in the highlights");
        let all2 = render_all(&s, &HdrConfig { exposure: -2.0, ..HdrConfig::faithful() }).unwrap();
        assert_ne!(all2.current, all2.faithful);
        assert_eq!(all2.faithful, all.faithful);
    }

    #[test]
    fn presets_classify_and_round_trip() {
        assert_eq!(HdrPreset::classify(&HdrConfig::default()), HdrPreset::Faithful);
        assert_eq!(
            HdrPreset::classify(&HdrConfig::preserve_highlights()),
            HdrPreset::PreserveHighlights
        );
        for p in [HdrPreset::Faithful, HdrPreset::PreserveHighlights] {
            assert_eq!(HdrPreset::classify(&p.config().unwrap()), p);
        }
        assert!(HdrPreset::Custom.config().is_none());
        let tweaked = HdrConfig { peak: 6.0, ..HdrConfig::faithful() };
        assert_eq!(HdrPreset::classify(&tweaked), HdrPreset::Custom);
        let tweaked = HdrConfig { dither: false, ..HdrConfig::faithful() };
        assert_eq!(HdrPreset::classify(&tweaked), HdrPreset::Custom);
        let op = HdrConfig { operator: TonemapOperator::AcesFit, ..HdrConfig::faithful() };
        assert_eq!(HdrPreset::classify(&op), HdrPreset::Custom);
    }

    #[test]
    fn preset_texts_exist() {
        for p in HdrPreset::ALL {
            assert!(!p.label().is_empty() && p.blurb().len() > 20);
        }
        for op in [TonemapOperator::Clip, TonemapOperator::Bt2390] {
            assert!(!operator_label(op).is_empty() && !operator_blurb(op).is_empty());
        }
    }

    #[test]
    fn extreme_but_valid_settings_render_without_error() {
        let s = build_scene(480.0);
        for cfg in [
            HdrConfig { peak: 1.0, knee: 0.0, exposure: -10.0, ..HdrConfig::faithful() },
            HdrConfig { peak: 100.0, knee: 1.0, exposure: 10.0, ..HdrConfig::faithful() },
        ] {
            let img = render(&s, &cfg).unwrap();
            assert_eq!((img.width(), img.height()), (SCENE_W, SCENE_H));
        }
    }
}
