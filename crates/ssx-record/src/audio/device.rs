//! Real audio devices through `cpal`: microphones and system-audio loopback.
//!
//! What `cpal` 0.18 supports (checked in its source, not assumed):
//!
//! | Platform | Microphone | System audio ("loopback") |
//! |---|---|---|
//! | Windows (WASAPI) | default input | an **input stream opened on an output device** turns on `AUDCLNT_STREAMFLAGS_LOOPBACK` |
//! | Linux, PipeWire host | default source | sinks are exposed as duplex devices; an input stream on one sets `stream.capture.sink`, i.e. captures what plays to that sink |
//! | Linux, PulseAudio host | default source | no loopback API, but monitor sources are ordinary sources: we pick the input device whose name says "monitor" |
//! | Linux, plain ALSA | default | none (only via a sound-server plugin) |
//! | macOS (CoreAudio) | default input | aggregate-device loopback, macOS 14.6+ |
//!
//! So [`CpalSource`] needs no extra dependency for loopback: it prefers the PipeWire host,
//! then PulseAudio, then whatever `cpal` defaults to, and reports a clear
//! [`AudioError::NoDevice`] when a host offers no way to capture the system output.
//!
//! The stream lives on a dedicated worker thread (some `cpal` streams are `!Send`), which
//! also makes the source itself trivially `Send`. The device callback timestamps every
//! block with the shared clock (arrival time minus the reported capture latency minus the
//! block length = time of its first sample) and pushes it into a bounded channel; a full
//! channel drops the block and counts it rather than blocking the audio thread.

use std::{
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::JoinHandle,
    time::Duration,
};

use cpal::{
    Device, InputCallbackInfo, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

use super::{AudioChunk, AudioEvent, AudioFormat, AudioSource};
use crate::{error::AudioError, time::Clock};

/// What to capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// A microphone or line-in.
    Microphone,
    /// Whatever the system is playing (loopback of an output device).
    SystemLoopback,
}

/// Which device of the [`DeviceKind`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeviceSelector {
    /// The system default.
    #[default]
    Default,
    /// The first device whose name contains this text (case-insensitive).
    Name(String),
}

/// A device found by [`list_devices`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Audio host (`PipeWire`, `WASAPI`, ...).
    pub host: String,
    /// Device name.
    pub name: String,
    /// It can be used as a microphone.
    pub input: bool,
    /// It can be used for loopback (an output that can be captured).
    pub loopback: bool,
}

fn err(e: &cpal::Error) -> AudioError {
    use cpal::ErrorKind as K;
    match e.kind() {
        K::DeviceNotAvailable | K::HostUnavailable => AudioError::NoDevice(e.to_string()),
        K::UnsupportedConfig => AudioError::UnsupportedFormat(e.to_string()),
        _ => AudioError::Backend { backend: "cpal", message: e.to_string() },
    }
}

/// Available hosts in the order we prefer them.
fn hosts() -> Vec<cpal::Host> {
    let mut ids = cpal::available_hosts();
    let rank = |name: &str| match name.to_ascii_lowercase().as_str() {
        "pipewire" => 0,
        "pulseaudio" => 1,
        "wasapi" | "coreaudio" => 0,
        "jack" => 8,
        _ => 5,
    };
    ids.sort_by_key(|id| rank(id.name()));
    ids.into_iter().filter_map(|id| cpal::host_from_id(id).ok()).collect()
}

fn device_name(d: &Device) -> String {
    d.description().map_or_else(|_| "unknown".to_owned(), |d| d.name().to_owned())
}

/// Lists the devices of every available host.
pub fn list_devices() -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    for host in hosts() {
        let host_name = host.id().name().to_owned();
        let Ok(devices) = host.devices() else { continue };
        for d in devices {
            let name = device_name(&d);
            let input = d.supports_input();
            let output = d.supports_output();
            let is_monitor = name.to_ascii_lowercase().contains("monitor");
            out.push(DeviceInfo {
                host: host_name.clone(),
                name,
                input: input && !output && !is_monitor,
                loopback: (input && output) || is_monitor || (output && cfg!(windows)),
            });
        }
    }
    out
}

/// Picks the device for `kind` on `host`, or explains why there is none.
fn pick_device(
    host: &cpal::Host,
    kind: DeviceKind,
    sel: &DeviceSelector,
) -> Result<(Device, String), AudioError> {
    let matches = |d: &Device| match sel {
        DeviceSelector::Default => true,
        DeviceSelector::Name(n) => {
            device_name(d).to_ascii_lowercase().contains(&n.to_ascii_lowercase())
        }
    };
    match kind {
        DeviceKind::Microphone => {
            let dev = match sel {
                DeviceSelector::Default => host.default_input_device(),
                DeviceSelector::Name(_) => {
                    host.input_devices().ok().and_then(|mut it| it.find(|d| matches(d)))
                }
            };
            dev.map(|d| {
                let n = device_name(&d);
                (d, n)
            })
            .ok_or_else(|| AudioError::NoDevice("no microphone found".into()))
        }
        DeviceKind::SystemLoopback => {
            // 1. An output device that the host lets us open as input (WASAPI, PipeWire).
            let out = match sel {
                DeviceSelector::Default => host.default_output_device(),
                DeviceSelector::Name(_) => host
                    .devices()
                    .ok()
                    .and_then(|mut it| it.find(|d| d.supports_output() && matches(d))),
            };
            if let Some(d) = out
                && d.supports_input()
                && d.default_input_config().is_ok()
            {
                let n = device_name(&d);
                return Ok((d, n));
            }
            // WASAPI reports `supports_input() == false` for a render endpoint but opens it
            // in loopback mode anyway; PulseAudio has monitor sources instead.
            #[cfg(windows)]
            if let Some(d) = host.default_output_device() {
                let n = device_name(&d);
                return Ok((d, n));
            }
            // 2. A monitor source (PulseAudio/ALSA-over-pulse naming).
            let monitor = host.input_devices().ok().and_then(|mut it| {
                it.find(|d| {
                    let n = device_name(d).to_ascii_lowercase();
                    n.contains("monitor") && matches(d)
                })
            });
            monitor
                .map(|d| {
                    let n = device_name(&d);
                    (d, n)
                })
                .ok_or_else(|| {
                    AudioError::NoDevice(format!(
                        "the `{}` audio host has no way to capture the system output \
                         (no loopback and no monitor source); on Linux use PipeWire or \
                         PulseAudio",
                        host.id().name()
                    ))
                })
        }
    }
}

/// Chooses a stream configuration: prefer `f32`, otherwise the device default format.
fn pick_config(dev: &Device) -> Result<(StreamConfig, SampleFormat), AudioError> {
    let default = dev.default_input_config().map_err(|e| err(&e))?;
    let want = default.sample_rate();
    let f32_cfg = dev.supported_input_configs().ok().and_then(|it| {
        it.filter(|c| c.sample_format() == SampleFormat::F32).find(|c| {
            c.channels() == default.channels()
                && c.min_sample_rate() <= want
                && want <= c.max_sample_rate()
        })
    });
    match f32_cfg {
        Some(c) => Ok((c.with_sample_rate(want).into(), SampleFormat::F32)),
        None => Ok((default.clone().into(), default.sample_format())),
    }
}

struct Ready {
    format: AudioFormat,
    label: String,
}

/// A microphone or loopback capture through `cpal`. See the module docs.
pub struct CpalSource {
    kind: DeviceKind,
    selector: DeviceSelector,
    label: String,
    rx: Option<Receiver<AudioChunk>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
    dropped: Arc<AtomicU64>,
}

impl std::fmt::Debug for CpalSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CpalSource")
            .field("kind", &self.kind)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl CpalSource {
    /// A source for `kind` and `selector`; the device is opened by `start`.
    pub fn new(kind: DeviceKind, selector: DeviceSelector) -> Self {
        let label = match kind {
            DeviceKind::Microphone => "microphone",
            DeviceKind::SystemLoopback => "system audio",
        };
        Self {
            kind,
            selector,
            label: label.to_owned(),
            rx: None,
            stop: None,
            thread: None,
            failure: Arc::default(),
            dropped: Arc::default(),
        }
    }

    /// Blocks that were dropped because the consumer was too slow.
    pub fn dropped_blocks(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn worker(
    kind: DeviceKind,
    sel: DeviceSelector,
    clock: Clock,
    tx: SyncSender<AudioChunk>,
    ready: &mpsc::Sender<Result<Ready, AudioError>>,
    stop: &Receiver<()>,
    failure: Arc<Mutex<Option<String>>>,
    dropped: Arc<AtomicU64>,
) {
    let opened = (|| {
        let mut last = AudioError::NoDevice("no audio host is available".into());
        for host in hosts() {
            let host_name = host.id().name().to_owned();
            let (dev, name) = match pick_device(&host, kind, &sel) {
                Ok(x) => x,
                Err(e) => {
                    tracing::debug!(host = %host_name, error = %e, "audio host cannot provide the device");
                    last = e;
                    continue;
                }
            };
            match open_stream(&dev, clock, tx.clone(), &failure, &dropped) {
                Ok((stream, format)) => {
                    return Ok((stream, Ready { format, label: format!("{name} [{host_name}]") }));
                }
                Err(e) => {
                    tracing::debug!(host = %host_name, device = %name, error = %e, "cannot open the audio stream");
                    last = e;
                }
            }
        }
        Err(last)
    })();
    match opened {
        Ok((stream, info)) => {
            let _ = ready.send(Ok(info));
            // Hold the stream until asked to stop (or the source is dropped).
            let _ = stop.recv();
            drop(stream);
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

fn open_stream(
    dev: &Device,
    clock: Clock,
    tx: SyncSender<AudioChunk>,
    failure: &Arc<Mutex<Option<String>>>,
    dropped: &Arc<AtomicU64>,
) -> Result<(Stream, AudioFormat), AudioError> {
    let (config, fmt) = pick_config(dev)?;
    let format = AudioFormat { sample_rate: config.sample_rate, channels: config.channels };
    let rate = f64::from(config.sample_rate);
    let ch = usize::from(config.channels);
    let failure2 = Arc::clone(failure);
    let on_error = move |e: cpal::Error| {
        if e.kind() == cpal::ErrorKind::DeviceChanged {
            return;
        }
        *failure2.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.to_string());
    };
    let dropped2 = Arc::clone(dropped);
    let send = move |samples: Vec<f32>, info: &InputCallbackInfo| {
        let frames = samples.len() / ch.max(1);
        let ts = info.timestamp();
        let latency = ts.callback.duration_since(ts.capture);
        let dur = Duration::from_secs_f64(frames as f64 / rate);
        let timestamp = clock.now().saturating_sub(latency).saturating_sub(dur);
        if tx.try_send(AudioChunk { samples, timestamp }).is_err() {
            dropped2.fetch_add(1, Ordering::Relaxed);
        }
    };
    macro_rules! build {
        ($t:ty, $conv:expr) => {{
            let send = send;
            dev.build_input_stream(
                config,
                move |data: &[$t], info: &InputCallbackInfo| {
                    let conv = $conv;
                    send(data.iter().map(|s| conv(*s)).collect(), info);
                },
                on_error,
                None,
            )
        }};
    }
    let stream = match fmt {
        SampleFormat::F32 => build!(f32, |s: f32| s),
        SampleFormat::I16 => build!(i16, |s: i16| f32::from(s) / 32768.0),
        SampleFormat::U16 => build!(u16, |s: u16| (f32::from(s) - 32768.0) / 32768.0),
        SampleFormat::I32 => build!(i32, |s: i32| s as f32 / 2_147_483_648.0),
        other => {
            return Err(AudioError::UnsupportedFormat(format!("sample format {other:?}")));
        }
    }
    .map_err(|e| err(&e))?;
    stream.play().map_err(|e| err(&e))?;
    Ok((stream, format))
}

impl AudioSource for CpalSource {
    fn name(&self) -> String {
        self.label.clone()
    }

    fn start(&mut self, clock: Clock) -> Result<AudioFormat, AudioError> {
        let (tx, rx) = mpsc::sync_channel(256);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (kind, sel) = (self.kind, self.selector.clone());
        let failure = Arc::clone(&self.failure);
        let dropped = Arc::clone(&self.dropped);
        let thread = std::thread::Builder::new()
            .name("ssx-audio-capture".into())
            .spawn(move || worker(kind, sel, clock, tx, &ready_tx, &stop_rx, failure, dropped))
            .map_err(|e| AudioError::Backend { backend: "cpal", message: e.to_string() })?;
        let ready =
            ready_rx.recv_timeout(Duration::from_secs(10)).map_err(|_| AudioError::Backend {
                backend: "cpal",
                message: "opening the audio device timed out".into(),
            })??;
        self.label = format!("{}: {}", self.label, ready.label);
        self.rx = Some(rx);
        self.stop = Some(stop_tx);
        self.thread = Some(thread);
        Ok(ready.format)
    }

    fn read(&mut self, timeout: Duration) -> Result<AudioEvent, AudioError> {
        let Some(rx) = &self.rx else { return Ok(AudioEvent::Ended) };
        match rx.recv_timeout(timeout) {
            Ok(c) => Ok(AudioEvent::Chunk(c)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let f = self.failure.lock().unwrap_or_else(PoisonError::into_inner).take();
                match f {
                    Some(message) => Err(AudioError::Backend { backend: "cpal", message }),
                    None => Ok(AudioEvent::Timeout),
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => match rx.try_recv() {
                Ok(c) => Ok(AudioEvent::Chunk(c)),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(AudioEvent::Ended),
            },
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CpalSource {
    fn drop(&mut self) {
        self.stop();
    }
}
