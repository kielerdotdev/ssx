//! The tray menu as data.
//!
//! [`build_menu`] turns the loaded [`Settings`] and the daemon's current [`UiState`] into a
//! [`Menu`]: a plain, comparable tree with stable action ids. The backends (`tray_ksni`,
//! `tray_native`) only translate that tree into their toolkit's items and map a click back to
//! an [`Action`]; every decision (what is listed, what is disabled, what the label says) is here
//! and unit-tested.
//!
//! Layout:
//!
//! ```text
//!   <one entry per still-capture workflow, with its hotkey>
//!   ----
//!   <recording: Record entries, or "Stop recording (0:12)" while one runs>
//!   ----
//!   Upload files...        (the workflow(s) that take files)
//!   Open editor
//!   ----
//!   History
//!   Settings
//!   Disable hotkeys / Enable hotkeys
//!   Cancel current capture   (only while something interactive is open)
//!   Open captures folder
//!   ----
//!   Quit
//! ```
//!
//! "Disabled" entries stay visible (greyed out) with the reason in the label suffix, because a
//! missing entry cannot be asked about, while "Region capture (overlay not found)" tells the
//! user what to fix.

use ssx_core::settings::{InputKind, Settings, Workflow};

use crate::{clock::format_elapsed, events::RecordingView};

/// What a menu entry does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Action {
    /// Run the workflow with this id (files workflows ask for files first).
    RunWorkflow(String),
    /// Stop the running recording.
    StopRecording,
    /// Close the capture overlay / editor that is open.
    CancelCurrent,
    /// Pick files and upload them with the default file workflow.
    UploadFiles,
    /// Open the editor on an empty canvas / the clipboard image.
    OpenEditor,
    /// Show the history window.
    OpenHistory,
    /// Show the settings window (or the config file).
    OpenSettings,
    /// Turn global hotkeys off or on.
    ToggleHotkeys,
    /// Open the folder screenshots are saved in.
    OpenCapturesFolder,
    /// Exit the daemon.
    Quit,
}

impl Action {
    /// A stable text id (what backends attach to their items).
    pub fn id(&self) -> String {
        match self {
            Self::RunWorkflow(id) => format!("wf:{id}"),
            Self::StopRecording => "stop-recording".to_owned(),
            Self::CancelCurrent => "cancel-current".to_owned(),
            Self::UploadFiles => "upload-files".to_owned(),
            Self::OpenEditor => "open-editor".to_owned(),
            Self::OpenHistory => "open-history".to_owned(),
            Self::OpenSettings => "open-settings".to_owned(),
            Self::ToggleHotkeys => "toggle-hotkeys".to_owned(),
            Self::OpenCapturesFolder => "open-captures-folder".to_owned(),
            Self::Quit => "quit".to_owned(),
        }
    }

    /// The inverse of [`id`](Self::id).
    pub fn from_id(id: &str) -> Option<Self> {
        if let Some(wf) = id.strip_prefix("wf:") {
            return (!wf.is_empty()).then(|| Self::RunWorkflow(wf.to_owned()));
        }
        Some(match id {
            "stop-recording" => Self::StopRecording,
            "cancel-current" => Self::CancelCurrent,
            "upload-files" => Self::UploadFiles,
            "open-editor" => Self::OpenEditor,
            "open-history" => Self::OpenHistory,
            "open-settings" => Self::OpenSettings,
            "toggle-hotkeys" => Self::ToggleHotkeys,
            "open-captures-folder" => Self::OpenCapturesFolder,
            "quit" => Self::Quit,
            _ => return None,
        })
    }
}

/// One clickable entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What it does.
    pub action: Action,
    /// Text.
    pub label: String,
    /// The hotkey shown beside the label (informational).
    pub shortcut: Option<String>,
    /// Clickable.
    pub enabled: bool,
    /// `Some` for check items.
    pub checked: Option<bool>,
}

/// A menu row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A clickable entry.
    Entry(Entry),
    /// A line.
    Separator,
    /// A non-clickable heading or status line.
    Header(String),
}

/// The whole menu.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Menu {
    /// Rows from top to bottom.
    pub items: Vec<Item>,
}

impl Menu {
    /// Every clickable entry, in order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.items.iter().filter_map(|i| if let Item::Entry(e) = i { Some(e) } else { None })
    }

    /// The entry for `action`.
    pub fn entry(&self, action: &Action) -> Option<&Entry> {
        self.entries().find(|e| &e.action == action)
    }
}

/// How hotkeys stand.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HotkeyView {
    /// The user has hotkeys switched on (the tray's toggle).
    pub enabled: bool,
    /// The mechanism in use (`None`: unavailable here, so the menu says so).
    pub backend: Option<String>,
    /// How many could not be registered.
    pub problems: usize,
}

/// What the tray needs to know about the daemon besides the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiState {
    /// The recording.
    pub recording: RecordingView,
    /// Something interactive (overlay, editor) is open.
    pub interactive_open: bool,
    /// The step of the current run, if any ("Uploading...").
    pub busy: Option<String>,
    /// Runs waiting for a slot.
    pub queued: usize,
    /// The newest run failed (tooltip and icon show it until the next run starts).
    pub last_error: Option<String>,
    /// The overlay helper was found (region capture works).
    pub overlay_available: bool,
    /// This build can record.
    pub record_supported: bool,
    /// The settings window helper exists (otherwise Settings opens the file).
    pub settings_ui_available: bool,
    /// Hotkeys.
    pub hotkeys: HotkeyView,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            recording: RecordingView::Idle,
            interactive_open: false,
            busy: None,
            queued: 0,
            last_error: None,
            overlay_available: true,
            record_supported: true,
            settings_ui_available: true,
            hotkeys: HotkeyView {
                enabled: true,
                backend: Some("global-hotkey".into()),
                problems: 0,
            },
        }
    }
}

fn shortcut(wf: &Workflow, hotkeys_on: bool) -> Option<String> {
    wf.trigger.hotkey.clone().filter(|h| hotkeys_on && !h.trim().is_empty())
}

fn needs_overlay(wf: &Workflow) -> bool {
    wf.input == InputKind::CaptureRegion
}

/// Builds the menu. See the module docs.
pub fn build_menu(settings: &Settings, ui: &UiState) -> Menu {
    let mut items = Vec::new();
    let hotkeys_on = ui.hotkeys.enabled && ui.hotkeys.backend.is_some();
    let recording = ui.recording.is_active();
    let sep = |items: &mut Vec<Item>| {
        if !matches!(items.last(), None | Some(Item::Separator)) {
            items.push(Item::Separator);
        }
    };

    // 1. Still captures and clipboard uploads.
    for wf in settings
        .workflows
        .iter()
        .filter(|w| w.input.is_still_capture() || w.input == InputKind::Clipboard)
    {
        let mut label = wf.name.clone();
        let mut enabled = true;
        if needs_overlay(wf) && !ui.overlay_available {
            label.push_str(" (overlay not found)");
            enabled = false;
        } else if needs_overlay(wf) && ui.interactive_open {
            enabled = false;
        }
        items.push(Item::Entry(Entry {
            action: Action::RunWorkflow(wf.id.clone()),
            label,
            shortcut: shortcut(wf, hotkeys_on),
            enabled,
            checked: None,
        }));
    }

    // 2. Recording.
    sep(&mut items);
    match ui.recording {
        RecordingView::Idle => {
            for wf in settings.workflows.iter().filter(|w| w.input.is_recording()) {
                let mut label = wf.name.clone();
                let mut enabled = true;
                if !ui.record_supported {
                    label.push_str(" (not available in this build)");
                    enabled = false;
                } else if ui.interactive_open {
                    enabled = false;
                }
                items.push(Item::Entry(Entry {
                    action: Action::RunWorkflow(wf.id.clone()),
                    label,
                    shortcut: shortcut(wf, hotkeys_on),
                    enabled,
                    checked: None,
                }));
            }
        }
        RecordingView::Selecting => {
            items.push(Item::Header("Choosing what to record...".to_owned()));
            items.push(Item::Entry(Entry {
                action: Action::StopRecording,
                label: "Cancel recording".to_owned(),
                shortcut: None,
                enabled: true,
                checked: None,
            }));
        }
        RecordingView::Recording { elapsed } => {
            items.push(Item::Entry(Entry {
                action: Action::StopRecording,
                label: format!("Stop recording ({})", format_elapsed(elapsed)),
                shortcut: settings
                    .workflows
                    .iter()
                    .filter(|w| w.input.is_recording())
                    .find_map(|w| shortcut(w, hotkeys_on)),
                enabled: true,
                checked: None,
            }));
        }
        RecordingView::Stopping => {
            items.push(Item::Header("Finishing the recording...".to_owned()));
        }
    }

    // 3. Files, editor.
    sep(&mut items);
    let file_workflows: Vec<&Workflow> =
        settings.workflows.iter().filter(|w| w.input == InputKind::Files).collect();
    if file_workflows.is_empty() {
        items.push(Item::Entry(Entry {
            action: Action::UploadFiles,
            label: "Upload files...".to_owned(),
            shortcut: None,
            enabled: true,
            checked: None,
        }));
    } else {
        for wf in file_workflows {
            let label = if wf.id == "upload-files" {
                "Upload files...".to_owned()
            } else {
                format!("{}...", wf.name.trim_end_matches("..."))
            };
            items.push(Item::Entry(Entry {
                action: if wf.id == "upload-files" {
                    Action::UploadFiles
                } else {
                    Action::RunWorkflow(wf.id.clone())
                },
                label,
                shortcut: shortcut(wf, hotkeys_on),
                enabled: true,
                checked: None,
            }));
        }
    }
    items.push(Item::Entry(Entry {
        action: Action::OpenEditor,
        label: "Open editor".to_owned(),
        shortcut: None,
        enabled: !ui.interactive_open,
        checked: None,
    }));

    // 4. Windows and toggles.
    sep(&mut items);
    items.push(Item::Entry(Entry {
        action: Action::OpenHistory,
        label: "History".to_owned(),
        shortcut: settings.hotkeys.open_history.clone().filter(|_| hotkeys_on),
        enabled: true,
        checked: None,
    }));
    items.push(Item::Entry(Entry {
        action: Action::OpenSettings,
        label: if ui.settings_ui_available { "Settings" } else { "Settings (edit the file)" }
            .to_owned(),
        shortcut: settings.hotkeys.open_settings.clone().filter(|_| hotkeys_on),
        enabled: true,
        checked: None,
    }));
    let (hk_label, hk_enabled) = match (&ui.hotkeys.backend, ui.hotkeys.enabled) {
        (None, _) => ("Hotkeys: not available here (see ssx hotkeys install)".to_owned(), false),
        (Some(_), true) if ui.hotkeys.problems > 0 => {
            (format!("Hotkeys on ({} could not be registered)", ui.hotkeys.problems), true)
        }
        (Some(_), true) => ("Hotkeys on".to_owned(), true),
        (Some(_), false) => ("Hotkeys off".to_owned(), true),
    };
    items.push(Item::Entry(Entry {
        action: Action::ToggleHotkeys,
        label: hk_label,
        shortcut: None,
        enabled: hk_enabled,
        checked: ui.hotkeys.backend.is_some().then_some(ui.hotkeys.enabled),
    }));
    if ui.interactive_open && !recording {
        items.push(Item::Entry(Entry {
            action: Action::CancelCurrent,
            label: "Cancel current capture".to_owned(),
            shortcut: None,
            enabled: true,
            checked: None,
        }));
    }
    items.push(Item::Entry(Entry {
        action: Action::OpenCapturesFolder,
        label: "Open captures folder".to_owned(),
        shortcut: None,
        enabled: true,
        checked: None,
    }));

    // 5. Quit.
    sep(&mut items);
    items.push(Item::Entry(Entry {
        action: Action::Quit,
        label: "Quit".to_owned(),
        shortcut: None,
        enabled: true,
        checked: None,
    }));
    Menu { items }
}

/// The tooltip / title: a single line saying what the daemon is doing.
pub fn tooltip(ui: &UiState) -> String {
    let core = match ui.recording {
        RecordingView::Recording { elapsed } => {
            format!("Recording {}", format_elapsed(elapsed))
        }
        RecordingView::Selecting => "Choosing what to record".to_owned(),
        RecordingView::Stopping => "Finishing the recording".to_owned(),
        RecordingView::Idle => match (&ui.busy, ui.interactive_open, &ui.last_error) {
            (Some(step), ..) => step.clone(),
            (None, true, _) => "Waiting for you".to_owned(),
            (None, false, Some(e)) => format!("Last run failed: {e}"),
            (None, false, None) => "Ready".to_owned(),
        },
    };
    let queued = if ui.queued > 0 { format!(" ({} waiting)", ui.queued) } else { String::new() };
    format!("ssx: {core}{queued}")
}

/// Which icon the tray shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconKind {
    /// Ready.
    Idle,
    /// Recording (red).
    Recording,
    /// The newest run failed.
    Error,
}

/// The icon for a state: recording wins over an old error.
pub fn icon_kind(ui: &UiState) -> IconKind {
    if ui.recording.is_active() {
        IconKind::Recording
    } else if ui.last_error.is_some() {
        IconKind::Error
    } else {
        IconKind::Idle
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ssx_core::settings::{AfterCapture, Trigger};

    use super::*;

    fn labels(m: &Menu) -> Vec<String> {
        m.items
            .iter()
            .map(|i| match i {
                Item::Entry(e) => e.label.clone(),
                Item::Separator => "---".to_owned(),
                Item::Header(h) => format!("# {h}"),
            })
            .collect()
    }

    #[test]
    fn the_default_menu_lists_every_workflow_with_its_hotkey() {
        let s = Settings::default();
        let m = build_menu(&s, &UiState::default());
        let region = m.entry(&Action::RunWorkflow("capture-region".into())).unwrap();
        assert_eq!(region.label, "Capture region, save, copy and upload");
        assert_eq!(region.shortcut.as_deref(), Some("Ctrl+PrintScreen"));
        assert!(region.enabled);
        let monitor = m.entry(&Action::RunWorkflow("capture-monitor".into())).unwrap();
        assert_eq!(monitor.shortcut, None, "no hotkey, no shortcut text");
        let rec = m.entry(&Action::RunWorkflow("record-screen".into())).unwrap();
        assert_eq!(rec.shortcut.as_deref(), Some("Shift+PrintScreen"));
        assert!(
            m.entry(&Action::UploadFiles).is_some(),
            "the files workflow becomes Upload files..."
        );
        assert!(
            m.entry(&Action::RunWorkflow("upload-files".into())).is_none(),
            "and does not appear twice"
        );
        for a in [
            Action::OpenEditor,
            Action::OpenHistory,
            Action::OpenSettings,
            Action::ToggleHotkeys,
            Action::OpenCapturesFolder,
            Action::Quit,
        ] {
            assert!(m.entry(&a).is_some(), "{a:?}");
        }
        assert!(m.entry(&Action::StopRecording).is_none());
        assert!(m.entry(&Action::CancelCurrent).is_none(), "nothing to cancel while idle");
        assert!(matches!(m.items.last(), Some(Item::Entry(e)) if e.action == Action::Quit));
    }

    #[test]
    fn separators_never_double_up_or_lead() {
        let mut s = Settings::default();
        s.workflows.clear();
        let m = build_menu(&s, &UiState::default());
        assert!(!matches!(m.items.first(), Some(Item::Separator)));
        for w in m.items.windows(2) {
            assert!(!(w[0] == Item::Separator && w[1] == Item::Separator), "{:?}", labels(&m));
        }
    }

    #[test]
    fn recording_state_replaces_the_record_entries_with_stop() {
        let s = Settings::default();
        let mut ui = UiState {
            recording: RecordingView::Recording { elapsed: Duration::from_secs(75) },
            ..UiState::default()
        };
        let m = build_menu(&s, &ui);
        let stop = m.entry(&Action::StopRecording).unwrap();
        assert_eq!(stop.label, "Stop recording (1:15)");
        assert_eq!(stop.shortcut.as_deref(), Some("Shift+PrintScreen"), "the same hotkey stops it");
        assert!(m.entry(&Action::RunWorkflow("record-screen".into())).is_none());
        assert!(m.entry(&Action::RunWorkflow("record-gif".into())).is_none());
        // Captures stay available while recording.
        assert!(m.entry(&Action::RunWorkflow("capture-fullscreen".into())).unwrap().enabled);

        ui.recording = RecordingView::Selecting;
        let m = build_menu(&s, &ui);
        assert_eq!(m.entry(&Action::StopRecording).unwrap().label, "Cancel recording");
        assert!(labels(&m).iter().any(|l| l.starts_with("# Choosing")));

        ui.recording = RecordingView::Stopping;
        let m = build_menu(&s, &ui);
        assert!(m.entry(&Action::StopRecording).is_none(), "already stopping");
        assert!(labels(&m).iter().any(|l| l.contains("Finishing")));
    }

    #[test]
    fn hotkey_text_disappears_when_hotkeys_are_off_or_unavailable() {
        let s = Settings::default();
        for hk in [
            HotkeyView { enabled: false, backend: Some("global-hotkey".into()), problems: 0 },
            HotkeyView { enabled: true, backend: None, problems: 0 },
        ] {
            let m = build_menu(&s, &UiState { hotkeys: hk, ..UiState::default() });
            assert!(m.entries().all(|e| e.shortcut.is_none()), "{:?}", labels(&m));
        }
    }

    #[test]
    fn the_hotkey_toggle_says_what_is_going_on() {
        let s = Settings::default();
        let toggle = |hk: HotkeyView| {
            let m = build_menu(&s, &UiState { hotkeys: hk, ..UiState::default() });
            m.entry(&Action::ToggleHotkeys).unwrap().clone()
        };
        let on = toggle(HotkeyView { enabled: true, backend: Some("x".into()), problems: 0 });
        assert_eq!((on.label.as_str(), on.checked, on.enabled), ("Hotkeys on", Some(true), true));
        let off = toggle(HotkeyView { enabled: false, backend: Some("x".into()), problems: 0 });
        assert_eq!((off.label.as_str(), off.checked), ("Hotkeys off", Some(false)));
        let some_failed =
            toggle(HotkeyView { enabled: true, backend: Some("x".into()), problems: 2 });
        assert!(some_failed.label.contains("2 could not be registered"));
        let none = toggle(HotkeyView { enabled: true, backend: None, problems: 0 });
        assert!(!none.enabled && none.checked.is_none());
        assert!(none.label.contains("ssx hotkeys install"));
    }

    #[test]
    fn region_entries_are_disabled_with_a_reason_when_the_overlay_is_missing() {
        let s = Settings::default();
        let m = build_menu(&s, &UiState { overlay_available: false, ..UiState::default() });
        let region = m.entry(&Action::RunWorkflow("capture-region".into())).unwrap();
        assert!(!region.enabled);
        assert!(region.label.ends_with("(overlay not found)"), "{}", region.label);
        assert!(m.entry(&Action::RunWorkflow("capture-fullscreen".into())).unwrap().enabled);
    }

    #[test]
    fn builds_without_recording_support_grey_out_the_record_entries() {
        let s = Settings::default();
        let m = build_menu(&s, &UiState { record_supported: false, ..UiState::default() });
        let rec = m.entry(&Action::RunWorkflow("record-screen".into())).unwrap();
        assert!(!rec.enabled && rec.label.contains("not available in this build"));
    }

    #[test]
    fn an_open_overlay_disables_what_needs_it_and_offers_cancel() {
        let s = Settings::default();
        let m = build_menu(&s, &UiState { interactive_open: true, ..UiState::default() });
        assert!(!m.entry(&Action::RunWorkflow("capture-region".into())).unwrap().enabled);
        assert!(!m.entry(&Action::RunWorkflow("record-screen".into())).unwrap().enabled);
        assert!(!m.entry(&Action::OpenEditor).unwrap().enabled);
        assert!(m.entry(&Action::RunWorkflow("capture-fullscreen".into())).unwrap().enabled);
        assert!(m.entry(&Action::CancelCurrent).unwrap().enabled);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn custom_workflows_show_up_and_hotkeys_with_odd_values_are_handled() {
        let mut s = Settings::default();
        s.workflows = vec![
            Workflow {
                id: "mine".into(),
                name: "My capture".into(),
                trigger: Trigger { hotkey: Some("Ctrl+Alt+M".into()), cli_name: None },
                input: InputKind::CaptureWindow,
                after_capture: vec![AfterCapture::SaveToFile],
                ..Workflow::default()
            },
            Workflow {
                id: "blank".into(),
                name: "Blank hotkey".into(),
                trigger: Trigger { hotkey: Some("  ".into()), cli_name: None },
                input: InputKind::CaptureFullscreen,
                ..Workflow::default()
            },
            Workflow {
                id: "bulk".into(),
                name: "Upload to my server".into(),
                input: InputKind::Files,
                ..Workflow::default()
            },
        ];
        let m = build_menu(&s, &UiState::default());
        assert_eq!(
            m.entry(&Action::RunWorkflow("mine".into())).unwrap().shortcut.as_deref(),
            Some("Ctrl+Alt+M")
        );
        assert_eq!(m.entry(&Action::RunWorkflow("blank".into())).unwrap().shortcut, None);
        let bulk = m.entry(&Action::RunWorkflow("bulk".into())).unwrap();
        assert_eq!(bulk.label, "Upload to my server...");
        assert!(
            m.entry(&Action::UploadFiles).is_none(),
            "no default file workflow, no default entry"
        );
    }

    #[test]
    fn the_settings_entry_says_when_it_only_opens_the_file() {
        let s = Settings::default();
        let m = build_menu(&s, &UiState { settings_ui_available: false, ..UiState::default() });
        assert_eq!(m.entry(&Action::OpenSettings).unwrap().label, "Settings (edit the file)");
    }

    #[test]
    fn action_ids_round_trip_and_reject_garbage() {
        let all = [
            Action::RunWorkflow("capture-region".into()),
            Action::StopRecording,
            Action::CancelCurrent,
            Action::UploadFiles,
            Action::OpenEditor,
            Action::OpenHistory,
            Action::OpenSettings,
            Action::ToggleHotkeys,
            Action::OpenCapturesFolder,
            Action::Quit,
        ];
        for a in all {
            assert_eq!(Action::from_id(&a.id()), Some(a.clone()), "{a:?}");
        }
        for bad in ["", "wf:", "nope", "WF:x", "quit "] {
            assert_eq!(Action::from_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn menu_ids_are_unique() {
        let m = build_menu(&Settings::default(), &UiState::default());
        let mut ids: Vec<String> = m.entries().map(|e| e.action.id()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n);
    }

    #[test]
    fn tooltips_and_icons_follow_the_state() {
        let mut ui = UiState::default();
        assert_eq!(tooltip(&ui), "ssx: Ready");
        assert_eq!(icon_kind(&ui), IconKind::Idle);
        ui.busy = Some("Uploading...".into());
        ui.queued = 2;
        assert_eq!(tooltip(&ui), "ssx: Uploading... (2 waiting)");
        ui.busy = None;
        ui.queued = 0;
        ui.interactive_open = true;
        assert_eq!(tooltip(&ui), "ssx: Waiting for you");
        ui.interactive_open = false;
        ui.last_error = Some("upload: HTTP 500".into());
        assert_eq!(tooltip(&ui), "ssx: Last run failed: upload: HTTP 500");
        assert_eq!(icon_kind(&ui), IconKind::Error);
        ui.recording = RecordingView::Recording { elapsed: Duration::from_secs(5) };
        assert_eq!(tooltip(&ui), "ssx: Recording 0:05");
        assert_eq!(icon_kind(&ui), IconKind::Recording, "recording wins over an old error");
        ui.recording = RecordingView::Selecting;
        assert_eq!(tooltip(&ui), "ssx: Choosing what to record");
        ui.recording = RecordingView::Stopping;
        assert_eq!(tooltip(&ui), "ssx: Finishing the recording");
    }

    #[test]
    fn the_menu_is_deterministic() {
        let s = Settings::default();
        let ui = UiState::default();
        assert_eq!(build_menu(&s, &ui), build_menu(&s, &ui));
    }
}
