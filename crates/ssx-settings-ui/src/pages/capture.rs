//! Capture & HDR: cursor, delay, HDR presets and a live preview of what they do to a
//! synthetic HDR scene.

use std::{sync::Arc, time::Instant};

use egui::{Color32, ColorImage, RichText, TextureHandle, TextureOptions, Ui, vec2};
use image::RgbaImage;
use ssx_core::settings::{HdrConfig, TonemapOperator};
use ssx_editor_ui::ui::theme;

use super::Cx;
use crate::{
    hdr_scene::{
        DEFAULT_SDR_WHITE, HdrPreset, Readout, SDR_WHITE_CHOICES, SCENE_H, SCENE_W, operator_blurb,
        operator_label,
    },
    preview_engine::{DEFAULT_DELAY, PreviewEngine, PreviewParams, PreviewResult},
    task::Waker,
    ui_kit::{self, Field},
};

/// State of the Capture & HDR page.
pub struct State {
    engine: PreviewEngine,
    /// The SDR-white level of the preview, in nits.
    pub sdr_white: f32,
    /// The user picked "Custom" (the classification alone cannot remember that).
    pub force_custom: bool,
    shown: Option<Arc<PreviewResult>>,
    textures: Option<[TextureHandle; 3]>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("sdr_white", &self.sdr_white)
            .field("force_custom", &self.force_custom)
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Starts the preview worker; `wake` repaints the window when a preview is ready.
    pub fn new(wake: &Waker) -> Self {
        let w = wake.clone();
        Self {
            engine: PreviewEngine::new(DEFAULT_DELAY, move || w()),
            sdr_white: DEFAULT_SDR_WHITE,
            force_custom: false,
            shown: None,
            textures: None,
        }
    }

    /// The preset to highlight for `cfg`.
    pub fn preset(&self, cfg: &HdrConfig) -> HdrPreset {
        if self.force_custom { HdrPreset::Custom } else { HdrPreset::classify(cfg) }
    }

    /// Applies a preset to the settings. `Custom` keeps the current values.
    pub fn choose(&mut self, cfg: &mut HdrConfig, preset: HdrPreset) {
        match preset.config() {
            Some(c) => {
                *cfg = c;
                self.force_custom = false;
            }
            None => self.force_custom = true,
        }
    }

    /// The preview worker (tests wait on it).
    pub fn engine(&mut self) -> &mut PreviewEngine {
        &mut self.engine
    }

    /// The newest finished preview.
    pub fn latest(&self) -> Option<&Arc<PreviewResult>> {
        self.engine.latest()
    }
}

/// Whether a config is one the tone mapper accepts (the preview skips invalid ones; the
/// validator explains them).
pub fn config_is_valid(c: &HdrConfig) -> bool {
    c.peak.is_finite()
        && (1.0..=100.0).contains(&c.peak)
        && c.knee.is_finite()
        && (0.0..=1.0).contains(&c.knee)
        && c.exposure.is_finite()
        && (-10.0..=10.0).contains(&c.exposure)
}

fn to_color_image(img: &RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::page_scroll(ui, "capture", |ui| {
        capture_card(ui, cx);
        hdr_card(ui, st, cx);
        preview_card(ui, st, cx);
    });
}

fn capture_card(ui: &mut Ui, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Capture"), |ui| {
        Field::new("Mouse cursor").show(ui, |ui| {
            ui_kit::switch(ui, "Include the cursor in screenshots", &mut cx.settings.capture.show_cursor);
        });
        ui.add_space(4.0);
        Field::new("Delay").issues(cx.issues, "capture.delay_ms")
            .help("Wait this long before capturing, so menus and tooltips can be opened first (at most one minute).")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ssx_editor_ui::ui::widgets::input_style(ui);
                    let mut ms = cx.settings.capture.delay_ms;
                    let r = ui.add(egui::DragValue::new(&mut ms).range(0..=60_000).clamp_existing_to_range(false).suffix(" ms").speed(10.0));
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::DragValue, true, "Delay"));
                    if r.changed() {
                        cx.settings.capture.delay_ms = ms;
                    }
                    for (label, v) in [("1 s", 1000), ("3 s", 3000), ("5 s", 5000)] {
                        if ui_kit::chip(ui, label, cx.settings.capture.delay_ms == v).clicked() {
                            cx.settings.capture.delay_ms = v;
                        }
                    }
                });
            });
    });
}

fn hdr_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("HDR screens"), |ui| {
        ui_kit::hint(ui, "When Windows HDR is on, the screen is brighter than an ordinary picture can be. These settings decide how it is squeezed into a normal (SDR) screenshot. They do nothing on screens without HDR.");
        ui.add_space(8.0);
        let cfg = &mut cx.settings.capture.hdr;
        let current = st.preset(cfg);
        Field::new("Preset").help(current.blurb()).show(ui, |ui| {
            let options: Vec<(HdrPreset, &str)> = HdrPreset::ALL.iter().map(|p| (*p, p.label())).collect();
            if let Some(p) = ui_kit::segmented_row(ui, current, &options) {
                st.choose(cfg, p);
            }
        });
        ui.add_space(6.0);
        ui_kit::divider(ui);
        Field::new("Operator").help(operator_blurb(cfg.operator)).show(ui, |ui| {
            let options = [
                (TonemapOperator::ReinhardExtended, operator_label(TonemapOperator::ReinhardExtended)),
                (TonemapOperator::Bt2390, operator_label(TonemapOperator::Bt2390)),
                (TonemapOperator::AcesFit, operator_label(TonemapOperator::AcesFit)),
                (TonemapOperator::Clip, operator_label(TonemapOperator::Clip)),
            ];
            if let Some(o) = ui_kit::segmented_row(ui, cfg.operator, &options) {
                cfg.operator = o;
            }
        });
        ui.add_space(4.0);
        Field::new("Peak").issues(cx.issues, "capture.hdr.peak")
            .help("The brightest content to keep, as a multiple of SDR white. Anything brighter is clipped after the roll-off.")
            .show(ui, |ui| {
                slider(ui, "Peak", &mut cfg.peak, 1.0..=100.0, "x SDR white", true);
            });
        Field::new("Knee").issues(cx.issues, "capture.hdr.knee")
            .help("Where the roll-off starts, as a fraction of SDR white. 1.0 leaves everything at or below SDR white exactly as it is; lower values keep more highlight detail but make white slightly grey.")
            .show(ui, |ui| {
                slider(ui, "Knee", &mut cfg.knee, 0.0..=1.0, "", false);
            });
        Field::new("Exposure").issues(cx.issues, "capture.hdr.exposure")
            .help("Brightens or darkens everything before the conversion, in stops (EV). 0 changes nothing.")
            .show(ui, |ui| {
                slider(ui, "Exposure", &mut cfg.exposure, -10.0..=10.0, " EV", false);
            });
        Field::new("Dither").help("Adds a whisper of noise before rounding to 8 bit, so smooth gradients do not band. Solid colours are never touched.")
            .show(ui, |ui| {
                ui_kit::switch(ui, "Dither", &mut cfg.dither);
            });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui_kit::button(ui, "Reset to Faithful").clicked() {
                st.choose(cfg, HdrPreset::Faithful);
            }
        });
    });
}

fn slider(ui: &mut Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str, log: bool) {
    ssx_editor_ui::ui::widgets::input_style(ui);
    let mut s = egui::Slider::new(v, range)
        .trailing_fill(true)
        .max_decimals(2)
        .suffix(suffix.to_owned())
        // A value that is out of range (from a hand-edited file) must be shown and reported,
        // not silently pulled into range just because the page was opened.
        .clamping(egui::SliderClamping::Edits);
    if log {
        s = s.logarithmic(true);
    }
    let r = ui.add(s);
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Slider, true, label));
}

fn preview_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Live preview"), |ui| {
        ui_kit::hint(ui, "A made-up HDR picture (a brightness ramp to 8x SDR white, colours, skin, sky, a sun and a window made only of ordinary colours) run through the same conversion the capture uses.");
        ui.add_space(6.0);
        Field::new("SDR white level").help("The \"SDR content brightness\" slider of Windows, in nits. Ordinary white sits here.").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for n in SDR_WHITE_CHOICES {
                    if ui_kit::chip(ui, &format!("{n:.0} nits"), (st.sdr_white - n).abs() < 0.5).clicked() {
                        st.sdr_white = n;
                    }
                }
            });
        });
        ui.add_space(8.0);

        // Ask for the preview of the current settings (debounced, off-thread).
        let cfg = cx.settings.capture.hdr;
        let valid = config_is_valid(&cfg);
        if valid {
            st.engine.request(PreviewParams { config: cfg, sdr_white_nits: st.sdr_white }, Instant::now());
        }
        if let Some(wait) = st.engine.tick(Instant::now()) {
            ui.ctx().request_repaint_after(wait);
        }
        if let Some(latest) = st.engine.latest().cloned()
            && st.shown.as_ref().is_none_or(|s| !Arc::ptr_eq(s, &latest))
        {
            if let Ok(r) = &latest.rendered {
                let opts = TextureOptions::LINEAR;
                st.textures = Some([
                    ui.ctx().load_texture("hdr-current", to_color_image(&r.current), opts),
                    ui.ctx().load_texture("hdr-faithful", to_color_image(&r.faithful), opts),
                    ui.ctx().load_texture("hdr-clipped", to_color_image(&r.clipped), opts),
                ]);
            }
            st.shown = Some(latest);
        }

        if !valid {
            ui.horizontal_top(|ui| {
                ui_kit::severity_icon(ui, ssx_core::settings::Severity::Error);
                ui.label(RichText::new("The preview is paused until the values above are in range.").color(ui_kit::ERROR_TEXT));
            });
        }
        let Some(shown) = st.shown.clone() else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Rendering the preview...");
            });
            return;
        };
        if let Err(e) = &shown.rendered {
            ui.label(RichText::new(format!("The preview failed: {e}")).color(ui_kit::ERROR_TEXT));
            return;
        }
        let (Some(tex), Ok(rendered)) = (&st.textures, &shown.rendered) else { return };
        let titles = [
            ("Your settings", "the settings on this page"),
            ("Faithful (default)", "knee 1.0: SDR content untouched"),
            ("Clipped (legacy capture)", "what a plain SDR capture gives"),
        ];
        let gap = 10.0;
        let w = ((ui.available_width() - gap * 2.0) / 3.0).floor();
        let h = (w * SCENE_H as f32 / SCENE_W as f32).round();
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for i in 0..3 {
                ui.vertical(|ui| {
                    ui.set_width(w);
                    ui.label(RichText::new(titles[i].0).strong().color(Color32::WHITE));
                    ui_kit::hint(ui, titles[i].1);
                    let r = ui.add(egui::Image::new((tex[i].id(), vec2(w, h))).corner_radius(4.0));
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Image, true, titles[i].0));
                    readout(ui, &rendered.readouts[i]);
                });
            }
        });
        ui.add_space(8.0);
        let r = &rendered.readouts[0];
        ui.horizontal_top(|ui| {
            let (ok, color) = if r.ui_untouched() { ("\u{2714}", ui_kit::OK_TEXT) } else { ("\u{25CF}", ui_kit::WARN_TEXT) };
            ui.label(RichText::new(ok).color(color));
            ui.add(egui::Label::new(RichText::new(format!("Your settings: {}", r.headline())).color(theme::TEXT)).wrap());
        });
        ui_kit::hint(ui, &format!(
            "Rendered in {} ms at {:.0} nits SDR white.",
            shown.took.as_millis(),
            shown.params.sdr_white_nits
        ));
    });
}

fn readout(ui: &mut Ui, r: &Readout) {
    ui.add_space(4.0);
    let (text, color) = if r.ui_untouched() {
        ("UI white: 255, untouched".to_owned(), ui_kit::OK_TEXT)
    } else {
        (format!("UI white: {} ({:.1} % darker)", r.white_out[0], r.white_dimming_percent()), ui_kit::WARN_TEXT)
    };
    ui.label(RichText::new(text).size(12.0).color(color));
    ui.label(
        RichText::new(format!(
            "Above SDR white: {} levels, {:.0} % pure white",
            r.highlight_levels,
            r.highlight_clipped * 100.0
        ))
        .size(12.0)
        .color(theme::TEXT_DIM),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::no_wake;

    #[test]
    fn choosing_presets_sets_the_values_and_custom_keeps_them() {
        let mut st = State::new(&no_wake());
        let mut cfg = HdrConfig::faithful();
        assert_eq!(st.preset(&cfg), HdrPreset::Faithful);
        st.choose(&mut cfg, HdrPreset::PreserveHighlights);
        assert_eq!(cfg, HdrConfig::preserve_highlights());
        assert_eq!(st.preset(&cfg), HdrPreset::PreserveHighlights);
        st.choose(&mut cfg, HdrPreset::Custom);
        assert_eq!(cfg, HdrConfig::preserve_highlights(), "Custom does not change values");
        assert_eq!(st.preset(&cfg), HdrPreset::Custom, "but it is remembered as the choice");
        st.choose(&mut cfg, HdrPreset::Faithful);
        assert_eq!(cfg, HdrConfig::faithful());
        assert!(!st.force_custom);
    }

    #[test]
    fn config_validity_matches_the_validators_ranges() {
        assert!(config_is_valid(&HdrConfig::faithful()));
        for bad in [
            HdrConfig { peak: 0.5, ..HdrConfig::faithful() },
            HdrConfig { peak: 101.0, ..HdrConfig::faithful() },
            HdrConfig { knee: 1.5, ..HdrConfig::faithful() },
            HdrConfig { knee: -0.1, ..HdrConfig::faithful() },
            HdrConfig { exposure: 11.0, ..HdrConfig::faithful() },
            HdrConfig { exposure: f32::NAN, ..HdrConfig::faithful() },
        ] {
            assert!(!config_is_valid(&bad), "{bad:?}");
            let mut s = ssx_core::settings::Settings::default();
            s.capture.hdr = bad;
            assert!(s.validate().iter().any(|i| i.severity == ssx_core::settings::Severity::Error), "{bad:?}");
        }
    }

    #[test]
    fn invalid_and_valid_agree_with_core_over_a_grid() {
        for peak in [0.0f32, 0.99, 1.0, 4.0, 100.0, 100.5] {
            for knee in [-0.5f32, 0.0, 0.5, 1.0, 1.01] {
                for exposure in [-10.5f32, -10.0, 0.0, 10.0, 10.5] {
                    let c = HdrConfig { peak, knee, exposure, ..HdrConfig::faithful() };
                    let mut s = ssx_core::settings::Settings::default();
                    s.capture.hdr = c;
                    let core_ok = !s.validate().iter().any(|i| i.path.starts_with("capture.hdr"));
                    assert_eq!(config_is_valid(&c), core_ok, "{c:?}");
                }
            }
        }
    }
}
