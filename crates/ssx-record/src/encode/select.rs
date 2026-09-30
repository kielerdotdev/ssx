//! Encoder auto-selection: an ordered candidate chain, filtered by policy, walked with a
//! [`Prober`] that finds out what *actually works* on this machine.
//!
//! Whether FFmpeg lists `h264_nvenc` says nothing about whether this PC has an NVIDIA GPU
//! and a working driver, so selection never trusts the encoder list. The production prober
//! ([`crate::encode::ffmpeg::FfmpegProber`]) opens each candidate with a tiny test frame
//! and encodes a few frames; [`CachingProber`] remembers the answer for the lifetime of the
//! application (opening NVENC or QSV takes 50-300 ms, which is far too slow to repeat per
//! recording). The policy in this module is pure, so it is unit-tested against a fake
//! prober.
//!
//! Chain (first usable wins):
//!
//! 1. hardware encoders of the requested codec, in vendor order for the platform
//!    (Windows NVENC, AMF, QSV, Media Foundation; Linux NVENC, VAAPI, QSV; macOS
//!    VideoToolbox);
//! 2. software encoders of that codec (`libx264` if GPL is allowed, `libopenh264`, ...);
//! 3. if allowed, the same for other codecs the container can hold, ending in FFmpeg's
//!    native `mpeg4`, which every FFmpeg build has.

use std::{collections::HashMap, sync::Mutex};

use super::{
    InputKind,
    settings::{Codec, Container, HwPolicy},
};
use crate::error::RecordError;

/// Hardware encoder API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HwApi {
    /// NVIDIA NVENC.
    Nvenc,
    /// AMD AMF.
    Amf,
    /// Intel Quick Sync (oneVPL / MFX).
    Qsv,
    /// VA-API (Intel/AMD on Linux).
    Vaapi,
    /// Apple VideoToolbox.
    VideoToolbox,
    /// Windows Media Foundation (vendor MFTs).
    MediaFoundation,
}

/// Kind of encoder implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncoderKind {
    /// GPU or fixed-function hardware.
    Hardware(HwApi),
    /// A software library (libx264, libvpx, ...).
    Software,
    /// FFmpeg's own software encoder (`mpeg4`).
    Native,
}

/// The platforms with distinct hardware chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// Windows.
    Windows,
    /// Linux and the BSDs.
    Linux,
    /// macOS.
    MacOs,
}

impl Platform {
    /// The platform this build runs on.
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_vendor = "apple") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// One encoder that might be usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// Codec family it produces.
    pub codec: Codec,
    /// FFmpeg encoder name.
    pub name: &'static str,
    /// Hardware or software.
    pub kind: EncoderKind,
    /// The encoder library is GPL-licensed.
    pub gpl: bool,
}

impl Candidate {
    /// Pixel layout this encoder is fed with.
    pub fn input_kind(&self) -> InputKind {
        match self.kind {
            EncoderKind::Hardware(_) => InputKind::Nv12,
            _ => InputKind::Yuv420p,
        }
    }

    /// `name (kind)` for logs and UI.
    pub fn describe(&self) -> String {
        match self.kind {
            EncoderKind::Hardware(api) => format!("{} (hardware, {api:?})", self.name),
            EncoderKind::Software => format!("{} (software)", self.name),
            EncoderKind::Native => format!("{} (built-in)", self.name),
        }
    }
}

const fn hw(codec: Codec, name: &'static str, api: HwApi) -> Candidate {
    Candidate { codec, name, kind: EncoderKind::Hardware(api), gpl: false }
}
const fn sw(codec: Codec, name: &'static str, gpl: bool) -> Candidate {
    Candidate { codec, name, kind: EncoderKind::Software, gpl }
}

fn hardware_chain(codec: Codec, platform: Platform) -> Vec<Candidate> {
    use HwApi::{Amf, MediaFoundation, Nvenc, Qsv, Vaapi, VideoToolbox};
    let all: &[Candidate] = match (codec, platform) {
        (Codec::H264, Platform::Windows) => &[
            hw(Codec::H264, "h264_nvenc", Nvenc),
            hw(Codec::H264, "h264_amf", Amf),
            hw(Codec::H264, "h264_qsv", Qsv),
            hw(Codec::H264, "h264_mf", MediaFoundation),
        ],
        (Codec::H264, Platform::Linux) => &[
            hw(Codec::H264, "h264_nvenc", Nvenc),
            hw(Codec::H264, "h264_vaapi", Vaapi),
            hw(Codec::H264, "h264_qsv", Qsv),
        ],
        (Codec::H264, Platform::MacOs) => &[hw(Codec::H264, "h264_videotoolbox", VideoToolbox)],
        (Codec::Hevc, Platform::Windows) => &[
            hw(Codec::Hevc, "hevc_nvenc", Nvenc),
            hw(Codec::Hevc, "hevc_amf", Amf),
            hw(Codec::Hevc, "hevc_qsv", Qsv),
            hw(Codec::Hevc, "hevc_mf", MediaFoundation),
        ],
        (Codec::Hevc, Platform::Linux) => &[
            hw(Codec::Hevc, "hevc_nvenc", Nvenc),
            hw(Codec::Hevc, "hevc_vaapi", Vaapi),
            hw(Codec::Hevc, "hevc_qsv", Qsv),
        ],
        (Codec::Hevc, Platform::MacOs) => &[hw(Codec::Hevc, "hevc_videotoolbox", VideoToolbox)],
        (Codec::Av1, Platform::Windows) => &[
            hw(Codec::Av1, "av1_nvenc", Nvenc),
            hw(Codec::Av1, "av1_amf", Amf),
            hw(Codec::Av1, "av1_qsv", Qsv),
        ],
        (Codec::Av1, Platform::Linux) => &[
            hw(Codec::Av1, "av1_nvenc", Nvenc),
            hw(Codec::Av1, "av1_vaapi", Vaapi),
            hw(Codec::Av1, "av1_qsv", Qsv),
        ],
        (Codec::Vp9, Platform::Linux) => {
            &[hw(Codec::Vp9, "vp9_vaapi", Vaapi), hw(Codec::Vp9, "vp9_qsv", Qsv)]
        }
        (Codec::Vp9, Platform::Windows) => &[hw(Codec::Vp9, "vp9_qsv", Qsv)],
        _ => &[],
    };
    all.to_vec()
}

fn software_chain(codec: Codec) -> Vec<Candidate> {
    match codec {
        Codec::H264 => {
            vec![sw(Codec::H264, "libx264", true), sw(Codec::H264, "libopenh264", false)]
        }
        Codec::Hevc => vec![sw(Codec::Hevc, "libx265", true)],
        Codec::Av1 => vec![
            sw(Codec::Av1, "libsvtav1", false),
            sw(Codec::Av1, "libaom-av1", false),
            sw(Codec::Av1, "librav1e", false),
        ],
        Codec::Vp9 => vec![sw(Codec::Vp9, "libvpx-vp9", false)],
        Codec::Mpeg4 => {
            vec![Candidate { codec, name: "mpeg4", kind: EncoderKind::Native, gpl: false }]
        }
        Codec::Auto => Vec::new(),
    }
}

/// Order in which other codecs are tried when the requested one has no working encoder.
const FALLBACK_CODECS: [Codec; 5] =
    [Codec::H264, Codec::Mpeg4, Codec::Vp9, Codec::Av1, Codec::Hevc];

/// What the caller wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionRequest {
    /// Output container.
    pub container: Container,
    /// Wanted codec (`Auto` resolves through the container).
    pub codec: Codec,
    /// Hardware policy.
    pub hw: HwPolicy,
    /// Allow GPL encoders.
    pub allow_gpl: bool,
    /// Allow other codecs when the requested one is unavailable.
    pub allow_codec_fallback: bool,
    /// Platform whose hardware chain to use.
    pub platform: Platform,
    /// A specific FFmpeg encoder name to force.
    pub encoder_override: Option<String>,
}

impl SelectionRequest {
    /// A request built from user settings for the current platform.
    pub fn from_settings(container: Container, v: &super::settings::VideoSettings) -> Self {
        Self {
            container,
            codec: v.codec,
            hw: v.hw,
            allow_gpl: v.allow_gpl,
            allow_codec_fallback: v.allow_codec_fallback,
            platform: Platform::current(),
            encoder_override: v.encoder_override.clone(),
        }
    }
}

/// The ordered candidate chain for a request (nothing is probed here).
pub fn candidates(req: &SelectionRequest) -> Vec<Candidate> {
    let primary = req.codec.resolve(req.container);
    let mut order = vec![primary];
    if req.allow_codec_fallback && req.hw != HwPolicy::HardwareOnly {
        order.extend(FALLBACK_CODECS.iter().copied().filter(|c| *c != primary));
    }
    let mut out = Vec::new();
    for codec in order {
        if !req.container.supports(codec) {
            continue;
        }
        if req.hw != HwPolicy::SoftwareOnly {
            out.extend(hardware_chain(codec, req.platform));
        }
        if req.hw != HwPolicy::HardwareOnly {
            out.extend(software_chain(codec).into_iter().filter(|c| req.allow_gpl || !c.gpl));
        }
    }
    out
}

/// Whether an encoder works here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeResult {
    /// The encoder opened and produced packets.
    Usable,
    /// It did not; the string says why (not compiled in, no device, driver error...).
    Unavailable(String),
}

impl ProbeResult {
    /// `true` for [`ProbeResult::Usable`].
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Usable)
    }
}

/// Finds out whether a candidate encoder works on this machine.
pub trait Prober {
    /// Tries `candidate` and reports the outcome.
    fn probe(&self, candidate: &Candidate) -> ProbeResult;
}

/// Caches another prober's answers by encoder name. Interior mutability lets a single
/// instance be shared (`&self`) by every recording of the application.
#[derive(Debug)]
pub struct CachingProber<P> {
    inner: P,
    cache: Mutex<HashMap<&'static str, ProbeResult>>,
}

impl<P: Prober> CachingProber<P> {
    /// Wraps `inner`.
    pub fn new(inner: P) -> Self {
        Self { inner, cache: Mutex::new(HashMap::new()) }
    }

    /// Forgets all cached answers (after a driver install or GPU hot-plug).
    pub fn clear(&self) {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
    }

    /// Number of cached answers.
    pub fn cached(&self) -> usize {
        self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }
}

impl<P: Prober> Prober for CachingProber<P> {
    fn probe(&self, candidate: &Candidate) -> ProbeResult {
        if let Some(hit) =
            self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(candidate.name)
        {
            return hit.clone();
        }
        // Probe outside the lock: opening a GPU encoder can take a while.
        let result = self.inner.probe(candidate);
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(candidate.name, result.clone());
        result
    }
}

/// The outcome of [`select`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The encoder to use.
    pub chosen: Candidate,
    /// Every candidate tried before (and including) the chosen one, with the reasons.
    pub tried: Vec<(Candidate, ProbeResult)>,
    /// Candidates after the chosen one, untested: used if opening at the real
    /// resolution fails although the probe passed.
    pub remaining: Vec<Candidate>,
    /// The chosen encoder is for a different codec than requested.
    pub codec_fallback: bool,
}

impl Selection {
    /// One line per rejected candidate, for logs.
    pub fn rejected_lines(&self) -> Vec<String> {
        self.tried
            .iter()
            .filter_map(|(c, r)| match r {
                ProbeResult::Unavailable(why) => Some(format!("{}: {why}", c.name)),
                ProbeResult::Usable => None,
            })
            .collect()
    }
}

/// Walks the candidate chain and returns the first encoder `prober` reports usable.
pub fn select(req: &SelectionRequest, prober: &dyn Prober) -> Result<Selection, RecordError> {
    let primary = req.codec.resolve(req.container);
    let list = if let Some(name) = &req.encoder_override {
        vec![override_candidate(name)?]
    } else {
        candidates(req)
    };
    let mut tried = Vec::new();
    for (i, cand) in list.iter().enumerate() {
        let result = prober.probe(cand);
        tried.push((*cand, result.clone()));
        if result.is_usable() {
            return Ok(Selection {
                chosen: *cand,
                tried,
                remaining: list[i + 1..].to_vec(),
                codec_fallback: cand.codec != primary,
            });
        }
    }
    Err(RecordError::NoEncoder {
        codec: primary.name().to_owned(),
        container: req.container.extension().to_owned(),
        tried: tried
            .iter()
            .map(|(c, r)| match r {
                ProbeResult::Unavailable(why) => format!("{}: {why}", c.name),
                ProbeResult::Usable => c.name.to_owned(),
            })
            .collect(),
    })
}

/// Finds the known candidate with this FFmpeg encoder name.
fn override_candidate(name: &str) -> Result<Candidate, RecordError> {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9, Codec::Mpeg4] {
        for platform in [Platform::Windows, Platform::Linux, Platform::MacOs] {
            if let Some(c) = hardware_chain(codec, platform).into_iter().find(|c| c.name == name) {
                return Ok(c);
            }
        }
        if let Some(c) = software_chain(codec).into_iter().find(|c| c.name == name) {
            return Ok(c);
        }
    }
    Err(RecordError::InvalidConfig(format!("unknown encoder `{name}`")))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashSet};

    use super::*;
    use crate::encode::settings::VideoSettings;

    /// A prober that accepts a fixed set of encoder names and counts calls.
    struct Fake {
        usable: HashSet<&'static str>,
        calls: RefCell<Vec<&'static str>>,
    }

    impl Fake {
        fn new(usable: &[&'static str]) -> Self {
            Self { usable: usable.iter().copied().collect(), calls: RefCell::default() }
        }
    }

    impl Prober for Fake {
        fn probe(&self, c: &Candidate) -> ProbeResult {
            self.calls.borrow_mut().push(c.name);
            if self.usable.contains(c.name) {
                ProbeResult::Usable
            } else {
                ProbeResult::Unavailable(format!("{} is not available (fake)", c.name))
            }
        }
    }

    fn req(container: Container, codec: Codec, platform: Platform) -> SelectionRequest {
        SelectionRequest {
            container,
            codec,
            hw: HwPolicy::PreferHardware,
            allow_gpl: true,
            allow_codec_fallback: true,
            platform,
            encoder_override: None,
        }
    }

    fn names(list: &[Candidate]) -> Vec<&'static str> {
        list.iter().map(|c| c.name).collect()
    }

    #[test]
    fn windows_h264_chain_is_hardware_first() {
        let list = candidates(&req(Container::Mp4, Codec::H264, Platform::Windows));
        assert_eq!(
            &names(&list)[..6],
            ["h264_nvenc", "h264_amf", "h264_qsv", "h264_mf", "libx264", "libopenh264"]
        );
    }

    #[test]
    fn linux_and_mac_chains() {
        let l = candidates(&req(Container::Mp4, Codec::H264, Platform::Linux));
        assert_eq!(
            &names(&l)[..5],
            ["h264_nvenc", "h264_vaapi", "h264_qsv", "libx264", "libopenh264"]
        );
        let m = candidates(&req(Container::Mp4, Codec::H264, Platform::MacOs));
        assert_eq!(&names(&m)[..2], ["h264_videotoolbox", "libx264"]);
    }

    #[test]
    fn native_encoder_is_the_first_codec_fallback() {
        let l = candidates(&req(Container::Mp4, Codec::H264, Platform::Linux));
        let listed = names(&l);
        let h264_last = listed.iter().position(|n| *n == "libopenh264").unwrap();
        assert_eq!(listed[h264_last + 1], "mpeg4");
        // WebM cannot hold mpeg4 or h264.
        let w = candidates(&req(Container::WebM, Codec::Auto, Platform::Linux));
        assert!(names(&w).contains(&"libvpx-vp9") && !names(&w).contains(&"mpeg4"));
        assert!(!names(&w).contains(&"libx264"));
    }

    #[test]
    fn software_only_skips_hardware_and_hardware_only_skips_software() {
        let mut r = req(Container::Mp4, Codec::H264, Platform::Linux);
        r.hw = HwPolicy::SoftwareOnly;
        let l = candidates(&r);
        assert!(l.iter().all(|c| !matches!(c.kind, EncoderKind::Hardware(_))));
        r.hw = HwPolicy::HardwareOnly;
        let l = candidates(&r);
        assert!(!l.is_empty());
        assert!(l.iter().all(|c| matches!(c.kind, EncoderKind::Hardware(_))));
        assert!(l.iter().all(|c| c.codec == Codec::H264), "no codec fallback for hardware-only");
    }

    #[test]
    fn gpl_encoders_can_be_excluded() {
        let mut r = req(Container::Mp4, Codec::H264, Platform::Linux);
        r.allow_gpl = false;
        let l = candidates(&r);
        assert!(!names(&l).contains(&"libx264"));
        assert!(names(&l).contains(&"libopenh264"));
        r.codec = Codec::Hevc;
        assert!(!names(&candidates(&r)).contains(&"libx265"));
    }

    #[test]
    fn codec_fallback_can_be_disabled() {
        let mut r = req(Container::Mp4, Codec::H264, Platform::Linux);
        r.allow_codec_fallback = false;
        assert!(candidates(&r).iter().all(|c| c.codec == Codec::H264));
    }

    #[test]
    fn selection_picks_first_usable_and_records_rejections() {
        let fake = Fake::new(&["h264_vaapi", "libx264"]);
        let s = select(&req(Container::Mp4, Codec::H264, Platform::Linux), &fake).unwrap();
        assert_eq!(s.chosen.name, "h264_vaapi");
        assert_eq!(fake.calls.borrow().as_slice(), ["h264_nvenc", "h264_vaapi"]);
        assert_eq!(s.rejected_lines().len(), 1);
        assert!(s.rejected_lines()[0].starts_with("h264_nvenc:"));
        assert_eq!(names(&s.remaining)[0], "h264_qsv");
        assert!(!s.codec_fallback);
    }

    #[test]
    fn fallback_chain_walks_hardware_software_native() {
        // Nothing but the native encoder works.
        let fake = Fake::new(&["mpeg4"]);
        let s = select(&req(Container::Mp4, Codec::H264, Platform::Windows), &fake).unwrap();
        assert_eq!(s.chosen.name, "mpeg4");
        assert!(s.codec_fallback);
        assert!(s.tried.len() > 5);
        assert!(s.tried[..s.tried.len() - 1].iter().all(|(_, r)| !r.is_usable()));
    }

    #[test]
    fn software_takes_over_when_hardware_is_missing() {
        let fake = Fake::new(&["libx264", "mpeg4"]);
        let s = select(&req(Container::Mp4, Codec::H264, Platform::Windows), &fake).unwrap();
        assert_eq!(s.chosen.name, "libx264");
        assert!(!s.codec_fallback);
        assert_eq!(s.chosen.input_kind(), InputKind::Yuv420p);
        // ... and GPL off drops to openh264.
        let fake = Fake::new(&["libx264", "libopenh264"]);
        let mut r = req(Container::Mp4, Codec::H264, Platform::Windows);
        r.allow_gpl = false;
        assert_eq!(select(&r, &fake).unwrap().chosen.name, "libopenh264");
    }

    #[test]
    fn hardware_uses_nv12() {
        let fake = Fake::new(&["h264_nvenc"]);
        let s = select(&req(Container::Mp4, Codec::H264, Platform::Linux), &fake).unwrap();
        assert_eq!(s.chosen.input_kind(), InputKind::Nv12);
    }

    #[test]
    fn no_usable_encoder_lists_every_reason() {
        let fake = Fake::new(&[]);
        let err = select(&req(Container::WebM, Codec::Auto, Platform::Linux), &fake).unwrap_err();
        let RecordError::NoEncoder { codec, container, tried } = err else { panic!("{err:?}") };
        assert_eq!(codec, "vp9");
        assert_eq!(container, "webm");
        assert!(tried.iter().any(|t| t.starts_with("libvpx-vp9:")), "{tried:?}");
        assert!(tried.iter().all(|t| t.contains("not available (fake)")));
    }

    #[test]
    fn override_probes_only_that_encoder() {
        let fake = Fake::new(&["libx264"]);
        let mut r = req(Container::Mp4, Codec::H264, Platform::Linux);
        r.encoder_override = Some("libx264".into());
        let s = select(&r, &fake).unwrap();
        assert_eq!(s.chosen.name, "libx264");
        assert_eq!(fake.calls.borrow().as_slice(), ["libx264"]);
        // A broken override fails instead of silently choosing something else.
        r.encoder_override = Some("h264_nvenc".into());
        assert!(select(&r, &fake).is_err());
    }

    #[test]
    fn caching_prober_probes_each_encoder_once() {
        let fake = Fake::new(&["libx264"]);
        let cache = CachingProber::new(&fake);
        let r = req(Container::Mp4, Codec::H264, Platform::Linux);
        assert_eq!(select(&r, &cache).unwrap().chosen.name, "libx264");
        let first = fake.calls.borrow().len();
        assert_eq!(select(&r, &cache).unwrap().chosen.name, "libx264");
        assert_eq!(fake.calls.borrow().len(), first, "second selection must hit the cache");
        assert_eq!(cache.cached(), first);
        cache.clear();
        assert_eq!(cache.cached(), 0);
    }

    impl<P: Prober> Prober for &P {
        fn probe(&self, c: &Candidate) -> ProbeResult {
            (**self).probe(c)
        }
    }

    #[test]
    fn request_from_settings_copies_policy() {
        let v =
            VideoSettings { hw: HwPolicy::SoftwareOnly, allow_gpl: false, ..Default::default() };
        let r = SelectionRequest::from_settings(Container::Mkv, &v);
        assert_eq!(r.hw, HwPolicy::SoftwareOnly);
        assert!(!r.allow_gpl);
        assert_eq!(r.platform, Platform::current());
    }
}
