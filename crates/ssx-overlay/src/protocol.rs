//! Wire format between the library client ([`crate::select_via_helper`]) and the
//! `ssx-overlay` helper process.
//!
//! * The **request** is one JSON document on the helper's stdin. It describes the frame
//!   (size, stride, format, origin, scale) and names where its pixels are; the pixels
//!   themselves never go through JSON.
//! * The **response** is one JSON line on stdout.
//! * Exit code 0 means "a response was written" (including `Cancelled`); anything else with
//!   no response means the helper crashed.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ssx_types::{Monitor, PixelFormat, Point, WindowInfo};

use crate::types::{OverlayOptions, OverlayOutcome};

/// Current protocol version; the helper rejects other versions.
pub const PROTOCOL_VERSION: u32 = 1;

/// Where the frame's raw pixels are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameSource {
    /// A file descriptor the helper inherited (a sealed `memfd` on Linux). The helper opens
    /// it through `/proc/self/fd/N`, which yields an independent file description.
    Fd(i32),
    /// A private temporary file (Windows, macOS); the client deletes it afterwards.
    Path(PathBuf),
}

/// Description of the raw pixel buffer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameDesc {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row in the buffer.
    pub stride: usize,
    /// Channel order.
    pub format: PixelFormat,
    /// Desktop position of the first pixel.
    pub origin: Point,
    /// `Frame::scale_factor`.
    pub scale_factor: f64,
    /// Where the pixels are.
    pub source: FrameSource,
}

/// Helper request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Must equal [`PROTOCOL_VERSION`].
    pub version: u32,
    /// The frozen desktop.
    pub frame: FrameDesc,
    /// Monitors.
    pub monitors: Vec<Monitor>,
    /// Windows for hover-snap.
    pub windows: Vec<WindowInfo>,
    /// Options.
    pub options: OverlayOptions,
}

/// Start-up timings measured inside the helper, in milliseconds since the helper's `main`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Timing {
    /// Request parsed, frame mapped, converted and pre-dimmed.
    pub ready_ms: f64,
    /// The first frame reached the screen (`None` if the overlay never got that far).
    pub first_frame_ms: Option<f64>,
}

/// Helper response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    /// The overlay ran to completion.
    Outcome {
        /// What the user did.
        outcome: OverlayOutcome,
        /// Start-up timings.
        timing: Timing,
    },
    /// The overlay could not run.
    Error {
        /// Human-readable reason.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use ssx_types::Rect;

    use super::*;
    use crate::types::{Selection, SelectionShape};

    #[test]
    fn request_and_response_round_trip() {
        let req = Request {
            version: PROTOCOL_VERSION,
            frame: FrameDesc {
                width: 10,
                height: 5,
                stride: 48,
                format: PixelFormat::Bgra8,
                origin: Point::new(-10, 0),
                scale_factor: 1.5,
                source: FrameSource::Fd(7),
            },
            monitors: vec![],
            windows: vec![],
            options: OverlayOptions::default(),
        };
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), req);
        let resp = Response::Outcome {
            outcome: OverlayOutcome::Selected(Selection {
                rect: Rect::new(1, 2, 3, 4),
                shape: SelectionShape::Freeform(vec![Point::new(1, 2)]),
                snapped_window: None,
            }),
            timing: Timing { ready_ms: 1.5, first_frame_ms: Some(9.0) },
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(!s.contains('\n'), "one line");
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), resp);
    }
}
