//! About: version, licence and third-party notices.

use egui::{Color32, RichText, Ui};
use ssx_editor_ui::ui::theme;

use super::Cx;
use crate::ui_kit;

/// The licence of ssx itself.
pub const LICENSE: &str = "GPL-3.0-or-later";
/// The project's repository.
pub const REPOSITORY: &str = "https://github.com/kielerdotdev/ssx";

/// A third-party component and its licence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notice {
    /// The crate or program.
    pub name: &'static str,
    /// What ssx uses it for.
    pub purpose: &'static str,
    /// Its licence (SPDX).
    pub license: &'static str,
}

/// The main third-party components. A complete, generated list (`cargo about`) ships with the
/// installers; this is the part worth reading.
pub const NOTICES: &[Notice] = &[
    Notice { name: "egui / eframe", purpose: "this window", license: "MIT OR Apache-2.0" },
    Notice { name: "wgpu", purpose: "GPU rendering", license: "MIT OR Apache-2.0" },
    Notice { name: "image", purpose: "PNG, JPEG, WebP, GIF, BMP", license: "MIT OR Apache-2.0" },
    Notice { name: "SQLite (rusqlite)", purpose: "the history database", license: "MIT; SQLite is public domain" },
    Notice { name: "tokio", purpose: "network uploads", license: "MIT" },
    Notice { name: "reqwest / rustls / ring", purpose: "HTTPS", license: "MIT OR Apache-2.0; ISC" },
    Notice { name: "keyring", purpose: "the credential store", license: "MIT OR Apache-2.0" },
    Notice { name: "arboard", purpose: "the clipboard", license: "MIT OR Apache-2.0" },
    Notice { name: "rfd", purpose: "file dialogs", license: "MIT" },
    Notice { name: "global-hotkey", purpose: "global shortcuts", license: "MIT OR Apache-2.0" },
    Notice { name: "ashpd / zbus", purpose: "desktop portals", license: "MIT" },
    Notice { name: "notify-rust", purpose: "notifications", license: "MIT OR Apache-2.0" },
    Notice { name: "zip", purpose: "zipping folders", license: "MIT" },
    Notice { name: "half, rayon, serde, toml, chrono", purpose: "core libraries", license: "MIT OR Apache-2.0" },
];

/// Draws the page.
pub fn ui(ui: &mut Ui, cx: &mut Cx<'_>) {
    ui_kit::page_scroll(ui, "about", |ui| {
        ui_kit::card(ui, None, |ui| {
            ui.horizontal(|ui| {
                logo(ui, 56.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new("ssx").size(26.0).strong().color(Color32::WHITE));
                    ui.label(RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION"))).color(theme::TEXT));
                    ui_kit::hint(ui, "Screenshots, recordings and uploads, on Windows, Linux and macOS.");
                });
            });
        });
        ui_kit::card(ui, Some("Licence"), |ui| {
            ui.label(format!("ssx is free software, released under the {LICENSE} licence."));
            ui_kit::hint(ui, "You may use, study, share and change it. If you distribute a modified version you must do so under the same licence and make the source available.");
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Source code:");
                if ui_kit::link(ui, REPOSITORY).clicked() {
                    let t = cx.time(ui.ctx());
                    if let Err(e) = cx.host.opener.open_url(REPOSITORY) {
                        cx.toasts.error(t, e);
                    }
                }
            });
        });
        ui_kit::card(ui, Some("Where things are"), |ui| {
            for (label, path) in [
                ("Settings", cx.host.paths.settings_file()),
                ("Config folder", cx.host.paths.config_dir.clone()),
                ("Data folder", cx.host.paths.data_dir.clone()),
                ("History database", cx.host.paths.history_db()),
            ] {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(130.0, 18.0), egui::Sense::hover());
                    ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(13.0), theme::TEXT_DIM);
                    ui.add(egui::Label::new(RichText::new(path.display().to_string()).monospace().size(12.0)).selectable(true));
                });
            }
        });
        ui_kit::card(ui, Some("Third-party software"), |ui| {
            ui_kit::hint(ui, "ssx is built on the work of many people. The main components, with their licences (all compatible with the GPL); the complete list is generated for each release.");
            ui.add_space(6.0);
            egui::Grid::new("notices").num_columns(3).spacing([18.0, 4.0]).striped(false).show(ui, |ui| {
                for n in NOTICES {
                    ui.label(RichText::new(n.name).color(Color32::WHITE));
                    ui.label(RichText::new(n.purpose).color(theme::TEXT_DIM));
                    ui.label(RichText::new(n.license).monospace().size(12.0));
                    ui.end_row();
                }
            });
        });
    });
}

/// The ssx mark: a rounded square in the accent colour with a viewfinder.
pub fn logo(ui: &mut Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    paint_logo(ui.painter(), rect);
}

/// Paints the mark into `rect`.
pub fn paint_logo(p: &egui::Painter, rect: egui::Rect) {
    let s = rect.width();
    p.rect_filled(rect, egui::CornerRadius::same((s * 0.22) as u8), theme::ACCENT);
    let inner = rect.shrink(s * 0.24);
    let stroke = egui::Stroke::new((s * 0.075).max(1.5), Color32::WHITE);
    let l = s * 0.16;
    for (corner, dx, dy) in [
        (inner.left_top(), 1.0, 1.0),
        (inner.right_top(), -1.0, 1.0),
        (inner.left_bottom(), 1.0, -1.0),
        (inner.right_bottom(), -1.0, -1.0),
    ] {
        p.line_segment([corner, corner + egui::vec2(dx * l, 0.0)], stroke);
        p.line_segment([corner, corner + egui::vec2(0.0, dy * l)], stroke);
    }
    p.circle_filled(rect.center(), s * 0.07, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn licence_matches_the_workspace() {
        assert_eq!(LICENSE, env!("CARGO_PKG_LICENSE"));
    }

    #[test]
    fn every_notice_has_a_licence_and_a_purpose() {
        assert!(NOTICES.len() >= 10);
        for n in NOTICES {
            assert!(!n.name.is_empty() && !n.purpose.is_empty() && !n.license.is_empty(), "{n:?}");
            assert!(!n.license.contains("GPL"), "{n:?}: third-party licences here are permissive");
        }
    }
}
