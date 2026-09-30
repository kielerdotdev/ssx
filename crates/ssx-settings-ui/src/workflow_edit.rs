//! Editing operations on the list of workflows: create, duplicate, delete, templates, the
//! steps a workflow may still be given, and names for everything.
//!
//! All of it works on plain `ssx_core::settings::Workflow` values, so the rules that keep the
//! settings valid (unique ids and command names, no duplicate hotkeys, no image-only steps on
//! recordings) are unit tested here instead of being buried in widget code.

use ssx_core::settings::{
    AfterCapture, AfterUpload, DestinationType, InputKind, Workflow, builtin_workflows,
};

/// Longest id the validator accepts.
const MAX_ID: usize = 64;
/// Longest command name the validator accepts.
const MAX_CLI: usize = 32;

/// `name` reduced to `[a-z0-9._-]`, at most `max` characters; `fallback` if nothing is left.
pub fn slugify(name: &str, max: usize, fallback: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches(['-', '.', '_']);
    let mut s: String = trimmed.chars().take(max).collect();
    s = s.trim_end_matches(['-', '.', '_']).to_owned();
    if s.is_empty() { fallback.to_owned() } else { s }
}

fn unique(base: &str, max: usize, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    (2..)
        .map(|n: usize| {
            let suffix = format!("-{n}");
            let keep = max.saturating_sub(suffix.len());
            let stem: String = base.chars().take(keep).collect();
            format!("{}{suffix}", stem.trim_end_matches('-'))
        })
        .find(|cand| !taken(cand))
        .unwrap_or_else(|| base.to_owned())
}

/// A workflow id derived from `base` that no workflow in `existing` has.
pub fn unique_id(base: &str, existing: &[Workflow]) -> String {
    let base = slugify(base, MAX_ID, "workflow");
    unique(&base, MAX_ID, &|c| existing.iter().any(|w| w.id == c))
}

/// A command name derived from `base` (lower-case letters, digits, `-`; at most 32
/// characters) that no workflow in `existing` uses.
pub fn unique_cli(base: &str, existing: &[Workflow]) -> String {
    let slug: String = slugify(base, MAX_CLI, "workflow")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    unique(&slug, MAX_CLI, &|c| existing.iter().any(|w| w.trigger.cli_name.as_deref() == Some(c)))
}

/// A display name derived from `base` that no workflow in `existing` has (`X`, `X (2)`, ...).
pub fn unique_name(base: &str, existing: &[Workflow]) -> String {
    if !existing.iter().any(|w| w.name == base) {
        return base.to_owned();
    }
    (2..)
        .map(|n| format!("{base} ({n})"))
        .find(|c| !existing.iter().any(|w| &w.name == c))
        .unwrap_or_else(|| base.to_owned())
}

/// A template offered by "New from template".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// What the menu shows.
    pub label: String,
    /// The workflow it creates.
    pub workflow: Workflow,
}

/// The blank template plus the built-in workflows.
pub fn templates() -> Vec<Template> {
    let mut v = vec![Template {
        label: "Blank: capture a region and save it".to_owned(),
        workflow: Workflow {
            id: "new-workflow".into(),
            name: "New workflow".into(),
            input: InputKind::CaptureRegion,
            after_capture: vec![AfterCapture::SaveToFile],
            ..Workflow::default()
        },
    }];
    v.extend(builtin_workflows().into_iter().map(|w| Template { label: w.name.clone(), workflow: w }));
    v
}

/// Creates a workflow from `template` that is valid next to `existing`: fresh id, command
/// name and display name, and no hotkey (a copied hotkey would be a duplicate).
pub fn instantiate(template: &Workflow, existing: &[Workflow]) -> Workflow {
    let mut w = template.clone();
    w.id = unique_id(&template.id, existing);
    w.name = unique_name(&template.name, existing);
    w.trigger.hotkey = None;
    w.trigger.cli_name = template
        .trigger
        .cli_name
        .as_deref()
        .map(|c| unique_cli(c, existing))
        .or_else(|| Some(unique_cli(&w.id, existing)));
    w
}

/// Inserts a copy of `list[index]` right after it and returns the copy's index.
pub fn duplicate(list: &mut Vec<Workflow>, index: usize) -> Option<usize> {
    let src = list.get(index)?.clone();
    let mut copy = src.clone();
    copy.id = unique_id(&format!("{}-copy", src.id), list);
    copy.name = unique_name(&format!("{} (copy)", src.name), list);
    copy.trigger.hotkey = None;
    copy.trigger.cli_name = src
        .trigger
        .cli_name
        .as_deref()
        .map(|c| unique_cli(&format!("{c}-copy"), list));
    list.insert(index + 1, copy);
    Some(index + 1)
}

/// Removes `list[index]`. Returns the index that should be selected afterwards.
pub fn delete(list: &mut Vec<Workflow>, index: usize) -> Option<usize> {
    if index >= list.len() {
        return None;
    }
    list.remove(index);
    if list.is_empty() { None } else { Some(index.min(list.len() - 1)) }
}

/// The other workflows (by index) that have the same command name as `list[index]`.
pub fn others_with_cli(list: &[Workflow], index: usize) -> Vec<usize> {
    let Some(mine) = list.get(index).and_then(|w| w.trigger.cli_name.as_deref()).filter(|c| !c.is_empty()) else {
        return Vec::new();
    };
    (0..list.len()).filter(|i| *i != index && list[*i].trigger.cli_name.as_deref() == Some(mine)).collect()
}

/// The other workflows (by index) that have the same id as `list[index]`.
pub fn others_with_id(list: &[Workflow], index: usize) -> Vec<usize> {
    let Some(mine) = list.get(index).map(|w| w.id.as_str()).filter(|c| !c.is_empty()) else {
        return Vec::new();
    };
    (0..list.len()).filter(|i| *i != index && list[*i].id == mine).collect()
}

// ---- names and steps ----------------------------------------------------------------------

/// Every input kind, with the words shown in the UI.
pub const INPUT_KINDS: [(InputKind, &str, &str); 9] = [
    (InputKind::CaptureRegion, "Capture region", "Draw a rectangle on a frozen screen."),
    (InputKind::CaptureFullscreen, "Capture screen", "The whole virtual desktop."),
    (InputKind::CaptureMonitor, "Capture monitor", "The monitor under the mouse cursor."),
    (InputKind::CaptureWindow, "Capture active window", "The window that has focus."),
    (InputKind::CaptureLastRegion, "Repeat last region", "The previous region, without the overlay."),
    (InputKind::RecordScreen, "Record screen", "Screen recording to a video file; the hotkey starts and stops."),
    (InputKind::RecordGif, "Record GIF", "Screen recording to an animated GIF."),
    (InputKind::Files, "Files", "Files you pass in (right-click menu, `ssx post-file`, drag and drop)."),
    (InputKind::Clipboard, "Clipboard", "Whatever is on the clipboard: image, text or files."),
];

/// The label of an input kind.
pub fn input_label(k: InputKind) -> &'static str {
    INPUT_KINDS.iter().find(|(i, _, _)| *i == k).map_or("?", |(_, l, _)| l)
}

/// The one-line description of an input kind.
pub fn input_blurb(k: InputKind) -> &'static str {
    INPUT_KINDS.iter().find(|(i, _, _)| *i == k).map_or("", |(_, _, d)| d)
}

/// Every after-capture step with its label and description.
pub const CAPTURE_STEPS: [(AfterCapture, &str, &str); 8] = [
    (AfterCapture::OpenEditor, "Open editor", "Edit the image first; closing the editor cancels the workflow."),
    (AfterCapture::CopyImageToClipboard, "Copy image to clipboard", "Put the image on the clipboard."),
    (AfterCapture::SaveToFile, "Save to file", "Save into the save folder using the file name pattern."),
    (AfterCapture::SaveAsDialog, "Ask where to save", "Show a Save As dialog."),
    (AfterCapture::PinToScreen, "Pin to screen", "Keep the image in an always-on-top window."),
    (AfterCapture::Ocr, "Recognise text (OCR)", "Copy the text found in the image."),
    (AfterCapture::Upload, "Upload", "Send it to the destination for its type."),
    (AfterCapture::DeleteLocalFile, "Delete local file", "Remove the saved file, but only after a confirmed upload."),
];

/// The label of an after-capture step.
pub fn capture_step_label(s: AfterCapture) -> &'static str {
    CAPTURE_STEPS.iter().find(|(x, _, _)| *x == s).map_or("?", |(_, l, _)| l)
}

/// The description of an after-capture step.
pub fn capture_step_blurb(s: AfterCapture) -> &'static str {
    CAPTURE_STEPS.iter().find(|(x, _, _)| *x == s).map_or("", |(_, _, d)| d)
}

/// `true` for steps that only make sense for images.
pub const fn image_only(s: AfterCapture) -> bool {
    matches!(
        s,
        AfterCapture::OpenEditor
            | AfterCapture::CopyImageToClipboard
            | AfterCapture::PinToScreen
            | AfterCapture::Ocr
    )
}

/// After-upload steps that can be added (RunCommand starts empty).
pub fn upload_step_kinds() -> [AfterUpload; 7] {
    [
        AfterUpload::CopyUrl,
        AfterUpload::CopyShortUrl,
        AfterUpload::OpenUrl,
        AfterUpload::ShortenUrl,
        AfterUpload::ShowQrCode,
        AfterUpload::ShowNotification,
        AfterUpload::RunCommand { program: String::new(), args: Vec::new() },
    ]
}

/// The label of an after-upload step.
pub fn upload_step_label(s: &AfterUpload) -> &'static str {
    match s {
        AfterUpload::CopyUrl => "Copy URL",
        AfterUpload::CopyShortUrl => "Copy short URL",
        AfterUpload::OpenUrl => "Open URL in browser",
        AfterUpload::ShortenUrl => "Shorten URL",
        AfterUpload::ShowQrCode => "Show QR code",
        AfterUpload::ShowNotification => "Show notification",
        AfterUpload::RunCommand { .. } => "Run command",
    }
}

/// The description of an after-upload step.
pub fn upload_step_blurb(s: &AfterUpload) -> &'static str {
    match s {
        AfterUpload::CopyUrl => "Copy the current URL (the short one after Shorten URL).",
        AfterUpload::CopyShortUrl => "Copy a shortened URL, shortening first if needed.",
        AfterUpload::OpenUrl => "Open the URL in the default browser.",
        AfterUpload::ShortenUrl => "Later steps then see the short URL.",
        AfterUpload::ShowQrCode => "Show a QR code for the URL.",
        AfterUpload::ShowNotification => "Tell you the result in a desktop notification.",
        AfterUpload::RunCommand { .. } => "Run a program; no shell is involved.",
    }
}

/// The placeholders a `run_command` argument may contain.
pub const RUN_COMMAND_PLACEHOLDERS: [(&str, &str); 6] = [
    ("{path}", "the local file"),
    ("{file_name}", "its name"),
    ("{url}", "the uploaded URL"),
    ("{short_url}", "the shortened URL"),
    ("{thumbnail_url}", "the thumbnail URL"),
    ("{deletion_url}", "the URL that deletes the upload"),
];

/// The after-capture steps that can still be added to `w` (not present yet, and valid for
/// its input).
pub fn addable_capture_steps(w: &Workflow) -> Vec<AfterCapture> {
    CAPTURE_STEPS
        .iter()
        .map(|(s, _, _)| *s)
        .filter(|s| !w.after_capture.contains(s))
        .filter(|s| !(w.input.is_recording() && image_only(*s)))
        .collect()
}

/// The after-upload steps that can still be added (`run_command` can be added repeatedly).
pub fn addable_upload_steps(w: &Workflow) -> Vec<AfterUpload> {
    upload_step_kinds()
        .into_iter()
        .filter(|s| matches!(s, AfterUpload::RunCommand { .. }) || !w.after_upload.contains(s))
        .collect()
}

/// Changes the input of `w` and drops the steps that cannot work for the new input (image
/// steps on a recording). Returns what was dropped, for a message.
pub fn set_input(w: &mut Workflow, input: InputKind) -> Vec<AfterCapture> {
    w.input = input;
    let mut dropped = Vec::new();
    if input.is_recording() {
        w.after_capture.retain(|s| {
            if image_only(*s) {
                dropped.push(*s);
                false
            } else {
                true
            }
        });
    }
    dropped
}

/// The destination types a workflow can override, with labels and whether the value is
/// special.
pub const DESTINATION_ROWS: [(DestinationType, &str, &str); 6] = [
    (DestinationType::Image, "Image", "Screenshots and images."),
    (DestinationType::Text, "Text", "Text snippets, OCR results."),
    (DestinationType::File, "File", "Any other file."),
    (
        DestinationType::Video,
        "Video",
        "Recordings. When unset, videos go to the File destination.",
    ),
    (DestinationType::UrlShortener, "URL shortener", "Used by the Shorten URL steps."),
    (DestinationType::UrlSharing, "URL sharing", "Posts the link to a service."),
];

/// The label of a destination type.
pub fn destination_label(t: DestinationType) -> &'static str {
    DESTINATION_ROWS.iter().find(|(d, _, _)| *d == t).map_or("?", |(_, l, _)| l)
}

/// Sets the override for `ty` (`None` or a blank name clears it).
pub fn set_destination(w: &mut Workflow, ty: DestinationType, name: Option<String>) {
    let name = name.filter(|n| !n.trim().is_empty());
    let d = &mut w.destination;
    match ty {
        DestinationType::Image => d.image = name,
        DestinationType::Text => d.text = name,
        DestinationType::File => d.file = name,
        DestinationType::Video => d.video = name,
        DestinationType::UrlShortener => d.url_shortener = name,
        DestinationType::UrlSharing => d.url_sharing = name,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ssx_core::settings::Settings;

    use super::*;

    #[test]
    fn slugs_are_valid_ids() {
        assert_eq!(slugify("Capture Region & Upload!", 64, "x"), "capture-region-upload");
        assert_eq!(slugify("  ", 64, "fallback"), "fallback");
        assert_eq!(slugify("---", 64, "fallback"), "fallback");
        assert_eq!(slugify("Ünïcode name", 64, "x"), "n-code-name");
        assert_eq!(slugify("a".repeat(100).as_str(), 10, "x").len(), 10);
        assert_eq!(slugify("Trailing-", 64, "x"), "trailing");
        assert_eq!(slugify("v1.2", 64, "x"), "v1-2");
    }

    #[test]
    fn unique_ids_count_up_within_the_limit() {
        let mut list = vec![];
        for _ in 0..4 {
            let mut w = Workflow::default();
            w.id = unique_id("shot", &list);
            list.push(w);
        }
        let ids: Vec<&str> = list.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["shot", "shot-2", "shot-3", "shot-4"]);
        let long = "x".repeat(64);
        let w = Workflow { id: long.clone(), ..Workflow::default() };
        let id = unique_id(&long, std::slice::from_ref(&w));
        assert!(id.len() <= 64 && id != long && id.ends_with("-2"), "{id}");
    }

    #[test]
    fn unique_cli_names_follow_the_validator_rules() {
        let w = Workflow {
            trigger: ssx_core::settings::Trigger { cli_name: Some("region".into()), hotkey: None },
            ..Workflow::default()
        };
        assert_eq!(unique_cli("region", std::slice::from_ref(&w)), "region-2");
        assert_eq!(unique_cli("Upload Files", &[]), "upload-files");
        assert_eq!(unique_cli("a.b_c", &[]), "a-b-c");
        assert!(unique_cli(&"y".repeat(80), &[]).len() <= 32);
    }

    #[test]
    fn duplicates_of_the_id_and_the_command_name_are_found_from_either_side() {
        let list = builtin_workflows();
        assert!(others_with_cli(&list, 0).is_empty());
        assert!(others_with_id(&list, 0).is_empty());
        let mut list = list;
        list[3].trigger.cli_name = Some("region".into());
        list[5].id = list[1].id.clone();
        assert_eq!(others_with_cli(&list, 0), [3]);
        assert_eq!(others_with_cli(&list, 3), [0]);
        assert_eq!(others_with_id(&list, 1), [5]);
        assert_eq!(others_with_id(&list, 5), [1]);
        list[0].trigger.cli_name = None;
        assert!(others_with_cli(&list, 0).is_empty());
        assert!(others_with_cli(&list, 99).is_empty());
    }

    #[test]
    fn unique_names() {
        let w = Workflow { name: "Shot".into(), ..Workflow::default() };
        assert_eq!(unique_name("Shot", std::slice::from_ref(&w)), "Shot (2)");
        assert_eq!(unique_name("Other", std::slice::from_ref(&w)), "Other");
    }

    #[test]
    fn templates_include_blank_and_every_builtin() {
        let t = templates();
        assert_eq!(t.len(), 1 + builtin_workflows().len());
        assert!(t[0].label.starts_with("Blank"));
        assert!(t.iter().skip(1).all(|x| !x.label.is_empty()));
    }

    #[test]
    fn instantiating_a_template_keeps_the_settings_valid() {
        let mut s = Settings::default();
        for t in templates() {
            let w = instantiate(&t.workflow, &s.workflows);
            assert!(w.trigger.hotkey.is_none());
            s.workflows.push(w);
        }
        // add the whole set a second time: still no id / command / name clash, no hotkey clash
        for t in templates() {
            let w = instantiate(&t.workflow, &s.workflows);
            s.workflows.push(w);
        }
        let errs: Vec<_> = s.validate().into_iter().filter(|i| i.severity == ssx_core::settings::Severity::Error).collect();
        assert!(errs.is_empty(), "{errs:#?}");
    }

    #[test]
    fn duplicate_inserts_after_the_source_without_stealing_the_hotkey() {
        let mut list = builtin_workflows();
        let n = list.len();
        let idx = duplicate(&mut list, 1).unwrap();
        assert_eq!(idx, 2);
        assert_eq!(list.len(), n + 1);
        assert_eq!(list[2].id, "capture-region-edit-copy");
        assert!(list[2].name.ends_with("(copy)"));
        assert_eq!(list[2].trigger.hotkey, None);
        assert_eq!(list[2].after_capture, list[1].after_capture);
        assert_eq!(list[2].trigger.cli_name.as_deref(), Some("region-edit-copy"));
        // twice: still unique
        let idx2 = duplicate(&mut list, 1).unwrap();
        assert_eq!(list[idx2].id, "capture-region-edit-copy-2");
        assert_eq!(duplicate(&mut list, 99), None);
        let s = Settings { workflows: list, ..Settings::default() };
        assert!(!s.validate().iter().any(|i| i.severity == ssx_core::settings::Severity::Error));
    }

    #[test]
    fn delete_picks_a_sensible_selection() {
        let mut list = builtin_workflows();
        let n = list.len();
        assert_eq!(delete(&mut list, 0), Some(0));
        let last = list.len() - 1;
        assert_eq!(delete(&mut list, last), Some(last - 1));
        assert_eq!(list.len(), n - 2);
        assert_eq!(delete(&mut list, 99), None);
        while list.len() > 1 {
            delete(&mut list, 0);
        }
        assert_eq!(delete(&mut list, 0), None);
        assert!(list.is_empty());
    }

    #[test]
    fn step_pickers_hide_what_is_present_or_invalid() {
        let mut w = Workflow {
            input: InputKind::CaptureRegion,
            after_capture: vec![AfterCapture::SaveToFile, AfterCapture::Upload],
            ..Workflow::default()
        };
        let add = addable_capture_steps(&w);
        assert!(!add.contains(&AfterCapture::SaveToFile) && !add.contains(&AfterCapture::Upload));
        assert!(add.contains(&AfterCapture::OpenEditor));
        w.input = InputKind::RecordScreen;
        let add = addable_capture_steps(&w);
        assert!(!add.contains(&AfterCapture::OpenEditor) && !add.contains(&AfterCapture::Ocr));
        assert!(add.contains(&AfterCapture::DeleteLocalFile));
    }

    #[test]
    fn run_command_can_be_added_repeatedly_but_other_upload_steps_once() {
        let mut w = Workflow::default();
        assert_eq!(addable_upload_steps(&w).len(), 7);
        w.after_upload = vec![
            AfterUpload::CopyUrl,
            AfterUpload::RunCommand { program: "x".into(), args: vec![] },
        ];
        let add = addable_upload_steps(&w);
        assert_eq!(add.len(), 6);
        assert!(!add.contains(&AfterUpload::CopyUrl));
        assert!(add.iter().any(|s| matches!(s, AfterUpload::RunCommand { .. })));
    }

    #[test]
    fn switching_to_a_recording_drops_image_steps_and_says_which() {
        let mut w = Workflow {
            after_capture: vec![
                AfterCapture::OpenEditor,
                AfterCapture::SaveToFile,
                AfterCapture::CopyImageToClipboard,
                AfterCapture::Upload,
            ],
            ..Workflow::default()
        };
        let dropped = set_input(&mut w, InputKind::RecordGif);
        assert_eq!(dropped, [AfterCapture::OpenEditor, AfterCapture::CopyImageToClipboard]);
        assert_eq!(w.after_capture, [AfterCapture::SaveToFile, AfterCapture::Upload]);
        assert!(set_input(&mut w, InputKind::CaptureWindow).is_empty());
        assert_eq!(w.input, InputKind::CaptureWindow);
        // the result validates
        let s = Settings { workflows: vec![Workflow { id: "a".into(), name: "A".into(), ..w }], ..Settings::default() };
        assert!(!s.validate().iter().any(|i| i.severity == ssx_core::settings::Severity::Error));
    }

    #[test]
    fn destination_overrides_set_and_clear() {
        let mut w = Workflow::default();
        set_destination(&mut w, DestinationType::Video, Some("youtube".into()));
        assert_eq!(w.destination.video.as_deref(), Some("youtube"));
        set_destination(&mut w, DestinationType::Video, Some("  ".into()));
        assert_eq!(w.destination.video, None);
        for (t, _, _) in DESTINATION_ROWS {
            set_destination(&mut w, t, Some("x".into()));
            assert_eq!(w.destination.get(t), Some("x"));
            set_destination(&mut w, t, None);
            assert_eq!(w.destination.get(t), None);
        }
    }

    #[test]
    fn every_enum_value_has_a_label() {
        for (i, l, d) in INPUT_KINDS {
            assert_eq!(input_label(i), l);
            assert_eq!(input_blurb(i), d);
        }
        for (s, l, d) in CAPTURE_STEPS {
            assert_eq!(capture_step_label(s), l);
            assert_eq!(capture_step_blurb(s), d);
        }
        for s in upload_step_kinds() {
            assert!(!upload_step_label(&s).is_empty() && !upload_step_blurb(&s).is_empty());
        }
        assert_eq!(destination_label(DestinationType::UrlShortener), "URL shortener");
        assert_eq!(INPUT_KINDS.len(), 9);
        assert_eq!(CAPTURE_STEPS.len(), 8);
    }

    proptest! {
        #[test]
        fn slugify_always_gives_a_valid_id(name in ".{0,100}") {
            let s = slugify(&name, 64, "workflow");
            prop_assert!(!s.is_empty() && s.len() <= 64);
            prop_assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')));
            let w = Workflow { id: s, name: "n".into(), ..Workflow::default() };
            let settings = Settings { workflows: vec![w], ..Settings::default() };
            prop_assert!(!settings.validate().iter().any(|i| i.path.ends_with(".id")));
        }

        #[test]
        fn repeated_duplication_never_collides(picks in proptest::collection::vec(0usize..50, 1..25)) {
            let mut list = builtin_workflows();
            for p in picks {
                let i = p % list.len();
                let _ = duplicate(&mut list, i);
            }
            let s = Settings { workflows: list, ..Settings::default() };
            let errors: Vec<_> = s.validate().into_iter().filter(|i| i.severity == ssx_core::settings::Severity::Error).collect();
            prop_assert!(errors.is_empty(), "{:?}", errors);
        }
    }
}
