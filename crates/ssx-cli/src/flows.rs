//! Building the ad-hoc workflows the CLI commands run, and applying command-line overrides to
//! configured ones. Pure functions over `ssx-core`'s settings types, unit-tested below.

use ssx_core::{
    settings::{
        AfterCapture, AfterUpload, DestinationOverride, DestinationType, ImageFormatKind,
        InputKind, Settings, Trigger, Workflow,
    },
    workflow::StepKind,
};

use crate::{
    cli::{FormatArg, KindArg},
    error::{CliError, CliResult},
};

/// Which optional steps a command asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wanted {
    /// Save the image in the configured save folder (no `--output`).
    pub save: bool,
    /// Copy the image to the clipboard.
    pub copy_image: bool,
    /// Upload.
    pub upload: bool,
    /// Copy the resulting URL(s).
    pub copy_url: bool,
}

impl Wanted {
    /// The optional steps whose failure must fail the command (the user asked for them).
    pub fn explicit_steps(self) -> Vec<StepKind> {
        let mut v = Vec::new();
        if self.copy_image {
            v.push(StepKind::CopyImage);
        }
        if self.copy_url {
            v.push(StepKind::CopyUrl);
        }
        v
    }
}

/// An ad-hoc workflow doing exactly what was asked, in a sensible order.
pub fn adhoc_workflow(id: &str, name: &str, input: InputKind, wanted: Wanted, to: Option<&str>) -> Workflow {
    let mut after_capture = Vec::new();
    if wanted.save {
        after_capture.push(AfterCapture::SaveToFile);
    }
    if wanted.copy_image {
        after_capture.push(AfterCapture::CopyImageToClipboard);
    }
    if wanted.upload {
        after_capture.push(AfterCapture::Upload);
    }
    let mut after_upload = Vec::new();
    if wanted.upload && wanted.copy_url {
        after_upload.push(AfterUpload::CopyUrl);
    }
    let mut wf = Workflow {
        id: id.to_owned(),
        name: name.to_owned(),
        trigger: Trigger::default(),
        input,
        after_capture,
        destination: DestinationOverride::default(),
        after_upload,
    };
    if let Some(name) = to {
        route_everything_to(&mut wf.destination, name);
    }
    wf
}

/// Sends every upload type to the uploader called `name`.
pub fn route_everything_to(dest: &mut DestinationOverride, name: &str) {
    for ty in [DestinationType::Image, DestinationType::Text, DestinationType::File, DestinationType::Video] {
        set_destination(dest, ty, name);
    }
}

fn set_destination(dest: &mut DestinationOverride, ty: DestinationType, name: &str) {
    let slot = match ty {
        DestinationType::Image => &mut dest.image,
        DestinationType::Text => &mut dest.text,
        DestinationType::File => &mut dest.file,
        DestinationType::Video => &mut dest.video,
        DestinationType::UrlShortener => &mut dest.url_shortener,
        DestinationType::UrlSharing => &mut dest.url_sharing,
    };
    *slot = Some(name.to_owned());
}

/// Applies `--to` / `--kind` to a (configured or ad-hoc) workflow.
///
/// `--to NAME` sends every file to that uploader. `--kind K` sends every file through the
/// destination configured for `K`, whatever its extension says.
pub fn apply_destination_flags(
    wf: &mut Workflow,
    settings: &Settings,
    to: Option<&str>,
    kind: Option<KindArg>,
) -> CliResult<()> {
    if let Some(name) = to {
        route_everything_to(&mut wf.destination, name);
    } else if let Some(kind) = kind {
        let (ty, label, key) = match kind {
            KindArg::Image => (DestinationType::Image, "image", "image"),
            KindArg::File => (DestinationType::File, "file", "file"),
            KindArg::Video => (DestinationType::Video, "video", "video"),
            KindArg::Text => (DestinationType::Text, "text", "text"),
        };
        let name = settings
            .destinations
            .resolve(ty, &wf.destination, None)
            .ok_or_else(|| {
                CliError::new(format!("no {label} uploader is configured for --kind {label}"))
                    .hint(format!("set destinations.{key} in settings.toml (ssx config set destinations.{key} NAME) or pass --to NAME"))
            })?
            .to_owned();
        route_everything_to(&mut wf.destination, &name);
    }
    Ok(())
}

/// The image format to encode with: `--format`, else the extension of `--output`, else the
/// setting.
pub fn choose_format(
    flag: Option<FormatArg>,
    output_ext: Option<&str>,
    setting: ImageFormatKind,
) -> CliResult<ImageFormatKind> {
    if let Some(f) = flag {
        return Ok(match f {
            FormatArg::Png => ImageFormatKind::Png,
            FormatArg::Jpg => ImageFormatKind::Jpg,
            FormatArg::Webp => ImageFormatKind::Webp,
        });
    }
    match output_ext.map(str::to_ascii_lowercase).as_deref() {
        None | Some("") => Ok(setting),
        Some("png") => Ok(ImageFormatKind::Png),
        Some("jpg" | "jpeg") => Ok(ImageFormatKind::Jpg),
        Some("webp") => Ok(ImageFormatKind::Webp),
        Some(other) => Err(CliError::usage(format!(
            "cannot tell the image format from the extension .{other}"
        ))
        .hint("use a .png, .jpg or .webp file name, or pass --format png|jpg|webp")),
    }
}

/// `path` with the format's extension appended when it has none.
pub fn with_default_extension(path: &std::path::Path, format: ImageFormatKind) -> std::path::PathBuf {
    if path.extension().is_some() {
        path.to_path_buf()
    } else {
        let mut p = path.as_os_str().to_owned();
        p.push(".");
        p.push(format.extension());
        p.into()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    #[test]
    fn workflows_do_exactly_what_was_asked_in_order() {
        let w = adhoc_workflow(
            "cli",
            "cli",
            InputKind::Files,
            Wanted { save: true, copy_image: true, upload: true, copy_url: true },
            None,
        );
        assert_eq!(
            w.after_capture,
            [AfterCapture::SaveToFile, AfterCapture::CopyImageToClipboard, AfterCapture::Upload]
        );
        assert_eq!(w.after_upload, [AfterUpload::CopyUrl]);

        let none = adhoc_workflow("cli", "cli", InputKind::Files, Wanted::default(), None);
        assert!(none.after_capture.is_empty() && none.after_upload.is_empty());
        // Copying a URL without uploading is meaningless and adds nothing.
        let no_upload = adhoc_workflow(
            "cli",
            "cli",
            InputKind::Files,
            Wanted { copy_url: true, ..Wanted::default() },
            None,
        );
        assert!(no_upload.after_upload.is_empty());
    }

    #[test]
    fn explicit_steps_are_what_the_user_asked_for() {
        let w = Wanted { copy_image: true, copy_url: true, ..Wanted::default() };
        assert_eq!(w.explicit_steps(), [StepKind::CopyImage, StepKind::CopyUrl]);
        assert!(Wanted::default().explicit_steps().is_empty());
    }

    #[test]
    fn to_routes_every_upload_type() {
        let w = adhoc_workflow("c", "c", InputKind::Files, Wanted { upload: true, ..Wanted::default() }, Some("mine"));
        for slot in [&w.destination.image, &w.destination.text, &w.destination.file, &w.destination.video] {
            assert_eq!(slot.as_deref(), Some("mine"));
        }
        assert!(w.destination.url_shortener.is_none());
    }

    #[test]
    fn kind_uses_the_configured_destination_for_that_kind() {
        let mut settings = Settings::default();
        settings.destinations.image = Some("imgur".into());
        settings.destinations.file = Some("s3".into());
        let mut wf = adhoc_workflow("c", "c", InputKind::Files, Wanted { upload: true, ..Wanted::default() }, None);
        apply_destination_flags(&mut wf, &settings, None, Some(KindArg::Image)).unwrap();
        assert_eq!(wf.destination.file.as_deref(), Some("imgur"), "a .zip is now sent as an image");
        assert_eq!(wf.destination.video.as_deref(), Some("imgur"));

        let mut wf = adhoc_workflow("c", "c", InputKind::Files, Wanted::default(), None);
        let e = apply_destination_flags(&mut wf, &settings, None, Some(KindArg::Text)).unwrap_err();
        assert!(e.message.contains("no text uploader") && e.hint.unwrap().contains("destinations.text"));

        // --to beats --kind.
        let mut wf = adhoc_workflow("c", "c", InputKind::Files, Wanted::default(), None);
        apply_destination_flags(&mut wf, &settings, Some("x"), Some(KindArg::Text)).unwrap();
        assert_eq!(wf.destination.text.as_deref(), Some("x"));
    }

    #[test]
    fn format_choice_precedence_and_errors() {
        use ImageFormatKind::*;
        assert_eq!(choose_format(Some(FormatArg::Webp), Some("png"), Jpg).unwrap(), Webp);
        assert_eq!(choose_format(None, Some("PNG"), Jpg).unwrap(), Png);
        assert_eq!(choose_format(None, Some("jpeg"), Png).unwrap(), Jpg);
        assert_eq!(choose_format(None, None, Webp).unwrap(), Webp);
        assert_eq!(choose_format(None, Some(""), Webp).unwrap(), Webp);
        let e = choose_format(None, Some("bmp"), Png).unwrap_err();
        assert_eq!(e.code, crate::error::ExitCode::Usage);
        assert!(e.hint.unwrap().contains("--format"));
    }

    #[test]
    fn missing_extensions_are_appended() {
        assert_eq!(with_default_extension(Path::new("shot"), ImageFormatKind::Jpg), PathBuf::from("shot.jpg"));
        assert_eq!(with_default_extension(Path::new("a/shot.png"), ImageFormatKind::Jpg), PathBuf::from("a/shot.png"));
        assert_eq!(with_default_extension(Path::new("v1.2/shot"), ImageFormatKind::Png), PathBuf::from("v1.2/shot.png"));
    }
}
