//! Platform-independent logic of the Windows Graphics Capture source.
//!
//! Everything that can be decided without calling the OS lives here, so it is compiled and
//! unit-tested on every platform; `source::windows` (Windows only) is the thin glue around
//! the `windows` crate:
//!
//! * [`native_format`]: DXGI texture format -> pixel format and colour space (frames are
//!   labelled from the texture that actually arrived, not from what was requested);
//! * [`depad_rows`] / [`clamp_content`]: the D3D staging texture has a row pitch that is
//!   larger than the row, and the frame's *content* may be smaller than the texture;
//! * [`pool_action`]: when a window is resized the frame pool must be re-created;
//! * [`TimestampMapper`]: WGC stamps frames with `SystemRelativeTime` (QPC, 100 ns ticks),
//!   which is much more accurate than "when the event handler ran". It is mapped onto the
//!   session [`crate::time::Clock`] and made monotonic;
//! * [`resolve_target`]: which monitor or window a [`CaptureTarget`] means, and the crop
//!   for regions.

use std::time::Duration;

use ssx_types::{ColorSpace, Frame, PixelFormat, Point, Rect, Size};

use super::CaptureTarget;
use crate::error::SourceError;

/// `DXGI_FORMAT_R16G16B16A16_FLOAT`.
pub const DXGI_R16G16B16A16_FLOAT: u32 = 10;
/// `DXGI_FORMAT_R8G8B8A8_UNORM`.
pub const DXGI_R8G8B8A8_UNORM: u32 = 28;
/// `DXGI_FORMAT_R8G8B8A8_UNORM_SRGB`.
pub const DXGI_R8G8B8A8_UNORM_SRGB: u32 = 29;
/// `DXGI_FORMAT_B8G8R8A8_UNORM`.
pub const DXGI_B8G8R8A8_UNORM: u32 = 87;
/// `DXGI_FORMAT_B8G8R8A8_UNORM_SRGB`.
pub const DXGI_B8G8R8A8_UNORM_SRGB: u32 = 91;

/// How the pixels of a texture with DXGI format `dxgi` are to be labelled, or `None` for a
/// format the source cannot read.
///
/// WGC desktop capture yields `B8G8R8A8` (SDR) or `R16G16B16A16_FLOAT` (HDR, linear scRGB
/// with 1.0 = 80 nits).
#[must_use]
pub fn native_format(dxgi: u32) -> Option<(PixelFormat, ColorSpace)> {
    match dxgi {
        DXGI_B8G8R8A8_UNORM | DXGI_B8G8R8A8_UNORM_SRGB => {
            Some((PixelFormat::Bgra8, ColorSpace::Srgb))
        }
        DXGI_R8G8B8A8_UNORM | DXGI_R8G8B8A8_UNORM_SRGB => {
            Some((PixelFormat::Rgba8, ColorSpace::Srgb))
        }
        DXGI_R16G16B16A16_FLOAT => Some((PixelFormat::Rgba16F, ColorSpace::ScRgbLinear)),
        _ => None,
    }
}

/// The part of a texture that holds picture: `content` (what WGC reports for the frame,
/// possibly not positive) clamped to the texture size. `None` when nothing is left.
#[must_use]
pub fn clamp_content(content: (i32, i32), texture: (u32, u32)) -> Option<(u32, u32)> {
    let w = u32::try_from(content.0).ok()?.min(texture.0);
    let h = u32::try_from(content.1).ok()?.min(texture.1);
    (w > 0 && h > 0).then_some((w, h))
}

/// Copies `height` rows of `width * bpp` bytes out of a mapped surface whose rows are
/// `pitch` bytes apart (the last row may be shorter than the pitch, as D3D maps it).
///
/// # Errors
/// [`SourceError::InvalidFrame`] when the pitch is smaller than a row or the buffer is too
/// short.
pub fn depad_rows(
    src: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    bpp: usize,
) -> Result<Vec<u8>, SourceError> {
    let row = width as usize * bpp;
    if pitch < row {
        return Err(SourceError::InvalidFrame(format!(
            "row pitch {pitch} is smaller than a row ({row} bytes)"
        )));
    }
    let rows = height as usize;
    if rows == 0 {
        return Ok(Vec::new());
    }
    let need = pitch
        .checked_mul(rows - 1)
        .and_then(|n| n.checked_add(row))
        .ok_or_else(|| SourceError::InvalidFrame("mapped surface size overflows".into()))?;
    if src.len() < need {
        return Err(SourceError::InvalidFrame(format!(
            "mapped surface has {} bytes, {need} needed",
            src.len()
        )));
    }
    if pitch == row {
        return Ok(src[..row * rows].to_vec());
    }
    let mut out = Vec::with_capacity(row * rows);
    for y in 0..rows {
        out.extend_from_slice(&src[y * pitch..y * pitch + row]);
    }
    Ok(out)
}

/// What to do with the frame pool when a frame with a given content size arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolAction {
    /// The pool's buffers still fit.
    Keep,
    /// Re-create the pool at this size (the captured window or monitor changed size).
    Recreate {
        /// New width in pixels.
        width: u32,
        /// New height in pixels.
        height: u32,
    },
}

/// Decides whether the frame pool (currently `pool` pixels) must be re-created for a frame
/// whose content is `content`. WGC keeps delivering frames in the old buffer size after a
/// window resize, cropped or padded, until the pool is re-created.
#[must_use]
pub fn pool_action(pool: (u32, u32), content: (i32, i32)) -> PoolAction {
    match (u32::try_from(content.0), u32::try_from(content.1)) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 && (w, h) != pool => {
            PoolAction::Recreate { width: w, height: h }
        }
        _ => PoolAction::Keep,
    }
}

/// Maps WGC's `SystemRelativeTime` onto the session clock.
///
/// Both are QPC-based on Windows, so they tick at the same rate; only the offset between
/// them is unknown. The offset is learned as the *minimum* of `arrival - system_time`: the
/// smallest observed value is the one with the least scheduling delay in it. Stamps never
/// exceed the arrival time (a frame cannot come from the future) and never go backwards.
#[derive(Debug, Default, Clone, Copy)]
pub struct TimestampMapper {
    offset: Option<i128>, // nanoseconds: clock = system + offset
    last: Duration,
}

impl TimestampMapper {
    /// A mapper that has seen no frame yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The session-clock time of a frame stamped `system_100ns` (100 ns ticks since boot)
    /// that reached us at `arrival` on the session clock.
    pub fn map(&mut self, system_100ns: i64, arrival: Duration) -> Duration {
        let system_ns = i128::from(system_100ns) * 100;
        let candidate = arrival.as_nanos() as i128 - system_ns;
        let offset = match self.offset {
            Some(o) => o.min(candidate),
            None => candidate,
        };
        self.offset = Some(offset);
        let mapped = (system_ns + offset).clamp(0, arrival.as_nanos() as i128);
        let t = Duration::from_nanos(u64::try_from(mapped).unwrap_or(u64::MAX)).max(self.last);
        self.last = t;
        t
    }
}

/// What the source needs to know about a monitor (built from `Monitor`).
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorDesc {
    /// `Monitor::id` (the GDI device name, `\\.\DISPLAY1`).
    pub id: String,
    /// Bounds on the virtual desktop, physical pixels.
    pub rect: Rect,
    /// The primary monitor.
    pub primary: bool,
    /// HDR is active: request a float pool.
    pub hdr_active: bool,
}

/// A capture item chosen for a [`CaptureTarget`].
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedTarget {
    /// A monitor (index into the list passed to [`resolve_target`]), optionally cropped to
    /// `crop` (desktop coordinates) afterwards.
    Monitor {
        /// Index into the monitor list.
        index: usize,
        /// Region of the desktop to keep.
        crop: Option<Rect>,
    },
    /// A top-level window by handle.
    Window(isize),
}

/// Parses a `WindowInfo::id` of the Windows backend (the decimal `HWND`).
#[must_use]
pub fn parse_window_id(id: &str) -> Option<isize> {
    id.trim().parse::<isize>().ok().filter(|h| *h != 0)
}

/// Picks the capture item for `target`.
///
/// WGC captures *one* item, so [`CaptureTarget::Desktop`] means the primary monitor (a
/// virtual desktop spanning several monitors would need one session per monitor and a
/// composite); a [`CaptureTarget::Region`] uses the monitor it overlaps most and crops to
/// the part of the region on it.
///
/// # Errors
/// [`SourceError::TargetNotFound`] for an unknown monitor id, a bad window id or a region
/// outside all monitors; [`SourceError::Unsupported`] for [`CaptureTarget::Pick`] (the
/// system picker needs a window to parent it to; the caller supplies the target).
pub fn resolve_target(
    target: &CaptureTarget,
    monitors: &[MonitorDesc],
) -> Result<ResolvedTarget, SourceError> {
    let primary = || {
        monitors
            .iter()
            .position(|m| m.primary)
            .or(if monitors.is_empty() { None } else { Some(0) })
            .ok_or_else(|| SourceError::Unavailable("no monitors".into()))
    };
    match target {
        CaptureTarget::Desktop => Ok(ResolvedTarget::Monitor { index: primary()?, crop: None }),
        CaptureTarget::Monitor(id) => monitors
            .iter()
            .position(|m| m.id.eq_ignore_ascii_case(id))
            .map(|index| ResolvedTarget::Monitor { index, crop: None })
            .ok_or_else(|| SourceError::TargetNotFound(format!("monitor {id}"))),
        CaptureTarget::Window(id) => parse_window_id(id)
            .map(ResolvedTarget::Window)
            .ok_or_else(|| SourceError::TargetNotFound(format!("window {id}"))),
        CaptureTarget::Region(r) => {
            let (index, part) = monitors
                .iter()
                .enumerate()
                .filter_map(|(i, m)| m.rect.intersect(*r).map(|p| (i, p)))
                .max_by_key(|(_, p)| u64::from(p.width) * u64::from(p.height))
                .ok_or_else(|| SourceError::TargetNotFound(format!("region {r:?}")))?;
            Ok(ResolvedTarget::Monitor { index, crop: Some(part) })
        }
        CaptureTarget::Pick => Err(SourceError::Unsupported(
            "the Windows source has no picker; choose a monitor or window".into(),
        )),
    }
}

/// Builds a [`Frame`] from a tightly packed read-back.
///
/// The desktop is opaque, but the alpha channel of capture textures is undefined, so it is
/// forced to opaque (8-bit formats; a float frame's alpha is not used downstream).
///
/// # Errors
/// [`SourceError::InvalidFrame`] when the buffer does not match the size.
pub fn frame_from_readback(
    data: Vec<u8>,
    size: Size,
    dxgi: u32,
    origin: Point,
    scale_factor: f64,
    sdr_white_nits: Option<f32>,
) -> Result<Frame, SourceError> {
    let (format, color_space) = native_format(dxgi)
        .ok_or_else(|| SourceError::InvalidFrame(format!("unsupported DXGI format {dxgi}")))?;
    let stride = size.width as usize * format.bytes_per_pixel();
    let mut frame = Frame::from_raw(size, stride, format, color_space, data)
        .map_err(|e| SourceError::InvalidFrame(e.to_string()))?;
    frame.origin = origin;
    frame.scale_factor = scale_factor;
    if format == PixelFormat::Rgba16F {
        frame.sdr_white_nits = sdr_white_nits.or(Some(80.0));
    } else {
        frame.set_opaque();
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mons() -> Vec<MonitorDesc> {
        vec![
            MonitorDesc {
                id: r"\\.\DISPLAY1".into(),
                rect: Rect::new(0, 0, 1920, 1080),
                primary: false,
                hdr_active: false,
            },
            MonitorDesc {
                id: r"\\.\DISPLAY2".into(),
                rect: Rect::new(1920, 0, 2560, 1440),
                primary: true,
                hdr_active: true,
            },
        ]
    }

    #[test]
    fn formats_are_labelled_from_the_texture() {
        assert_eq!(
            native_format(DXGI_B8G8R8A8_UNORM),
            Some((PixelFormat::Bgra8, ColorSpace::Srgb))
        );
        assert_eq!(
            native_format(DXGI_R16G16B16A16_FLOAT),
            Some((PixelFormat::Rgba16F, ColorSpace::ScRgbLinear))
        );
        assert_eq!(native_format(DXGI_R8G8B8A8_UNORM).unwrap().0, PixelFormat::Rgba8);
        assert_eq!(native_format(24), None); // R10G10B10A2 is not read
    }

    #[test]
    fn content_is_clamped_to_the_texture() {
        assert_eq!(clamp_content((800, 600), (1024, 768)), Some((800, 600)));
        assert_eq!(clamp_content((2000, 600), (1024, 768)), Some((1024, 600)));
        assert_eq!(clamp_content((0, 600), (1024, 768)), None);
        assert_eq!(clamp_content((-5, 600), (1024, 768)), None);
    }

    #[test]
    fn depad_removes_row_padding_and_tolerates_a_short_last_row() {
        // 2x3 image, 4 bytes/px, pitch 12 (row 8): rows are 8 bytes then 4 padding.
        let mut src = Vec::new();
        for y in 0..3u8 {
            src.extend((0..8).map(|i| y * 10 + i));
            if y < 2 {
                src.extend([0xEE; 4]);
            }
        }
        let out = depad_rows(&src, 12, 2, 3, 4).unwrap();
        assert_eq!(out.len(), 24);
        assert_eq!(&out[8..16], &[10, 11, 12, 13, 14, 15, 16, 17]);
        assert!(!out.contains(&0xEE));
        assert!(depad_rows(&src[..20], 12, 2, 3, 4).is_err());
        assert!(depad_rows(&src, 6, 2, 3, 4).is_err());
        assert_eq!(depad_rows(&[], 8, 2, 0, 4).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn depad_of_tight_rows_is_a_plain_copy() {
        let src: Vec<u8> = (0..24).collect();
        assert_eq!(depad_rows(&src, 8, 2, 3, 4).unwrap(), src);
    }

    #[test]
    fn the_pool_is_recreated_only_for_a_real_size_change() {
        assert_eq!(pool_action((800, 600), (800, 600)), PoolAction::Keep);
        assert_eq!(
            pool_action((800, 600), (1024, 600)),
            PoolAction::Recreate { width: 1024, height: 600 }
        );
        assert_eq!(pool_action((800, 600), (0, 0)), PoolAction::Keep);
        assert_eq!(pool_action((800, 600), (-3, 10)), PoolAction::Keep);
    }

    #[test]
    fn timestamps_are_anchored_monotonic_and_never_in_the_future() {
        let mut m = TimestampMapper::new();
        // System time is 5 s since boot; the first frame reaches us 1 ms late (clock 2.001 s
        // while the true offset is -3.000 s).
        let sys0 = 50_000_000; // 5 s in 100 ns ticks
        let t0 = m.map(sys0, Duration::from_micros(2_001_000));
        assert_eq!(t0, Duration::from_micros(2_001_000));
        // 33 ms later, arriving only 0.2 ms late: the learned offset shrinks to the smaller
        // delay, so this stamp is exact.
        let t1 = m.map(sys0 + 330_000, Duration::from_micros(2_033_200));
        assert_eq!(t1, Duration::from_micros(2_033_200));
        // A frame that waited 10 ms in a queue keeps its real capture time.
        let t2 = m.map(sys0 + 660_000, Duration::from_micros(2_076_000));
        assert_eq!(t2, Duration::from_micros(2_066_200));
        // A stamp that would go backwards is held.
        let t3 = m.map(sys0 + 100_000, Duration::from_micros(2_200_000));
        assert_eq!(t3, t2);
        // And never past the arrival time.
        let t4 = m.map(sys0 + 90_000_000, Duration::from_micros(2_300_000));
        assert_eq!(t4, Duration::from_micros(2_300_000));
    }

    #[test]
    fn targets_resolve_to_monitors_windows_and_crops() {
        let m = mons();
        assert_eq!(
            resolve_target(&CaptureTarget::Desktop, &m).unwrap(),
            ResolvedTarget::Monitor { index: 1, crop: None },
            "the desktop is the primary monitor"
        );
        assert_eq!(
            resolve_target(&CaptureTarget::Monitor(r"\\.\display1".into()), &m).unwrap(),
            ResolvedTarget::Monitor { index: 0, crop: None }
        );
        assert!(matches!(
            resolve_target(&CaptureTarget::Monitor("nope".into()), &m),
            Err(SourceError::TargetNotFound(_))
        ));
        assert_eq!(
            resolve_target(&CaptureTarget::Window("197430".into()), &m).unwrap(),
            ResolvedTarget::Window(197_430)
        );
        assert!(resolve_target(&CaptureTarget::Window("0".into()), &m).is_err());
        assert!(resolve_target(&CaptureTarget::Window("abc".into()), &m).is_err());
        // A region straddling both monitors goes to the one it covers more of.
        let region = Rect::new(1800, 100, 400, 300); // 120 px on #1, 280 px on #2
        assert_eq!(
            resolve_target(&CaptureTarget::Region(region), &m).unwrap(),
            ResolvedTarget::Monitor { index: 1, crop: Some(Rect::new(1920, 100, 280, 300)) }
        );
        assert!(resolve_target(&CaptureTarget::Region(Rect::new(9000, 0, 10, 10)), &m).is_err());
        assert!(matches!(
            resolve_target(&CaptureTarget::Pick, &m),
            Err(SourceError::Unsupported(_))
        ));
        assert!(resolve_target(&CaptureTarget::Desktop, &[]).is_err());
    }

    #[test]
    fn readback_frames_carry_placement_and_opaque_alpha() {
        let f = frame_from_readback(
            [10u8, 20, 30, 0].repeat(4),
            Size::new(2, 2),
            DXGI_B8G8R8A8_UNORM,
            Point::new(1920, 0),
            1.5,
            None,
        )
        .unwrap();
        assert_eq!(f.format(), PixelFormat::Bgra8);
        assert_eq!(f.origin, Point::new(1920, 0));
        assert!(f.data().chunks(4).all(|p| p[3] == 255));
        assert!(f.sdr_white_nits.is_none());

        let hdr = frame_from_readback(
            vec![0; 2 * 2 * 8],
            Size::new(2, 2),
            DXGI_R16G16B16A16_FLOAT,
            Point::default(),
            1.0,
            Some(200.0),
        )
        .unwrap();
        assert_eq!(hdr.color_space(), ColorSpace::ScRgbLinear);
        assert_eq!(hdr.sdr_white_nits, Some(200.0));
        assert!(
            frame_from_readback(
                vec![0; 3],
                Size::new(2, 2),
                DXGI_B8G8R8A8_UNORM,
                Point::default(),
                1.0,
                None
            )
            .is_err()
        );
        assert!(
            frame_from_readback(vec![0; 16], Size::new(2, 2), 24, Point::default(), 1.0, None)
                .is_err()
        );
    }
}
