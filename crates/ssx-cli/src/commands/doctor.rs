//! `ssx doctor`: what works on this machine, why, and what to fix.
//!
//! The report is *data first*: [`gather`] probes the machine into a [`Report`], [`derive_problems`]
//! turns that into an actionable list (pure, unit-tested), and [`render`] prints it. `--json`
//! prints the same [`Report`]. Probing never changes anything and never takes a screenshot; it
//! only enumerates monitors and asks each service whether it can start.

use std::path::PathBuf;

use serde::Serialize;
use ssx_core::settings::Settings;
use ssx_hotkeys::detect::{Environment, Platform, detect};
use ssx_services::{ClipboardDiagnosis, ExternalEditor, UploaderInfo, probe_notifications};
use ssx_shell::{Context, Integrations};

use crate::{
    app::App,
    cli::DoctorArgs,
    commands::{
        config::findings_for,
        hotkeys::{bindings_from_settings, describe_detection, strategy_text},
        list::rect_text,
        shell::{IntegrationState, states},
    },
    error::{CliError, CliResult},
    output::{Style, out_line, out_text},
};

/// How bad a problem is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing, nothing is broken.
    Info,
    /// A feature will not work as expected.
    Warning,
    /// A core function is broken.
    Error,
}

/// One actionable finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// Severity.
    pub severity: Severity,
    /// What is wrong.
    pub message: String,
    /// What to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Problem {
    fn new(severity: Severity, message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { severity, message: message.into(), hint: Some(hint.into()) }
    }
}

/// OS facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OsInfo {
    /// `linux`, `windows`, `macos`.
    pub name: String,
    /// CPU architecture.
    pub arch: String,
    /// Distribution name from `/etc/os-release`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distribution: Option<String>,
}

/// The graphical session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionInfo {
    /// `x11`, `wayland` or `unknown`.
    pub session_type: String,
    /// `WAYLAND_DISPLAY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wayland_display: Option<String>,
    /// `DISPLAY`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// `XDG_CURRENT_DESKTOP`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_env: Option<String>,
    /// The desktop / compositor ssx recognised.
    pub desktop: String,
    /// `SSX_BACKEND`, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forced_backend: Option<String>,
}

/// One capture backend candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attempt {
    /// Backend family.
    pub backend: String,
    /// Whether it initialised.
    pub ok: bool,
    /// Why not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The capture backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureInfo {
    /// A backend works.
    pub ok: bool,
    /// The chosen backend's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Every candidate tried, in order.
    pub attempts: Vec<Attempt>,
    /// What the backend can do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<serde_json::Value>,
    /// Why no backend works (contains one reason per candidate).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A monitor as reported.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorInfo {
    /// Backend id.
    pub id: String,
    /// Name.
    pub name: String,
    /// `x,y WxH` in desktop pixels.
    pub rect: String,
    /// UI scale factor.
    pub scale_factor: f64,
    /// Primary monitor.
    pub primary: bool,
    /// `on`, `off` or `unknown`.
    pub hdr: String,
    /// SDR white level while HDR is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdr_white_nits: Option<f32>,
    /// Panel peak luminance, if reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_luminance_nits: Option<f32>,
}

/// Clipboard state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClipboardInfo {
    /// Something can copy.
    pub usable: bool,
    /// `arboard` opens the clipboard.
    pub arboard: bool,
    /// Why not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arboard_error: Option<String>,
    /// `wl-copy` installed.
    pub wl_copy: bool,
    /// `xclip` installed.
    pub xclip: bool,
    /// Format for file lists.
    pub file_list_format: String,
}

/// Notification service state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NotificationInfo {
    /// A notification service answered.
    pub ok: bool,
    /// Its name and version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Why not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Secret store state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyringInfo {
    /// Secrets survive the process.
    pub persistent: bool,
    /// The store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Why there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Where things live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathsInfo {
    /// Config directory.
    pub config_dir: PathBuf,
    /// `settings.toml`.
    pub settings_file: PathBuf,
    /// Whether it exists.
    pub settings_exists: bool,
    /// Data directory.
    pub data_dir: PathBuf,
    /// History database.
    pub history_db: PathBuf,
    /// Whether the history database can be opened.
    pub history_ok: bool,
    /// The `.sxcu` folder.
    pub uploaders_dir: PathBuf,
}

/// Settings health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettingsInfo {
    /// Loaded and free of errors.
    pub valid: bool,
    /// Error / warning texts.
    pub findings: Vec<String>,
    /// Why the file could not be read at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Editor helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EditorInfo {
    /// Helper found.
    pub found: bool,
    /// Its path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

/// Hotkey situation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HotkeyInfo {
    /// Preferred mechanism, in words.
    pub recommended: String,
    /// All mechanisms, best first.
    pub strategies: Vec<String>,
    /// Workflows that have a bindable hotkey.
    pub bound_workflows: usize,
}

/// Uploader listing entry (name, kind, error).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UploaderState {
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: String,
    /// Load error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The complete report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// ssx version.
    pub version: String,
    /// OS.
    pub os: OsInfo,
    /// Graphical session.
    pub session: SessionInfo,
    /// Capture backend.
    pub capture: CaptureInfo,
    /// Monitors.
    pub monitors: Vec<MonitorInfo>,
    /// Why monitors could not be listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitors_error: Option<String>,
    /// Clipboard.
    pub clipboard: ClipboardInfo,
    /// Notifications.
    pub notifications: NotificationInfo,
    /// Secret store.
    pub keyring: KeyringInfo,
    /// Paths.
    pub paths: PathsInfo,
    /// Settings.
    pub settings: SettingsInfo,
    /// Upload destinations.
    pub uploaders: Vec<UploaderState>,
    /// Editor helper.
    pub editor: EditorInfo,
    /// File-manager integrations.
    pub shell_integrations: Vec<IntegrationState>,
    /// Hotkeys.
    pub hotkeys: HotkeyInfo,
    /// What to fix, worst first.
    pub problems: Vec<Problem>,
}

/// Pulls `PRETTY_NAME` out of an os-release file.
pub fn parse_os_release(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_owned())
        .filter(|v| !v.is_empty())
}

/// The session as seen through `env`.
pub fn session_info(env: &Environment, forced_backend: Option<String>) -> SessionInfo {
    let d = detect(env, Platform::current());
    let (desktop, session) = describe_detection(&d);
    SessionInfo {
        session_type: session.to_ascii_lowercase(),
        wayland_display: env.wayland_display.clone(),
        display: env.display.clone(),
        desktop_env: env.xdg_current_desktop.clone(),
        desktop,
        forced_backend,
    }
}

/// Turns the probed facts into the list of things to fix, worst first.
pub fn derive_problems(r: &Report) -> Vec<Problem> {
    use Severity::{Error, Info, Warning};
    let mut p = Vec::new();

    if !r.capture.ok {
        let graphical = r.session.display.is_some() || r.session.wayland_display.is_some();
        let hint = if graphical {
            "each backend's reason is listed above; force one with --backend / SSX_BACKEND. On Wayland, wlroots \
             compositors (sway, Hyprland) work directly, GNOME and KDE need xdg-desktop-portal"
        } else {
            "there is no graphical session (neither DISPLAY nor WAYLAND_DISPLAY is set): run ssx inside a desktop session"
        };
        p.push(Problem::new(Error, "no screen-capture backend works, so ssx cannot take screenshots", hint));
    }
    if let Some(e) = &r.monitors_error {
        let sev = if r.capture.ok { Info } else { Warning };
        p.push(Problem::new(
            sev,
            format!("monitors cannot be listed ({e}); `capture monitor` and multi-monitor region maths are limited"),
            "expected on portal-based capture (GNOME/KDE), where the desktop picks the source",
        ));
    }
    for m in r.monitors.iter().filter(|m| m.hdr == "on") {
        p.push(Problem::new(
            Info,
            format!("HDR is on for {:?}: screenshots are tone-mapped to SDR", m.name),
            "tune it with `ssx config set capture.hdr.knee 0.8` (keep highlight detail) or capture.hdr.exposure (stops)",
        ));
    }
    if !r.clipboard.usable {
        p.push(Problem::new(
            Warning,
            "the clipboard cannot be used, so --copy and copy steps will fail",
            "install `wl-clipboard` (Wayland) or `xclip` (X11), and run inside a desktop session",
        ));
    } else if !r.clipboard.wl_copy && !r.clipboard.xclip && r.session.session_type != "unknown" && cfg!(target_os = "linux") {
        p.push(Problem::new(
            Info,
            "no wl-copy/xclip: copies made by short-lived `ssx` commands may vanish when the command exits",
            "install `wl-clipboard` (Wayland) or `xclip` (X11)",
        ));
    }
    if !r.notifications.ok {
        p.push(Problem::new(
            Warning,
            format!(
                "no desktop notification service ({}); notifications are skipped",
                r.notifications.error.as_deref().unwrap_or("unavailable")
            ),
            "run a notification daemon (dunst, mako, swaync) or a full desktop session",
        ));
    }
    if !r.keyring.persistent {
        p.push(Problem::new(
            Warning,
            format!(
                "no OS credential store ({}); uploader secrets cannot be saved",
                r.keyring.reason.as_deref().unwrap_or("unavailable")
            ),
            "start a Secret Service provider (GNOME Keyring, KWallet) or pass secrets as SSX_SECRET_<NAME> environment variables",
        ));
    }
    if !r.paths.history_ok {
        p.push(Problem::new(
            Warning,
            format!("the history database {} cannot be opened", r.paths.history_db.display()),
            "check permissions of the data directory; uploads still work but are not recorded",
        ));
    }
    if let Some(e) = &r.settings.error {
        p.push(Problem::new(Error, format!("settings.toml cannot be read: {e}"), "fix it with `ssx config edit` or start over with `ssx config reset`"));
    } else if !r.settings.valid {
        p.push(Problem::new(
            Error,
            format!("settings.toml has errors ({} finding(s))", r.settings.findings.len()),
            "run `ssx config validate` for the details",
        ));
    }
    for u in r.uploaders.iter().filter(|u| u.error.is_some()) {
        p.push(Problem::new(
            Warning,
            format!("uploader {:?} does not load: {}", u.name, u.error.as_deref().unwrap_or_default()),
            "fix its [uploaders] table or .sxcu file; `ssx uploaders list` shows all",
        ));
    }
    if !r.editor.found {
        p.push(Problem::new(
            Warning,
            "the image editor helper `ssx-editor-ui` was not found: `ssx edit` and --edit will not work",
            "install it next to ssx or on PATH, or set SSX_EDITOR_UI to its path",
        ));
    }
    p.push(Problem::new(
        Info,
        "interactive region selection is not available in this build",
        "capture exact regions with `ssx capture region --rect x,y,w,h`, or repeat one with `ssx capture last-region`",
    ));
    if !r.shell_integrations.is_empty()
        && r.shell_integrations.iter().any(|s| s.detected)
        && !r.shell_integrations.iter().any(|s| s.installed)
    {
        p.push(Problem::new(
            Info,
            "no file-manager right-click entries are installed",
            "add them with `ssx shell install` (preview with --dry-run)",
        ));
    }
    if r.hotkeys.bound_workflows > 0 && r.hotkeys.strategies.iter().all(|s| s.contains("bind `ssx run")) {
        p.push(Problem::new(
            Info,
            "this desktop cannot register global hotkeys for ssx by itself",
            "bind `ssx run <workflow>` in your desktop's keyboard settings",
        ));
    }
    p.sort_by(|a, b| b.severity.cmp(&a.severity));
    p
}

/// Probes the machine.
pub fn gather(app: &App) -> CliResult<Report> {
    let env = Environment::from_env();
    let forced = app.global.backend.clone().or_else(|| std::env::var("SSX_BACKEND").ok().filter(|v| !v.is_empty()));
    let session = session_info(&env, forced);

    // Settings problems must not stop the diagnosis, so load them by hand.
    let settings_path = app.paths.settings_file();
    let (settings, settings_info) = match std::fs::read_to_string(&settings_path) {
        Ok(text) => match findings_for(&text, &app.paths.config_dir) {
            Ok(f) => {
                let valid = !f.iter().any(|x| x.severity == "error");
                let findings = f.iter().map(|x| format!("{}: {}: {}", x.severity, x.path, x.message)).collect();
                let s = Settings::from_toml_str(&text).map(|l| l.settings).unwrap_or_default();
                (s, SettingsInfo { valid, findings, error: None })
            }
            Err(e) => (Settings::default(), SettingsInfo { valid: false, findings: vec![], error: Some(e) }),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (Settings::default(), SettingsInfo { valid: true, findings: vec![], error: None })
        }
        Err(e) => (
            Settings::default(),
            SettingsInfo { valid: false, findings: vec![], error: Some(e.to_string()) },
        ),
    };

    let services = app.services(&settings)?;

    let (capture, monitors, monitors_error) = match services.capturer.backend_report() {
        Ok(rep) => {
            let caps = rep.capabilities;
            let capture = CaptureInfo {
                ok: true,
                backend: Some(rep.name.to_owned()),
                attempts: rep
                    .attempts
                    .iter()
                    .map(|(k, r)| Attempt {
                        backend: k.name().to_owned(),
                        ok: r.is_ok(),
                        reason: r.as_ref().err().cloned(),
                    })
                    .collect(),
                capabilities: Some(serde_json::json!({
                    "enumerate_monitors": caps.enumerate_monitors,
                    "enumerate_windows": caps.enumerate_windows,
                    "capture_windows": caps.capture_windows,
                    "cursor": caps.cursor,
                    "hdr_float": caps.hdr_float,
                    "native_desktop": caps.native_desktop,
                    "needs_user_interaction": caps.needs_user_interaction,
                })),
                error: None,
            };
            let (mons, err) = match services.capturer.monitors() {
                Ok(m) => (m.iter().map(monitor_info).collect(), None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            };
            (capture, mons, err)
        }
        Err(e) => (
            CaptureInfo { ok: false, backend: None, attempts: vec![], capabilities: None, error: Some(e.to_string()) },
            Vec::new(),
            None,
        ),
    };

    let clip: ClipboardDiagnosis = ssx_services::clipboard::diagnose();
    let clipboard = ClipboardInfo {
        usable: clip.usable(),
        arboard: clip.arboard.is_ok(),
        arboard_error: clip.arboard.clone().err(),
        wl_copy: clip.wl_copy,
        xclip: clip.xclip,
        file_list_format: match clip.flavor {
            ssx_services::FileListFlavor::UriList => "text/uri-list",
            ssx_services::FileListFlavor::GnomeCopiedFiles => "x-special/gnome-copied-files",
        }
        .to_owned(),
    };
    let notifications = match probe_notifications() {
        Ok(server) => NotificationInfo { ok: true, server: Some(server), error: None },
        Err(e) => NotificationInfo { ok: false, server: None, error: Some(e) },
    };
    let st = ssx_services::LayeredSecretStore::probe_status();
    let keyring = KeyringInfo {
        persistent: st.persistent,
        backend: st.backend.map(str::to_owned),
        reason: st.unavailable_reason,
    };
    let history_ok = app.open_history().is_ok();
    let paths = PathsInfo {
        config_dir: app.paths.config_dir.clone(),
        settings_file: settings_path.clone(),
        settings_exists: settings_path.exists(),
        data_dir: app.paths.data_dir.clone(),
        history_db: app.paths.history_db(),
        history_ok,
        uploaders_dir: ssx_services::upload::sxcu_dir(&app.paths.config_dir),
    };
    let uploaders: Vec<UploaderState> = services
        .uploads
        .list()
        .into_iter()
        .map(|UploaderInfo { name, kind, error, .. }| UploaderState { name, kind, error })
        .collect();
    let editor = ExternalEditor::discover();
    let editor = EditorInfo { found: editor.helper().is_some(), path: editor.helper().map(std::path::Path::to_path_buf) };

    let shell_integrations = crate::app::exe_path()
        .ok()
        .and_then(|exe| Context::from_env(exe).ok())
        .map(|ctx| states(&Integrations::native(), &ctx))
        .unwrap_or_default();

    let det = detect(&env, Platform::current());
    let (bindings, _) = bindings_from_settings(&settings, "ssx");
    let hotkeys = HotkeyInfo {
        recommended: strategy_text(det.primary()).to_owned(),
        strategies: det.candidates.iter().map(|s| strategy_text(*s).to_owned()).collect(),
        bound_workflows: bindings.len(),
    };

    let mut report = Report {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        os: OsInfo {
            name: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            distribution: std::fs::read_to_string("/etc/os-release").ok().as_deref().and_then(parse_os_release),
        },
        session,
        capture,
        monitors,
        monitors_error,
        clipboard,
        notifications,
        keyring,
        paths,
        settings: settings_info,
        uploaders,
        editor,
        shell_integrations,
        hotkeys,
        problems: Vec::new(),
    };
    report.problems = derive_problems(&report);
    Ok(report)
}

fn monitor_info(m: &ssx_types::Monitor) -> MonitorInfo {
    MonitorInfo {
        id: m.id.clone(),
        name: m.name.clone(),
        rect: rect_text(m.rect),
        scale_factor: m.scale_factor,
        primary: m.primary,
        hdr: match m.hdr {
            None => "unknown",
            Some(h) if h.active => "on",
            Some(_) => "off",
        }
        .to_owned(),
        sdr_white_nits: m.hdr.filter(|h| h.active).map(|h| h.sdr_white_nits),
        max_luminance_nits: m.hdr.and_then(|h| h.max_luminance_nits),
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Renders the report for people.
pub fn render(r: &Report, style: Style) -> String {
    let mut o = String::new();
    let h = |o: &mut String, t: &str| {
        o.push('\n');
        o.push_str(&style.bold(t));
        o.push('\n');
    };
    o.push_str(&format!(
        "ssx {} on {} {}{}\n",
        r.version,
        r.os.name,
        r.os.arch,
        r.os.distribution.as_ref().map_or_else(String::new, |d| format!(" ({d})"))
    ));

    h(&mut o, "Session");
    o.push_str(&format!(
        "  type: {}   desktop: {}{}{}{}\n",
        r.session.session_type,
        r.session.desktop,
        r.session.display.as_ref().map_or_else(String::new, |d| format!("   DISPLAY={d}")),
        r.session.wayland_display.as_ref().map_or_else(String::new, |d| format!("   WAYLAND_DISPLAY={d}")),
        r.session.forced_backend.as_ref().map_or_else(String::new, |d| format!("   SSX_BACKEND={d}")),
    ));

    h(&mut o, "Screen capture");
    match (&r.capture.backend, &r.capture.error) {
        (Some(b), _) => o.push_str(&format!("  backend: {}\n", style.green(b))),
        (None, e) => o.push_str(&format!("  backend: {}\n  {}\n", style.red("none"), e.clone().unwrap_or_default())),
    }
    for a in &r.capture.attempts {
        o.push_str(&format!(
            "  tried {}: {}\n",
            a.backend,
            a.reason.as_deref().map_or_else(|| "ok".to_owned(), |x| format!("rejected: {x}"))
        ));
    }
    if let Some(c) = &r.capture.capabilities {
        let on: Vec<&str> = c
            .as_object()
            .map(|m| m.iter().filter(|(_, v)| v.as_bool() == Some(true)).map(|(k, _)| k.as_str()).collect())
            .unwrap_or_default();
        o.push_str(&format!("  can: {}\n", if on.is_empty() { "nothing special".to_owned() } else { on.join(", ") }));
    }

    h(&mut o, "Monitors");
    if r.monitors.is_empty() {
        o.push_str(&format!("  {}\n", r.monitors_error.as_deref().unwrap_or("none reported")));
    } else {
        for m in &r.monitors {
            o.push_str(&format!(
                "  {} ({}): {}, scale {:.2}{}, HDR {}{}\n",
                m.name,
                m.id,
                m.rect,
                m.scale_factor,
                if m.primary { ", primary" } else { "" },
                m.hdr,
                m.sdr_white_nits.map_or_else(String::new, |n| format!(" ({n:.0} nits SDR white)")),
            ));
        }
    }

    h(&mut o, "Desktop integration");
    o.push_str(&format!(
        "  clipboard: {} (arboard {}, wl-copy {}, xclip {}; file lists as {})\n",
        yes_no(r.clipboard.usable),
        yes_no(r.clipboard.arboard),
        yes_no(r.clipboard.wl_copy),
        yes_no(r.clipboard.xclip),
        r.clipboard.file_list_format
    ));
    o.push_str(&format!(
        "  notifications: {}\n",
        r.notifications.server.clone().or_else(|| r.notifications.error.clone().map(|e| format!("unavailable ({e})"))).unwrap_or_default()
    ));
    o.push_str(&format!(
        "  secret store: {}\n",
        r.keyring.backend.clone().unwrap_or_else(|| format!("none ({})", r.keyring.reason.clone().unwrap_or_default()))
    ));
    o.push_str(&format!(
        "  editor helper: {}\n",
        r.editor.path.as_ref().map_or_else(|| "not found".to_owned(), |p| p.display().to_string())
    ));
    for s in &r.shell_integrations {
        o.push_str(&format!(
            "  {}: {}, {}\n",
            s.name,
            if s.detected { "found" } else { "not found" },
            if s.installed { "entries installed" } else { "no entries" }
        ));
    }

    h(&mut o, "Hotkeys");
    o.push_str(&format!(
        "  {} workflow(s) with a hotkey; best mechanism: {}\n",
        r.hotkeys.bound_workflows, r.hotkeys.recommended
    ));

    h(&mut o, "Files");
    o.push_str(&format!(
        "  config: {}{}\n  data:   {}\n  history: {}{}\n  uploaders: {} configured\n",
        r.paths.settings_file.display(),
        if r.paths.settings_exists { "" } else { " (not created yet, defaults in use)" },
        r.paths.data_dir.display(),
        r.paths.history_db.display(),
        if r.paths.history_ok { "" } else { " (cannot be opened)" },
        r.uploaders.len(),
    ));
    for f in &r.settings.findings {
        o.push_str(&format!("  settings: {f}\n"));
    }

    h(&mut o, &format!("Problems ({})", r.problems.len()));
    if r.problems.is_empty() {
        o.push_str("  none\n");
    }
    for p in &r.problems {
        let tag = match p.severity {
            Severity::Error => style.red("error  "),
            Severity::Warning => style.yellow("warning"),
            Severity::Info => style.dim("info   "),
        };
        o.push_str(&format!("  {tag} {}\n", p.message));
        if let Some(hint) = &p.hint {
            o.push_str(&format!("          hint: {hint}\n"));
        }
    }
    o
}

/// `ssx doctor`.
pub fn run(app: &App, args: &DoctorArgs) -> CliResult<()> {
    let report = gather(app)?;
    if args.json {
        out_line(&serde_json::to_string_pretty(&report)?);
    } else {
        out_text(&render(&report, app.out));
    }
    let errors = report.problems.iter().filter(|p| p.severity == Severity::Error).count();
    if errors > 0 {
        return Err(CliError::new(format!("doctor found {errors} error{}", if errors == 1 { "" } else { "s" }))
            .hint("the problems are listed above"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Report {
        Report {
            version: "0.1.0".into(),
            os: OsInfo { name: "linux".into(), arch: "x86_64".into(), distribution: Some("Test Linux".into()) },
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
                backend: Some("wayland-wlr-screencopy".into()),
                attempts: vec![Attempt { backend: "wayland".into(), ok: true, reason: None }],
                capabilities: Some(serde_json::json!({"enumerate_monitors": true, "cursor": true, "hdr_float": false})),
                error: None,
            },
            monitors: vec![MonitorInfo {
                id: "HEADLESS-1".into(),
                name: "HEADLESS-1".into(),
                rect: "0,0 800x600".into(),
                scale_factor: 1.0,
                primary: true,
                hdr: "unknown".into(),
                sdr_white_nits: None,
                max_luminance_nits: None,
            }],
            monitors_error: None,
            clipboard: ClipboardInfo {
                usable: true,
                arboard: true,
                arboard_error: None,
                wl_copy: true,
                xclip: false,
                file_list_format: "x-special/gnome-copied-files".into(),
            },
            notifications: NotificationInfo { ok: true, server: Some("mako 1.9".into()), error: None },
            keyring: KeyringInfo { persistent: true, backend: Some("Secret Service".into()), reason: None },
            paths: PathsInfo {
                config_dir: "/c".into(),
                settings_file: "/c/settings.toml".into(),
                settings_exists: true,
                data_dir: "/c/data".into(),
                history_db: "/c/data/history.sqlite3".into(),
                history_ok: true,
                uploaders_dir: "/c/uploaders".into(),
            },
            settings: SettingsInfo { valid: true, findings: vec![], error: None },
            uploaders: vec![UploaderState { name: "local".into(), kind: "local".into(), error: None }],
            editor: EditorInfo { found: true, path: Some("/usr/bin/ssx-editor-ui".into()) },
            shell_integrations: vec![],
            hotkeys: HotkeyInfo {
                recommended: "sway bindsym include file".into(),
                strategies: vec!["sway bindsym include file".into()],
                bound_workflows: 4,
            },
            problems: vec![],
        }
    }

    fn messages(r: &Report) -> Vec<(Severity, String)> {
        derive_problems(r).into_iter().map(|p| (p.severity, p.message)).collect()
    }

    #[test]
    fn a_healthy_system_only_gets_the_region_overlay_note() {
        let m = messages(&healthy());
        assert_eq!(m.len(), 1, "{m:?}");
        assert_eq!(m[0].0, Severity::Info);
        assert!(m[0].1.contains("interactive region"));
    }

    #[test]
    fn no_capture_backend_is_an_error_with_session_specific_advice() {
        let mut r = healthy();
        r.capture = CaptureInfo {
            ok: false,
            backend: None,
            attempts: vec![],
            capabilities: None,
            error: Some("wayland: no compositor; portal: no portal".into()),
        };
        let p = derive_problems(&r);
        assert_eq!(p[0].severity, Severity::Error, "worst first");
        assert!(p[0].hint.as_ref().unwrap().contains("--backend"));

        r.session.display = None;
        r.session.wayland_display = None;
        let p = derive_problems(&r);
        assert!(p[0].hint.as_ref().unwrap().contains("no graphical session"));
    }

    #[test]
    fn missing_services_are_warnings_with_a_fix() {
        let mut r = healthy();
        r.clipboard.usable = false;
        r.notifications = NotificationInfo { ok: false, server: None, error: Some("no bus".into()) };
        r.keyring = KeyringInfo { persistent: false, backend: None, reason: Some("no D-Bus session".into()) };
        r.editor = EditorInfo { found: false, path: None };
        r.uploaders.push(UploaderState { name: "bad".into(), kind: "broken".into(), error: Some("missing bucket".into()) });
        let p = derive_problems(&r);
        let text = p.iter().map(|x| x.message.as_str()).collect::<Vec<_>>().join("\n");
        for needle in ["clipboard cannot be used", "no desktop notification service (no bus)", "no OS credential store (no D-Bus session)", "ssx-editor-ui", "uploader \"bad\" does not load: missing bucket"] {
            assert!(text.contains(needle), "{needle} missing from:\n{text}");
        }
        assert!(p.iter().all(|x| x.hint.is_some()), "every problem says what to do");
        assert!(p.windows(2).all(|w| w[0].severity >= w[1].severity), "sorted worst first");
    }

    #[test]
    fn hdr_and_portal_notes_are_informational() {
        let mut r = healthy();
        r.monitors[0].hdr = "on".into();
        r.monitors_error = Some("the portal cannot enumerate monitors".into());
        let m = messages(&r);
        assert!(m.iter().any(|(s, t)| *s == Severity::Info && t.contains("HDR is on")));
        assert!(m.iter().any(|(s, t)| *s == Severity::Info && t.contains("monitors cannot be listed")));
    }

    #[test]
    fn broken_settings_are_errors() {
        let mut r = healthy();
        r.settings = SettingsInfo { valid: false, findings: vec!["error: x".into()], error: None };
        assert_eq!(derive_problems(&r)[0].severity, Severity::Error);
        r.settings = SettingsInfo { valid: false, findings: vec![], error: Some("not TOML".into()) };
        assert!(derive_problems(&r)[0].message.contains("cannot be read"));
    }

    #[test]
    fn os_release_parsing() {
        assert_eq!(parse_os_release("NAME=\"X\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n").as_deref(), Some("Ubuntu 24.04 LTS"));
        assert_eq!(parse_os_release("PRETTY_NAME='Arch Linux'").as_deref(), Some("Arch Linux"));
        assert_eq!(parse_os_release("NAME=x"), None);
        assert_eq!(parse_os_release("PRETTY_NAME=\"\""), None);
    }

    #[test]
    fn the_report_renders_every_section_and_serialises() {
        let mut r = healthy();
        r.problems = derive_problems(&r);
        let text = render(&r, Style::plain());
        for needle in [
            "ssx 0.1.0 on linux x86_64 (Test Linux)",
            "Session",
            "type: wayland",
            "backend: wayland-wlr-screencopy",
            "tried wayland: ok",
            "can: enumerate_monitors, cursor",
            "HEADLESS-1 (HEADLESS-1): 0,0 800x600, scale 1.00, primary, HDR unknown",
            "clipboard: yes",
            "notifications: mako 1.9",
            "secret store: Secret Service",
            "Problems (1)",
        ] {
            assert!(text.contains(needle), "{needle} missing from:\n{text}");
        }
        let json = serde_json::to_value(&r).unwrap();
        for key in ["version", "os", "session", "capture", "monitors", "clipboard", "notifications", "keyring", "paths", "settings", "uploaders", "editor", "shell_integrations", "hotkeys", "problems"] {
            assert!(json.get(key).is_some(), "{key} missing from the JSON");
        }
        assert_eq!(json["problems"][0]["severity"], "info");
    }

    #[test]
    fn session_facts_come_from_the_environment_snapshot() {
        let env = Environment::from_pairs([("XDG_CURRENT_DESKTOP", "GNOME"), ("WAYLAND_DISPLAY", "wayland-0"), ("XDG_SESSION_TYPE", "wayland")]);
        let s = session_info(&env, Some("x11".into()));
        assert_eq!((s.session_type.as_str(), s.desktop.as_str()), ("wayland", "GNOME"));
        assert_eq!(s.forced_backend.as_deref(), Some("x11"));
        assert_eq!(s.wayland_display.as_deref(), Some("wayland-0"));
    }
}
