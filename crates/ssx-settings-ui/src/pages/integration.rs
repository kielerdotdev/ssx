//! Integration: right-click entries in the file managers, and the diagnostics report.
//!
//! Installing and removing go through `ssx_shell::Integrations` exactly as `ssx shell
//! install` does (idempotent, marker files, nothing of anyone else's touched) and the
//! per-file-manager [`Report`] is shown as it comes back. The diagnostics are the CLI's
//! `doctor` report, gathered on a worker thread (probing the clipboard, the notification
//! service and the credential store can take seconds), with a *Copy report* button that puts
//! the same text `ssx doctor` prints on the clipboard.

use egui::{Color32, RichText, Ui};
use ssx_cli::{
    commands::{
        doctor::{MonitorInfo, Problem, Report as DoctorReport, Severity as DoctorSeverity, render},
        shell::{IntegrationState, states},
    },
    output::Style,
};
use ssx_editor_ui::ui::theme;
use ssx_shell::{Report as ShellReport, Status};

use super::Cx;
use crate::{
    task::Slot,
    ui_kit::{self, Field},
};

/// What a finished install / uninstall hands back.
#[derive(Debug, Clone)]
pub struct ShellDone {
    /// What happened to each file manager (`None` when only the state was refreshed).
    pub report: Option<ShellReport>,
    /// The state afterwards.
    pub states: Vec<IntegrationState>,
    /// `true` if it was an uninstall.
    pub uninstall: bool,
}

/// State of the Integration page.
#[derive(Default)]
pub struct State {
    /// Install even for file managers that were not detected.
    pub force: bool,
    /// The state of each file manager.
    pub states: Vec<IntegrationState>,
    /// The report of the last install or uninstall.
    pub last: Option<(bool, ShellReport)>,
    /// The diagnostics, once gathered.
    pub doctor: Option<Result<DoctorReport, String>>,
    /// Which file manager's details are open.
    pub details_open: Option<String>,
    shell_slot: Slot<ShellDone>,
    doctor_slot: Slot<Result<DoctorReport, String>>,
    states_loaded: bool,
    doctor_started: bool,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("force", &self.force)
            .field("states", &self.states.len())
            .field("doctor", &self.doctor.as_ref().map(Result::is_ok))
            .finish_non_exhaustive()
    }
}

impl State {
    /// Whether background work is running.
    pub fn busy(&self) -> bool {
        self.shell_slot.running() || self.doctor_slot.running()
    }

    /// Re-reads the state of the file managers on the next frame.
    pub fn refresh_states(&mut self) {
        self.states_loaded = false;
    }

    /// Runs the diagnostics again.
    pub fn rerun_doctor(&mut self) {
        self.doctor_started = false;
    }
}

/// How a per-file-manager status reads, and whether it is good news.
pub fn status_text(s: &Status) -> (String, bool) {
    match s {
        Status::Installed => ("installed".to_owned(), true),
        Status::Updated => ("updated".to_owned(), true),
        Status::AlreadyPresent => ("already installed".to_owned(), true),
        Status::Removed => ("removed".to_owned(), true),
        Status::NotPresent => ("was not installed".to_owned(), true),
        Status::Skipped(why) => (format!("skipped: {why}"), false),
        Status::Failed(e) => (format!("failed: {e}"), false),
    }
}

/// `"HDMI-1 (0,0 2560x1440, scale 1.00, primary), HDR on (203 nits SDR white)"`.
pub fn monitor_line(m: &MonitorInfo) -> String {
    let hdr = match (m.hdr.as_str(), m.sdr_white_nits) {
        ("on", Some(n)) => format!("HDR on ({n:.0} nits SDR white)"),
        ("on", None) => "HDR on".to_owned(),
        ("off", _) => "HDR off".to_owned(),
        _ => "HDR unknown".to_owned(),
    };
    format!(
        "{} ({}, scale {:.2}{}), {hdr}",
        m.name,
        m.rect,
        m.scale_factor,
        if m.primary { ", primary" } else { "" }
    )
}

/// The report as plain text, exactly what `ssx doctor` prints.
pub fn report_text(r: &DoctorReport) -> String {
    render(r, Style::plain())
}

fn severity_color(s: DoctorSeverity) -> Color32 {
    match s {
        DoctorSeverity::Error => ui_kit::ERROR_TEXT,
        DoctorSeverity::Warning => ui_kit::WARN_TEXT,
        DoctorSeverity::Info => theme::ACCENT,
    }
}

fn severity_word(s: DoctorSeverity) -> &'static str {
    match s {
        DoctorSeverity::Error => "error",
        DoctorSeverity::Warning => "warning",
        DoctorSeverity::Info => "info",
    }
}

fn poll(st: &mut State, ctx: &egui::Context) {
    if let Some(done) = st.shell_slot.poll() {
        st.states = done.states;
        st.states_loaded = true;
        if let Some(r) = done.report {
            st.last = Some((done.uninstall, r));
        }
    }
    if let Some(r) = st.doctor_slot.poll() {
        st.doctor = Some(r);
    }
    let _ = ctx;
}

/// Draws the page.
pub fn ui(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    let ctx = ui.ctx().clone();
    poll(st, &ctx);
    if !st.states_loaded && !st.shell_slot.running() {
        let shell = cx.host.shell.clone();
        st.states_loaded = true;
        st.shell_slot.start(cx.wake, move || ShellDone {
            report: None,
            states: match &shell.context {
                Ok(c) => states(&shell.integrations, c),
                Err(_) => Vec::new(),
            },
            uninstall: false,
        });
    }
    if !st.doctor_started {
        st.doctor_started = true;
        let doctor = cx.host.doctor.clone();
        let paths = cx.host.paths.clone();
        st.doctor_slot.start(cx.wake, move || doctor.gather(&paths));
    }
    ui_kit::page_scroll(ui, "integration", |ui| {
        menus_card(ui, st, cx);
        diagnostics(ui, st, cx);
    });
}

fn menus_card(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Right-click menus"), |ui| {
        ui_kit::hint(ui, "Adds \"Upload with ssx\", \"Edit image with ssx\" and \"Upload as video with ssx\" to your file manager's context menu. Each entry only starts the ssx command with the selected files; nothing else is installed, and every file ssx writes is marked so it can be removed cleanly.");
        ui.add_space(6.0);
        let shell = cx.host.shell.clone();
        let ctx_ok = match &shell.context {
            Ok(c) => {
                Field::new("Program the menus run").show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(c.ssx_exe.display().to_string()).monospace().size(12.5)).selectable(true));
                    if !c.ssx_exe.is_file() {
                        ui.horizontal_top(|ui| {
                            ui_kit::severity_icon(ui, ssx_core::settings::Severity::Warning);
                            ui.add(egui::Label::new(RichText::new("This program was not found; the menu entries would do nothing until ssx is installed there.").size(12.0).color(ui_kit::WARN_TEXT)).wrap());
                        });
                    }
                });
                true
            }
            Err(e) => {
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, ssx_core::settings::Severity::Error);
                    ui.add(egui::Label::new(RichText::new(format!("The menus cannot be installed: {e}")).color(ui_kit::ERROR_TEXT)).wrap());
                });
                false
            }
        };
        ui.add_space(6.0);
        if st.states.is_empty() && st.shell_slot.running() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Looking for file managers...");
            });
        }
        for s in st.states.clone() {
            file_manager_row(ui, st, cx, &s);
        }
        if ctx_ok {
            ui.add_space(6.0);
            ui_kit::divider(ui);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                let busy = st.shell_slot.running();
                if ui_kit::primary(ui, "Install for my file managers", !busy).clicked() && !busy {
                    run_shell(st, cx, false);
                }
                if ui_kit::button_if(ui, "Remove all entries", !busy, "Working").clicked() {
                    run_shell(st, cx, true);
                }
                ui_kit::switch(ui, "Also for file managers that were not found", &mut st.force);
            });
        }
        if let Some((uninstall, report)) = st.last.clone() {
            ui.add_space(8.0);
            ui.label(RichText::new(if uninstall { "Result of the removal" } else { "Result of the installation" }).strong().color(Color32::WHITE));
            ui.add_space(2.0);
            for e in &report.entries {
                let (text, ok) = status_text(&e.status);
                ui.horizontal_top(|ui| {
                    let (mark, color) = if ok { ("\u{2714}", ui_kit::OK_TEXT) } else if e.status.is_failure() { ("\u{2716}", ui_kit::ERROR_TEXT) } else { ("\u{2013}", theme::TEXT_DIM) };
                    ui.label(RichText::new(mark).color(color));
                    ui.label(RichText::new(e.name).strong());
                    ui.add(egui::Label::new(RichText::new(text).color(if e.status.is_failure() { ui_kit::ERROR_TEXT } else { theme::TEXT_DIM })).wrap());
                });
            }
            if report.has_failures() {
                ui_kit::hint(ui, "The others were processed normally.");
            }
        }
    });
}

fn file_manager_row(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>, s: &IntegrationState) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(RichText::new(&s.name).strong().color(Color32::WHITE));
        ui_kit::badge(ui, if s.detected { "found" } else { "not found" }, if s.detected { ui_kit::OK_TEXT } else { theme::TEXT_DIM });
        ui_kit::badge(ui, if s.installed { "menu installed" } else { "no menu" }, if s.installed { theme::ACCENT } else { theme::TEXT_DIM });
        let open = st.details_open.as_deref() == Some(s.id.as_str());
        if ui_kit::link(ui, if open { "Hide details" } else { "Details" }).clicked() {
            st.details_open = if open { None } else { Some(s.id.clone()) };
        }
    });
    ui_kit::hint(ui, &s.detail);
    if st.details_open.as_deref() == Some(s.id.as_str())
        && let Ok(c) = &cx.host.shell.context
        && let Some(i) = cx.host.shell.integrations.get(&s.id)
    {
        let d = i.describe(c);
        egui::Frame::new()
            .fill(theme::CANVAS_BG)
            .corner_radius(egui::CornerRadius::same(5))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&d.summary).size(12.5));
                for a in &d.artefacts {
                    ui.add(egui::Label::new(RichText::new(a).monospace().size(11.5).color(theme::TEXT_DIM)).wrap().selectable(true));
                }
                for n in &d.notes {
                    ui_kit::hint(ui, n);
                }
            });
    }
    ui.add_space(6.0);
}

fn run_shell(st: &mut State, cx: &Cx<'_>, uninstall: bool) {
    let shell = cx.host.shell.clone();
    let force = st.force;
    st.shell_slot.start(cx.wake, move || {
        let Ok(ctx) = &shell.context else {
            return ShellDone { report: None, states: Vec::new(), uninstall };
        };
        let report = if uninstall { shell.integrations.uninstall_all(ctx) } else { shell.integrations.install_all_with(ctx, force) };
        let states = states(&shell.integrations, ctx);
        ShellDone { report: Some(report), states, uninstall }
    });
}

fn diagnostics(ui: &mut Ui, st: &mut State, cx: &mut Cx<'_>) {
    ui_kit::card(ui, Some("Diagnostics"), |ui| {
        ui.horizontal(|ui| {
            if st.doctor_slot.running() {
                ui.spinner();
                ui.label("Checking this machine...");
            } else {
                if ui_kit::button(ui, "Run again").clicked() {
                    st.rerun_doctor();
                }
                let can = st.doctor.as_ref().is_some_and(Result::is_ok);
                if ui_kit::button_if(ui, "Copy report", can, "Nothing to copy yet").on_hover_text("Copy the report as text, to paste into a bug report").clicked()
                    && let Some(Ok(r)) = &st.doctor
                {
                    ui.ctx().copy_text(report_text(r));
                    let t = cx.time(ui.ctx());
                    cx.toasts.success(t, "Report copied to the clipboard");
                }
            }
        });
        ui.add_space(6.0);
        match st.doctor.clone() {
            None => {}
            Some(Err(e)) => {
                ui.horizontal_top(|ui| {
                    ui_kit::severity_icon(ui, ssx_core::settings::Severity::Error);
                    ui.add(egui::Label::new(RichText::new(format!("The diagnostics failed: {e}")).color(ui_kit::ERROR_TEXT)).wrap());
                });
            }
            Some(Ok(r)) => report_view(ui, &r),
        }
    });
}

fn kv(ui: &mut Ui, key: &str, value: impl Into<String>) {
    ui.horizontal_top(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(150.0, 18.0), egui::Sense::hover());
        ui.painter().text(rect.left_top() + egui::vec2(0.0, 1.0), egui::Align2::LEFT_TOP, key, egui::FontId::proportional(12.5), theme::TEXT_DIM);
        ui.add(egui::Label::new(RichText::new(value.into()).size(12.5)).wrap().selectable(true));
    });
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong().size(13.0).color(Color32::WHITE));
    ui.add_space(2.0);
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn report_view(ui: &mut Ui, r: &DoctorReport) {
    if r.problems.is_empty() {
        ui.horizontal(|ui| {
            ui.label(RichText::new("\u{2714}").color(ui_kit::OK_TEXT));
            ui.label("No problems found.");
        });
    }
    for p in &r.problems {
        problem(ui, p);
    }
    section(ui, "System");
    kv(ui, "ssx", format!("{} on {} {}{}", r.version, r.os.name, r.os.arch, r.os.distribution.as_ref().map_or_else(String::new, |d| format!(" ({d})"))));
    kv(ui, "Session", format!("{}, desktop {}", r.session.session_type, r.session.desktop));
    section(ui, "Screen capture");
    match (&r.capture.backend, &r.capture.error) {
        (Some(b), _) => kv(ui, "Backend", b.clone()),
        (None, e) => kv(ui, "Backend", format!("none. {}", e.clone().unwrap_or_default())),
    }
    for a in &r.capture.attempts {
        kv(ui, &format!("Tried {}", a.backend), a.reason.as_ref().map_or_else(|| "worked".to_owned(), |x| format!("rejected: {x}")));
    }
    section(ui, "Monitors and HDR");
    if r.monitors.is_empty() {
        kv(ui, "Monitors", r.monitors_error.clone().unwrap_or_else(|| "none reported".to_owned()));
    }
    for m in &r.monitors {
        kv(ui, &m.id, monitor_line(m));
    }
    section(ui, "This desktop");
    kv(ui, "Clipboard", format!("{} (wl-copy {}, xclip {})", if r.clipboard.usable { "works" } else { "not available" }, yes_no(r.clipboard.wl_copy), yes_no(r.clipboard.xclip)));
    kv(ui, "Notifications", r.notifications.server.clone().or_else(|| r.notifications.error.clone().map(|e| format!("not available ({e})"))).unwrap_or_default());
    kv(ui, "Secret store", r.keyring.backend.clone().unwrap_or_else(|| format!("none ({})", r.keyring.reason.clone().unwrap_or_default())));
    kv(ui, "Editor helper", r.editor.path.as_ref().map_or_else(|| "not found".to_owned(), |p| p.display().to_string()));
    kv(ui, "Hotkeys", format!("{} workflow(s) with a shortcut; {}", r.hotkeys.bound_workflows, r.hotkeys.recommended));
    section(ui, "Files");
    kv(ui, "Settings", format!("{}{}", r.paths.settings_file.display(), if r.paths.settings_exists { "" } else { " (not created yet)" }));
    kv(ui, "Data", r.paths.data_dir.display().to_string());
    kv(ui, "History", format!("{}{}", r.paths.history_db.display(), if r.paths.history_ok { "" } else { " (cannot be opened)" }));
    kv(ui, "Uploaders", format!("{} configured, folder {}", r.uploaders.len(), r.paths.uploaders_dir.display()));
}

fn problem(ui: &mut Ui, p: &Problem) {
    ui.horizontal_top(|ui| {
        ui_kit::badge(ui, severity_word(p.severity), severity_color(p.severity));
        ui.vertical(|ui| {
            ui.add(egui::Label::new(RichText::new(&p.message).color(theme::TEXT)).wrap());
            if let Some(h) = &p.hint {
                ui_kit::hint(ui, h);
            }
        });
    });
    ui.add_space(4.0);
}

/// A believable report for tests and screenshots.
pub fn sample_report() -> DoctorReport {
    use ssx_cli::commands::doctor::{
        Attempt, CaptureInfo, ClipboardInfo, EditorInfo, HotkeyInfo, KeyringInfo, NotificationInfo, OsInfo,
        PathsInfo, SessionInfo, SettingsInfo, UploaderState,
    };
    let mut r = DoctorReport {
        version: "0.1.0".to_owned(),
        os: OsInfo { name: "linux".into(), arch: "x86_64".into(), distribution: Some("Fedora Linux 42".into()) },
        session: SessionInfo {
            session_type: "wayland".into(),
            wayland_display: Some("wayland-1".into()),
            display: None,
            desktop_env: Some("sway".into()),
            desktop: "sway".into(),
            forced_backend: None,
        },
        capture: CaptureInfo {
            ok: true,
            backend: Some("wlroots screencopy".into()),
            attempts: vec![
                Attempt { backend: "wayland".into(), ok: true, reason: None },
                Attempt { backend: "portal".into(), ok: false, reason: Some("no xdg-desktop-portal on the session bus".into()) },
            ],
            capabilities: None,
            error: None,
        },
        monitors: vec![
            MonitorInfo { id: "DP-1".into(), name: "DELL U2723QE".into(), rect: "0,0 3840x2160".into(), scale_factor: 1.5, primary: true, hdr: "on".into(), sdr_white_nits: Some(203.0), max_luminance_nits: Some(600.0) },
            MonitorInfo { id: "HDMI-A-1".into(), name: "LG 27GL850".into(), rect: "3840,0 2560x1440".into(), scale_factor: 1.0, primary: false, hdr: "off".into(), sdr_white_nits: None, max_luminance_nits: None },
        ],
        monitors_error: None,
        clipboard: ClipboardInfo { usable: true, arboard: true, arboard_error: None, wl_copy: true, xclip: false, file_list_format: "text/uri-list".into() },
        notifications: NotificationInfo { ok: true, server: Some("mako 1.9".into()), error: None },
        keyring: KeyringInfo { persistent: false, backend: None, reason: Some("no Secret Service on the session bus".into()) },
        paths: PathsInfo {
            config_dir: "/home/demo/.config/ssx".into(),
            settings_file: "/home/demo/.config/ssx/settings.toml".into(),
            settings_exists: true,
            data_dir: "/home/demo/.local/share/ssx".into(),
            history_db: "/home/demo/.local/share/ssx/history.sqlite3".into(),
            history_ok: true,
            uploaders_dir: "/home/demo/.config/ssx/uploaders".into(),
        },
        settings: SettingsInfo { valid: true, findings: vec![], error: None },
        uploaders: vec![UploaderState { name: "local".into(), kind: "local".into(), error: None }],
        editor: EditorInfo { found: true, path: Some("/usr/local/bin/ssx-editor-ui".into()) },
        shell_integrations: vec![],
        hotkeys: HotkeyInfo { recommended: "sway bindsym include file".into(), strategies: vec!["sway bindsym include file".into()], bound_workflows: 4 },
        problems: vec![],
    };
    r.problems = ssx_cli::commands::doctor::derive_problems(&r);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_read_naturally() {
        assert_eq!(status_text(&Status::Installed), ("installed".to_owned(), true));
        assert_eq!(status_text(&Status::NotPresent).0, "was not installed");
        let (t, ok) = status_text(&Status::Skipped("Dolphin is not installed".into()));
        assert!(t.contains("Dolphin") && !ok);
        let (t, ok) = status_text(&Status::Failed("permission denied".into()));
        assert!(t.starts_with("failed") && !ok);
    }

    #[test]
    fn monitor_lines_show_hdr_state() {
        let r = sample_report();
        assert_eq!(monitor_line(&r.monitors[0]), "DELL U2723QE (0,0 3840x2160, scale 1.50, primary), HDR on (203 nits SDR white)");
        assert_eq!(monitor_line(&r.monitors[1]), "LG 27GL850 (3840,0 2560x1440, scale 1.00), HDR off");
    }

    #[test]
    fn the_copied_report_is_what_doctor_prints() {
        let r = sample_report();
        let t = report_text(&r);
        assert!(t.contains("ssx 0.1.0 on linux x86_64"), "{t}");
        assert!(t.contains("wlroots screencopy") && t.contains("HDR on"), "{t}");
        assert!(!t.contains('\u{1b}'), "no colour codes in a pasted report");
    }

    #[test]
    fn the_sample_report_has_problems_worth_showing() {
        let r = sample_report();
        assert!(r.problems.iter().any(|p| p.message.contains("credential store")), "{:?}", r.problems);
        assert!(r.problems.iter().any(|p| matches!(p.severity, DoctorSeverity::Info)));
    }
}
