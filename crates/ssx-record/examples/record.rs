//! Records the screen for a while and writes a video file.
//!
//! ```text
//! cargo run -p ssx-record --release --example record -- \
//!     --seconds 10 --fps 30 -o out.mp4 [--source x11|wayland|portal|windows|synthetic] [--audio] [--mic]
//! ```
//!
//! * `--source` defaults to `auto` (Windows: WGC; Wayland: wlroots protocols, then the
//!   portal; otherwise X11). `synthetic` records colour bars with a burnt-in frame counter
//!   and, with `--audio`, a synthetic tone, so the pipeline can be tried anywhere.
//! * `--audio` records system audio (loopback); `--mic` the default microphone. Audio that
//!   cannot be opened is reported and the recording continues without it.
//! * The container follows the extension: `.mp4`, `.webm`, `.mkv` or `.gif`.
//! * `--software` skips hardware encoders (useful to compare).
//!
//! Ctrl-C is not handled: the recording stops after `--seconds`, then the file is finalised.

use std::{path::PathBuf, process::ExitCode, time::Duration};

use ssx_record::{
    audio::{
        AudioSource,
        synth::{Signal, SyntheticAudio, SyntheticAudioConfig},
    },
    encode::HwPolicy,
    session::{RecordConfig, RecordingSession},
    source::{
        CaptureTarget, SourceConfig,
        auto::{SourceKind, open},
        synthetic::TimeMode,
    },
    time::Fps,
};

struct Args {
    seconds: f64,
    fps: Fps,
    output: PathBuf,
    source: SourceKind,
    audio: bool,
    mic: bool,
    software: bool,
}

fn usage() -> &'static str {
    "usage: record [--seconds N] [--fps N] [-o FILE] [--source auto|x11|wayland|portal|windows|synthetic] [--audio] [--mic] [--software]"
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        seconds: 5.0,
        fps: Fps::FPS_30,
        output: PathBuf::from("recording.mp4"),
        source: SourceKind::Auto,
        audio: false,
        mic: false,
        software: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--seconds" | "-t" => {
                a.seconds = value("--seconds")?.parse().map_err(|e| format!("--seconds: {e}"))?;
                if a.seconds.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                    return Err("--seconds must be positive".into());
                }
            }
            "--fps" => {
                let f: f64 = value("--fps")?.parse().map_err(|e| format!("--fps: {e}"))?;
                a.fps = Fps::from_f64(f).ok_or_else(|| format!("--fps {f} is not usable"))?;
            }
            "-o" | "--output" => a.output = PathBuf::from(value("-o")?),
            "--source" => {
                let s = value("--source")?;
                a.source = SourceKind::parse(&s).ok_or_else(|| format!("unknown source `{s}`"))?;
            }
            "--audio" => a.audio = true,
            "--mic" => a.mic = true,
            "--software" => a.software = true,
            "-h" | "--help" => return Err(usage().into()),
            other => return Err(format!("unknown argument `{other}`\n{}", usage())),
        }
    }
    Ok(a)
}

fn audio_sources(a: &Args) -> Vec<Box<dyn AudioSource>> {
    let mut v: Vec<Box<dyn AudioSource>> = Vec::new();
    if a.source == SourceKind::Synthetic {
        if a.audio {
            let signal = Signal::Tone { hz: 440.0, amplitude: 0.4, start: Duration::ZERO };
            v.push(Box::new(SyntheticAudio::new(SyntheticAudioConfig::new(
                signal,
                TimeMode::Realtime,
            ))));
        }
        return v;
    }
    #[cfg(feature = "audio")]
    {
        use ssx_record::audio::device::{CpalSource, DeviceKind, DeviceSelector};
        if a.audio {
            v.push(Box::new(CpalSource::new(DeviceKind::SystemLoopback, DeviceSelector::Default)));
        }
        if a.mic {
            v.push(Box::new(CpalSource::new(DeviceKind::Microphone, DeviceSelector::Default)));
        }
    }
    #[cfg(not(feature = "audio"))]
    if a.audio || a.mic {
        eprintln!("audio was not compiled in (feature `audio`); recording video only");
    }
    v
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let source_cfg = SourceConfig { target: CaptureTarget::Desktop, fps: args.fps, cursor: true };
    let source = open(args.source, source_cfg)?;
    let mut cfg = RecordConfig::new(&args.output);
    cfg.fps = args.fps;
    if args.software {
        cfg.video.hw = HwPolicy::SoftwareOnly;
    }
    let audio = audio_sources(args);

    #[cfg(feature = "ffmpeg")]
    let prober = ssx_record::encode::ffmpeg::FfmpegProber;
    #[cfg(not(feature = "ffmpeg"))]
    let prober = ssx_record::adapter::NoFfmpegProber;

    eprintln!(
        "recording {} s at {} fps from `{}` to {}",
        args.seconds,
        args.fps.as_f64(),
        source_name(args.source),
        args.output.display()
    );
    let session = RecordingSession::start(cfg, source, audio, &prober)?;
    let info = session.info().clone();
    eprintln!(
        "source {}x{}, encoder: {}, audio: {}",
        info.size.width,
        info.size.height,
        info.encoder,
        if info.has_audio { "yes" } else { "no" }
    );
    for w in session.warnings() {
        eprintln!("warning: {w}");
    }
    // Ends early if the source ends (window closed) or a limit is hit.
    let _ = session.wait_finished(Duration::from_secs_f64(args.seconds));
    let rec = session.stop()?;
    for w in &rec.warnings {
        eprintln!("warning: {w}");
    }
    let s = &rec.stats;
    println!(
        "wrote {} ({} bytes, {:.2} s, {}x{}, {}{})",
        rec.path.display(),
        rec.file_bytes,
        rec.duration.as_secs_f64(),
        rec.size.width,
        rec.size.height,
        rec.encoder,
        if rec.has_audio { ", with audio" } else { "" }
    );
    println!(
        "frames: {} encoded of {} slots ({} captured, {} repeated, {} dropped under load); ended: {:?}",
        s.encoded,
        s.slots,
        s.captured,
        s.duplicated,
        s.dropped_real(),
        rec.end_reason
    );
    Ok(())
}

fn source_name(k: SourceKind) -> &'static str {
    match k {
        SourceKind::Auto => "auto",
        SourceKind::X11 => "x11",
        SourceKind::Wayland => "wayland",
        SourceKind::Portal => "portal",
        SourceKind::Windows => "windows",
        SourceKind::Synthetic => "synthetic",
    }
}
