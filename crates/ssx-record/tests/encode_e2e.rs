//! End-to-end encoder tests: synthetic source -> convert -> encoder -> file, then decode
//! the file back and check what a player would see.

#![cfg(feature = "ffmpeg")]

mod common;

use ssx_record::{
    encode::{
        CachingProber, Candidate, Codec, Container, HwPolicy, Prober, Quality, QualityPreset,
        SelectionRequest, VideoSettings, candidates,
        ffmpeg::{FfmpegProber, ffmpeg_version, listed_encoders},
    },
    source::synthetic::bar_color_at,
    time::Fps,
    verify::inspect,
};
use ssx_types::Size;

fn close(a: [u8; 3], b: [u8; 3], tol: i32) -> bool {
    a.iter().zip(b).all(|(x, y)| (i32::from(*x) - i32::from(y)).abs() <= tol)
}

/// Checks a decoded picture against the synthetic pattern for frame `n`.
fn assert_pattern(img: &ssx_record::verify::RgbImage, n: u64, tol: i32) {
    // Bar colours at the centre of each of the 8 bars, top area.
    let w = img.width;
    for bar in 0..8u32 {
        let x = bar * (w / 8) + w / 16;
        let y = img.height / 10;
        let expect = bar_color_at(n, x, w);
        let got = img.pixel(x, y);
        assert!(close(got, expect, tol), "frame {n} bar at x={x}: got {got:?}, want {expect:?}");
    }
}

#[test]
fn prints_the_encoders_of_this_machine() {
    let listed = listed_encoders();
    eprintln!("{}", ffmpeg_version());
    eprintln!("listed encoders: {listed:?}");
    let prober = CachingProber::new(FfmpegProber);
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1, Codec::Vp9, Codec::Mpeg4] {
        let req = SelectionRequest {
            container: Container::Mkv,
            codec,
            hw: HwPolicy::PreferHardware,
            allow_gpl: true,
            allow_codec_fallback: false,
            platform: ssx_record::encode::select::Platform::current(),
            encoder_override: None,
        };
        for c in candidates(&req) {
            eprintln!("  {:<14} {:?}", c.name, prober.probe(&c));
        }
    }
    assert!(
        prober
            .probe(&Candidate {
                codec: Codec::Mpeg4,
                name: "mpeg4",
                kind: ssx_record::encode::EncoderKind::Native,
                gpl: false
            })
            .is_usable(),
        "the native mpeg4 encoder exists in every FFmpeg build"
    );
}

fn check_video_file(
    path: &std::path::Path,
    frames: u64,
    fps: Fps,
    size: Size,
    pixel_tol: i32,
) -> ssx_record::verify::MediaReport {
    let rep = inspect(path).expect("decode back");
    let v = rep.video.as_ref().expect("video stream");
    assert_eq!((v.width, v.height), (size.width, size.height));
    assert_eq!(v.frame_count() as u64, frames, "frame count");
    let period = v.mean_frame_period();
    assert!(
        (period - 1.0 / fps.as_f64()).abs() < 0.002,
        "frame period {period} vs {}",
        1.0 / fps.as_f64()
    );
    assert!((v.avg_fps - fps.as_f64()).abs() < 0.5, "avg fps {}", v.avg_fps);
    let expected_duration = frames as f64 / fps.as_f64();
    assert!(
        (rep.duration - expected_duration).abs() < 0.1,
        "duration {} vs {expected_duration}",
        rep.duration
    );
    // Counters burnt into the frames: every frame must show its own index.
    for (i, c) in v.counters.iter().enumerate() {
        assert_eq!(u64::from(*c), i as u64, "frame {i} shows counter {c}");
    }
    assert_pattern(v.first.as_ref().unwrap(), 0, pixel_tol);
    assert_pattern(v.last.as_ref().unwrap(), frames - 1, pixel_tol);
    assert!(rep.seekable, "seeking must work");
    rep
}

#[test]
fn mp4_h264_software_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.mp4");
    let size = Size::new(320, 240);
    let (summary, sel) = common::encode_synthetic(
        &path,
        Container::Mp4,
        common::sw_settings(),
        size,
        Fps::FPS_30,
        60,
        None,
    );
    eprintln!("selected {} ({} bytes)", summary.encoder, summary.bytes);
    assert_eq!(sel.chosen.codec, Codec::H264, "libx264 is installed on the dev box");
    assert_eq!(summary.video_frames, 60);
    assert!(summary.bytes > 1000);
    let rep = check_video_file(&path, 60, Fps::FPS_30, size, 24);
    let mp4 = rep.mp4.as_ref().expect("mp4 layout");
    assert!(mp4.has_moov(), "{:?}", mp4.boxes);
    assert!(mp4.moov_first(), "faststart: moov must precede mdat, boxes: {:?}", mp4.boxes);
    assert_eq!(rep.video.as_ref().unwrap().codec, "h264");
}

#[test]
fn mp4_mpeg4_native_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.mp4");
    let size = Size::new(320, 240);
    let settings = VideoSettings {
        encoder_override: Some("mpeg4".into()),
        quality: Quality::Preset(QualityPreset::High),
        ..common::sw_settings()
    };
    let (summary, _) =
        common::encode_synthetic(&path, Container::Mp4, settings, size, Fps::FPS_30, 45, None);
    assert!(summary.encoder.contains("mpeg4"));
    let rep = check_video_file(&path, 45, Fps::FPS_30, size, 40);
    assert_eq!(rep.video.as_ref().unwrap().codec, "mpeg4");
}

#[test]
fn webm_vp9_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.webm");
    let size = Size::new(320, 240);
    let (summary, sel) = common::encode_synthetic(
        &path,
        Container::WebM,
        common::sw_settings(),
        size,
        Fps::FPS_30,
        60,
        None,
    );
    eprintln!("selected {}", summary.encoder);
    assert_eq!(sel.chosen.name, "libvpx-vp9");
    let rep = check_video_file(&path, 60, Fps::FPS_30, size, 24);
    assert_eq!(rep.video.as_ref().unwrap().codec, "vp9");
    assert!(rep.format.contains("webm"), "{}", rep.format);
}

#[test]
fn mkv_and_odd_frame_rates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.mkv");
    let size = Size::new(256, 144);
    let fps = Fps::new(30_000, 1001);
    let _ =
        common::encode_synthetic(&path, Container::Mkv, common::sw_settings(), size, fps, 40, None);
    let rep = inspect(&path).unwrap();
    let v = rep.video.unwrap();
    assert_eq!(v.frame_count(), 40);
    assert!(
        (v.mean_frame_period() - 1001.0 / 30_000.0).abs() < 0.0015,
        "{}",
        v.mean_frame_period()
    );
}

#[test]
fn fragmented_mp4_is_playable_without_faststart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frag.mp4");
    let settings =
        VideoSettings { mp4: ssx_record::encode::Mp4Mode::Fragmented, ..common::sw_settings() };
    let size = Size::new(320, 240);
    let _ = common::encode_synthetic(&path, Container::Mp4, settings, size, Fps::FPS_30, 30, None);
    let rep = inspect(&path).unwrap();
    let mp4 = rep.mp4.unwrap();
    assert!(mp4.boxes.iter().any(|b| b == "moof"), "{:?}", mp4.boxes);
    assert_eq!(rep.video.unwrap().frame_count(), 30);
}

#[test]
fn keyframe_interval_is_respected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kf.mp4");
    let settings = VideoSettings {
        keyframe_interval: std::time::Duration::from_secs(1),
        ..common::sw_settings()
    };
    let size = Size::new(320, 240);
    let _ = common::encode_synthetic(&path, Container::Mp4, settings, size, Fps::FPS_30, 91, None);
    // ffprobe is the independent witness for packet flags.
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time,flags"])
        .args(["-of", "csv=p=0"])
        .arg(&path)
        .output();
    let Ok(out) = out else {
        eprintln!("SKIP keyframe check: ffprobe not installed");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let keys: Vec<f64> = text
        .lines()
        .filter(|l| l.contains(",K"))
        .filter_map(|l| l.split(',').next()?.parse().ok())
        .collect();
    assert!(keys.len() >= 3 && keys.len() <= 5, "keyframes at {keys:?}");
    assert!(keys[0] < 0.05);
    assert!((keys[1] - 1.0).abs() < 0.2, "second keyframe at {}", keys[1]);
}

#[test]
fn ffprobe_agrees_with_our_decoder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("probe.mp4");
    let size = Size::new(640, 360);
    let _ = common::encode_synthetic(
        &path,
        Container::Mp4,
        common::sw_settings(),
        size,
        Fps::FPS_30,
        60,
        None,
    );
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=codec_name,width,height,r_frame_rate,nb_read_frames,pix_fmt,color_space,color_range"])
        .args(["-of", "default=nw=1"])
        .arg(&path)
        .output();
    let Ok(out) = out else {
        eprintln!("SKIP: ffprobe not installed");
        return;
    };
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!("{text}");
    for want in [
        "width=640",
        "height=360",
        "r_frame_rate=30/1",
        "nb_read_frames=60",
        "pix_fmt=yuv420p",
        "color_space=bt709",
        "color_range=tv",
    ] {
        assert!(text.contains(want), "ffprobe output lacks `{want}`:\n{text}");
    }
}

#[test]
fn audio_track_is_present_and_in_sync() {
    let dir = tempfile::tempdir().unwrap();
    let size = Size::new(320, 240);
    for (container, name) in
        [(Container::Mp4, "a.mp4"), (Container::WebM, "a.webm"), (Container::Mkv, "a.mkv")]
    {
        let path = dir.path().join(name);
        // 3 s of video, 3 s of audio with a 440 Hz tone starting at 1.0 s.
        let (summary, _) = common::encode_synthetic(
            &path,
            container,
            common::sw_settings(),
            size,
            Fps::FPS_30,
            90,
            Some((3.0, 1.0)),
        );
        assert!(summary.has_audio, "{name}: {summary:?}");
        let rep = inspect(&path).unwrap();
        let a = rep.audio.as_ref().unwrap_or_else(|| panic!("{name}: no audio stream"));
        eprintln!(
            "{name}: audio codec {} rate {} start {} decoded {:.3}s",
            a.codec,
            a.sample_rate,
            a.start,
            a.decoded_seconds()
        );
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(a.channels, 2);
        assert!((a.decoded_seconds() - 3.0).abs() < 0.08, "{name}: {}", a.decoded_seconds());
        let onset = a.onset(0.1).expect("tone must be audible");
        // The container may carry a start offset (encoder delay); the sync criterion is
        // where the tone lands on the *shared* timeline.
        let onset_abs = onset + a.start.max(0.0);
        assert!(
            (onset_abs - 1.0).abs() < 0.04,
            "{name}: tone starts at {onset_abs:.3}s, expected 1.000s (+-40 ms)"
        );
        assert!(a.rms(1.2, 2.5) > 0.2, "{name}: tone level");
        assert!(a.rms(0.1, 0.9) < 0.02, "{name}: silence before the tone");
        // Video and audio must cover the same time.
        let v = rep.video.unwrap();
        assert!(
            (v.duration - a.duration).abs() < 0.1,
            "{name}: video {} vs audio {}",
            v.duration,
            a.duration
        );
    }
}
