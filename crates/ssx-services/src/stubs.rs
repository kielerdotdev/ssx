//! Services whose implementation lives in crates that do not exist yet.
//!
//! Each fails with [`ServiceError::Unsupported`] naming the future crate, so the engine
//! reports "pinning images to the screen (needs the ssx-overlay crate) is not supported here"
//! instead of a generic message, and a reader of the code knows where the real thing goes.

use std::path::Path;

use ssx_core::workflow::{
    CancelToken, Ocr, Pinner, RecordRequest, Recorder, RecordingSession, SaveDialog, ServiceError,
};
use ssx_types::Frame;

fn unsupported<T>(what: &str, crate_name: &str) -> Result<T, ServiceError> {
    Err(ServiceError::Unsupported(format!(
        "{what} (needs the {crate_name} crate, which is not built yet)"
    )))
}

/// Screen recording; will be provided by `ssx-record`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubRecorder;

impl Recorder for StubRecorder {
    fn start(&self, _: &RecordRequest) -> Result<Box<dyn RecordingSession>, ServiceError> {
        unsupported("screen recording", "ssx-record")
    }
}

/// Pin-to-screen windows; will be provided by `ssx-overlay`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubPinner;

impl Pinner for StubPinner {
    fn pin(&self, _: &Frame) -> Result<(), ServiceError> {
        unsupported("pinning images to the screen", "ssx-overlay")
    }
}

/// Text recognition; will be provided by `ssx-ocr`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StubOcr;

impl Ocr for StubOcr {
    fn recognize(&self, _: &Frame, _: &CancelToken) -> Result<String, ServiceError> {
        unsupported("text recognition", "ssx-ocr")
    }
}

/// The "save as" dialog when the `save-dialog` feature is off (the CLI never has one).
#[derive(Debug, Default, Clone, Copy)]
pub struct StubSaveDialog;

impl SaveDialog for StubSaveDialog {
    fn choose_path(&self, _: &Path) -> Result<Option<std::path::PathBuf>, ServiceError> {
        Err(ServiceError::Unsupported(
            "the save-as dialog (this build has no dialog support; enable the `save-dialog` feature of ssx-services)"
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use ssx_types::{ColorSpace, PixelFormat, Size};

    use super::*;

    #[test]
    fn stubs_name_the_crate_that_will_replace_them() {
        let frame = Frame::new(Size::new(1, 1), PixelFormat::Rgba8, ColorSpace::Srgb);
        let cases: [(ServiceError, &str); 4] = [
            (StubPinner.pin(&frame).unwrap_err(), "ssx-overlay"),
            (StubOcr.recognize(&frame, &CancelToken::new()).unwrap_err(), "ssx-ocr"),
            (
                StubRecorder
                    .start(&RecordRequest {
                        kind: ssx_core::workflow::RecordKind::Video,
                        output_dir: ".".into(),
                        file_stem: "x".into(),
                        include_cursor: false,
                    })
                    .map(|_| ())
                    .unwrap_err(),
                "ssx-record",
            ),
            (StubSaveDialog.choose_path(Path::new("x.png")).unwrap_err(), "save-dialog"),
        ];
        for (err, needle) in cases {
            assert!(matches!(&err, ServiceError::Unsupported(m) if m.contains(needle)), "{err}");
        }
    }
}
