//! Real audio devices through `cpal`, against a private headless `PipeWire`.
//!
//! The graph: a `null-audio-sink` that is the **default sink** (what "system audio" is), and a
//! `pw-loopback` that exposes a virtual **default microphone**. `pw-play` puts a 300 Hz tone
//! on the sink and a 700 Hz tone into the microphone. A recording with `CpalSource`s for
//! both must contain both tones (mixed) and stay aligned with the video.
//!
//! What this proves: cpal's `PipeWire` host offers loopback and input capture, our device
//! selection and timestamping work, and the mix reaches the file. What it cannot prove:
//! WASAPI loopback (needs Windows) or ALSA/PulseAudio-only systems.
#![cfg(all(target_os = "linux", feature = "audio", feature = "ffmpeg"))]
#![allow(unsafe_code)] // `set_var` for the in-process PipeWire client; one test, no threads yet

#[path = "pwstack/mod.rs"]
mod pwstack;

use std::{
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use pwstack::{Bus, PipeWire};
use ssx_record::{
    audio::{
        AudioEvent, AudioSource,
        device::{CpalSource, DeviceKind, DeviceSelector, list_devices},
    },
    encode::{HwPolicy, VideoSettings, ffmpeg::FfmpegProber},
    session::{RecordConfig, RecordingSession},
    source::{
        FrameSource,
        synthetic::{SyntheticConfig, SyntheticSource},
    },
    time::{Clock, Fps},
    verify::inspect,
};

fn write_wav(path: &Path, hz: f32, secs: f32) {
    let rate = 48_000u32;
    let n = (rate as f32 * secs) as usize;
    let mut pcm = Vec::with_capacity(n * 4);
    for i in 0..n {
        let s =
            ((i as f32 / rate as f32 * hz * std::f32::consts::TAU).sin() * 0.4 * 32767.0) as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(&pcm);
    std::fs::write(path, w).expect("write wav");
}

/// Goertzel power at `hz`, normalised to the signal power.
fn tone_strength(samples: &[f32], channels: usize, rate: f32, hz: f32) -> f32 {
    let mono: Vec<f32> = samples.chunks(channels).map(|f| f[0]).collect();
    let k = 2.0 * (std::f32::consts::TAU * hz / rate).cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for x in &mono {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - k * s1 * s2;
    let energy: f32 = mono.iter().map(|x| x * x).sum();
    if energy == 0.0 { 0.0 } else { power / (energy * mono.len() as f32 / 2.0) }
}

/// The strongest [`tone_strength`] within +-1 % of `hz`: a device whose clock differs from the
/// system clock is resampled by the drift controller, which moves the pitch by that amount.
fn tone_near(samples: &[f32], channels: usize, rate: f32, hz: f32) -> f32 {
    (-40..=40)
        .map(|i| tone_strength(samples, channels, rate, hz * (1.0 + i as f32 * 0.00025)))
        .fold(0.0, f32::max)
}

struct Guard(Vec<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        for c in &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[test]
fn pipewire_loopback_and_microphone_are_recorded_and_mixed() {
    let Some(bus) = Bus::start() else { return };
    let Some(pw) = PipeWire::start(&bus) else { return };
    let dir = pw.dir.path().to_path_buf();
    for prog in ["pw-cli", "pw-loopback", "pw-play"] {
        if Command::new(prog)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("SKIP: `{prog}` is not installed");
            return;
        }
    }
    let env = |c: &mut Command| {
        c.env("XDG_RUNTIME_DIR", &dir)
            .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    };
    // Null sink = the default output (system audio).
    let mut c = Command::new("pw-cli");
    c.args([
        "create-node",
        "adapter",
        "{ factory.name=support.null-audio-sink node.name=ssx-sink media.class=Audio/Sink object.linger=true audio.position=[FL FR] }",
    ]);
    env(&mut c);
    assert!(c.status().unwrap().success());
    // Virtual microphone fed through `ssx-mic-in`.
    let mut c = Command::new("pw-loopback");
    c.args([
        "-m",
        "[ FL FR ]",
        "--capture-props=media.class=Audio/Sink node.name=ssx-mic-in",
        "--playback-props=media.class=Audio/Source node.name=ssx-mic",
    ]);
    env(&mut c);
    let loopback = c.spawn().expect("pw-loopback");
    std::thread::sleep(Duration::from_millis(2500)); // wireplumber sets the defaults

    // The in-process PipeWire client (cpal) finds the private instance through the environment.
    // SAFETY: this is the only test in this binary and no other thread reads the environment yet.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        std::env::set_var("PIPEWIRE_RUNTIME_DIR", &dir);
    }
    let devices = list_devices();
    eprintln!("devices: {devices:#?}");
    assert!(
        devices.iter().any(|d| d.host.eq_ignore_ascii_case("pipewire") && d.loopback),
        "{devices:?}"
    );

    // Tones: 300 Hz to the sink, 700 Hz into the virtual mic.
    let (sys_wav, mic_wav) = (dir.join("sys.wav"), dir.join("mic.wav"));
    write_wav(&sys_wav, 300.0, 12.0);
    write_wav(&mic_wav, 700.0, 12.0);
    let mut players = Vec::new();
    for (target, wav) in [("ssx-sink", &sys_wav), ("ssx-mic-in", &mic_wav)] {
        let mut c = Command::new("pw-play");
        c.arg("--target").arg(target).arg(wav);
        env(&mut c);
        players.push(c.spawn().expect("pw-play"));
    }
    players.push(loopback);
    let _guard = Guard(players);
    std::thread::sleep(Duration::from_millis(800));

    // 1. Each device on its own.
    let clock = Clock::start();
    let mut sys = CpalSource::new(DeviceKind::SystemLoopback, DeviceSelector::Default);
    let fmt = sys.start(clock).expect("system loopback opens on PipeWire");
    eprintln!("system audio: {} {fmt:?}", sys.name());
    let mut got = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        if let AudioEvent::Chunk(c) = sys.read(Duration::from_millis(100)).unwrap() {
            got.extend(c.samples);
        }
    }
    sys.stop();
    let ch = usize::from(fmt.channels);
    let (s300, s700) = (
        tone_strength(&got, ch, fmt.sample_rate as f32, 300.0),
        tone_strength(&got, ch, fmt.sample_rate as f32, 700.0),
    );
    eprintln!("system audio tone strength: 300 Hz {s300:.3}, 700 Hz {s700:.3}");
    assert!(s300 > 0.5 && s700 < 0.05, "the loopback carries the sink's tone only");
    // The virtual PipeWire clock is not the system clock: measure by how much.
    let device_rate = (got.len() / ch) as f64 / t0.elapsed().as_secs_f64();
    eprintln!("system audio delivered {device_rate:.0} frames/s of wall time (nominal 48000)");

    // 2. Both at once, recorded with video through the whole session.
    let path = dir.join("av.mp4");
    let mut cfg = RecordConfig::new(&path);
    cfg.video = VideoSettings { hw: HwPolicy::SoftwareOnly, ..VideoSettings::default() };
    let video: Box<dyn FrameSource> =
        Box::new(SyntheticSource::new(SyntheticConfig::new(320, 240, Fps::FPS_30)));
    let audio: Vec<Box<dyn AudioSource>> = vec![
        Box::new(CpalSource::new(DeviceKind::SystemLoopback, DeviceSelector::Default)),
        Box::new(CpalSource::new(DeviceKind::Microphone, DeviceSelector::Default)),
    ];
    let s = RecordingSession::start(cfg, video, audio, &FfmpegProber).expect("start");
    assert!(s.info().has_audio, "warnings: {:?}", s.warnings());
    std::thread::sleep(Duration::from_millis(3000));
    let r = s.stop().unwrap();
    eprintln!("warnings: {:?}", r.warnings);
    let rep = inspect(&path).unwrap();
    let a = rep.audio.expect("audio track");
    let v = rep.video.unwrap();
    assert!(
        (a.decoded_seconds() - r.duration.as_secs_f64()).abs() < 0.15,
        "audio {} vs video {}",
        a.decoded_seconds(),
        r.duration.as_secs_f64()
    );
    let body = &a.samples[(0.5 * 48_000.0) as usize * 2..];
    let (t300, t700) = (tone_near(body, 2, 48_000.0, 300.0), tone_near(body, 2, 48_000.0, 700.0));
    eprintln!("recorded mix tone strength: 300 Hz {t300:.3}, 700 Hz {t700:.3}");
    assert!(
        t300 > 0.15 && t700 > 0.15,
        "both the system tone and the microphone tone are in the mix"
    );
    assert!(t300 + t700 > 0.6, "and they make up most of the signal");
    assert!(v.frame_count() > 80);
}
