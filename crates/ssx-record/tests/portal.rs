//! `ScreenCast` portal -> `PipeWire` recording, end to end without a desktop:
//!
//! * a private D-Bus session bus (`dbus-daemon`),
//! * a headless `pipewire` + `wireplumber` pair in a private runtime directory,
//! * a **video producer node** (`Video/Source`) drawing a moving box, standing in for the
//!   compositor's screen-cast node,
//! * a **mock `org.freedesktop.portal.ScreenCast`** service that implements the portal's
//!   request/response protocol (`CreateSession`, `SelectSources`, Start, `OpenPipeWireRemote`)
//!   and hands out a socket to the private `PipeWire` instance.
//!
//! `PortalSource` talks to all of that exactly as it would to GNOME or KDE. What this does
//! *not* prove: a real compositor's stream (buffer types, damage behaviour), and the
//! compositor's picker UI. Tests skip with a printed reason when `dbus-daemon`, `pipewire` or
//! `wireplumber` are missing.
#![cfg(all(target_os = "linux", feature = "portal", feature = "ffmpeg"))]
#![allow(clippy::too_many_lines, clippy::used_underscore_binding, clippy::cast_possible_wrap)] // zbus macro expansion; small ids

use std::{
    collections::HashMap,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{properties::properties, spa};
use ssx_record::{
    encode::{HwPolicy, VideoSettings, ffmpeg::FfmpegProber},
    error::SourceError,
    session::{RecordConfig, RecordingSession},
    source::{FrameSource, SourceConfig, portal::PortalSource},
    time::{Clock, Fps},
    verify::{RgbImage, inspect},
};
#[path = "pwstack/mod.rs"]
mod pwstack;
use pwstack::{Bus, PipeWire, connect_fd};
use zbus::{
    Connection, fdo,
    message::Header,
    zvariant::{self, ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

// ---- the video producer (stands in for the compositor's screen-cast node) --------------------

const W: u32 = 640;
const H: u32 = 360;
const BOX: u32 = 60;
const SPEED: f64 = 300.0;

struct Producer {
    quit: pw::channel::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
    node_id: u32,
}

struct ProdData {
    t0: Instant,
}

fn buffers_param() -> Vec<u8> {
    use spa::pod::{Object, Property, PropertyFlags, Value};
    let int =
        |key: u32, v: i32| Property { key, flags: PropertyFlags::empty(), value: Value::Int(v) };
    let obj = Object {
        type_: spa::sys::SPA_TYPE_OBJECT_ParamBuffers,
        id: spa::sys::SPA_PARAM_Buffers,
        properties: vec![
            Property {
                key: spa::sys::SPA_PARAM_BUFFERS_buffers,
                flags: PropertyFlags::empty(),
                value: Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                    spa::utils::ChoiceFlags::empty(),
                    spa::utils::ChoiceEnum::Range { default: 4, min: 2, max: 8 },
                ))),
            },
            int(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
            int(spa::sys::SPA_PARAM_BUFFERS_size, (W * H * 4) as i32),
            int(spa::sys::SPA_PARAM_BUFFERS_stride, (W * 4) as i32),
            int(spa::sys::SPA_PARAM_BUFFERS_dataType, 1 << spa::sys::SPA_DATA_MemFd),
        ],
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    )
    .expect("serialize buffers")
    .0
    .into_inner()
}

fn producer_format() -> Vec<u8> {
    use spa::param::{
        format::{FormatProperties, MediaSubtype, MediaType},
        video::VideoFormat,
    };
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle { width: W, height: H }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("serialize format")
    .0
    .into_inner()
}

impl Producer {
    fn start(socket: &Path) -> Producer {
        let fd = connect_fd(socket);
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (id_tx, id_rx) = mpsc::channel::<u32>();
        let thread = std::thread::spawn(move || {
            pw::init();
            let mainloop = pw::main_loop::MainLoopRc::new(None).expect("loop");
            let context = pw::context::ContextRc::new(&mainloop, None).expect("context");
            let core = context.connect_fd_rc(fd, None).expect("core");
            let stream = pw::stream::StreamBox::new(
                &core,
                "ssx-test-screen",
                properties! {
                    *pw::keys::MEDIA_CLASS => "Video/Source",
                    *pw::keys::NODE_NAME => "ssx-test-screen",
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                },
            )
            .expect("stream");
            let sent = std::cell::Cell::new(false);
            let _l = stream
                .add_local_listener_with_user_data(ProdData { t0: Instant::now() })
                .state_changed(move |st, _, _old, new| {
                    if matches!(
                        new,
                        pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
                    ) && !sent.replace(true)
                    {
                        let _ = id_tx.send(st.node_id());
                    }
                })
                .param_changed(|st, _, id, param| {
                    if param.is_some() && id == spa::param::ParamType::Format.as_raw() {
                        let bytes = buffers_param();
                        if let Some(pod) = spa::pod::Pod::from_bytes(&bytes) {
                            let _ = st.update_params(&mut [pod]);
                        }
                    }
                })
                .process(|st, ud| {
                    let Some(mut buf) = st.dequeue_buffer() else { return };
                    let Some(d) = buf.datas_mut().first_mut() else { return };
                    let x = ((ud.t0.elapsed().as_secs_f64() * SPEED) as u32) % (W - BOX);
                    let Some(bytes) = d.data() else { return };
                    let n = (W * H * 4) as usize;
                    if bytes.len() < n {
                        return;
                    }
                    bytes[..n].fill(0);
                    for row in (H / 2 - BOX / 2)..(H / 2 + BOX / 2) {
                        let start = ((row * W + x) * 4) as usize;
                        for px in bytes[start..start + (BOX * 4) as usize].chunks_exact_mut(4) {
                            px.copy_from_slice(&[0, 0, 255, 255]);
                        }
                    }
                    let c = d.chunk_mut();
                    *c.offset_mut() = 0;
                    *c.stride_mut() = (W * 4) as i32;
                    *c.size_mut() = W * H * 4;
                })
                .register()
                .expect("listener");
            let fmt = producer_format();
            let mut params = [spa::pod::Pod::from_bytes(&fmt).expect("pod")];
            stream
                .connect(
                    spa::utils::Direction::Output,
                    None,
                    pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )
                .expect("connect producer");
            let ml = mainloop.clone();
            let _q = quit_rx.attach(mainloop.loop_(), move |()| ml.quit());
            mainloop.run();
        });
        let node_id =
            id_rx.recv_timeout(Duration::from_secs(10)).expect("the producer node appeared");
        Producer { quit: quit_tx, thread: Some(thread), node_id }
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---- the mock portal ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behavior {
    Grant,
    Cancel,
}

struct MockScreenCast {
    node_id: u32,
    socket: PathBuf,
    behavior: Arc<Mutex<Behavior>>,
    log: Arc<Mutex<Vec<String>>>,
}

fn request_path(header: &Header<'_>, options: &HashMap<String, OwnedValue>) -> fdo::Result<String> {
    let token = match options.get("handle_token").map(|v| &**v) {
        Some(Value::Str(s)) => s.to_string(),
        _ => return Err(fdo::Error::InvalidArgs("handle_token missing".into())),
    };
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::Failed("no sender".into()))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    Ok(format!("/org/freedesktop/portal/desktop/request/{sender}/{token}"))
}

fn respond(
    conn: &Connection,
    path: &str,
    code: u32,
    results: HashMap<&'static str, Value<'static>>,
) {
    let (conn, path) = (conn.clone(), path.to_owned());
    std::thread::spawn(move || {
        let r = async_io::block_on(conn.emit_signal(
            None::<&str>,
            path.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        ));
        assert!(r.is_ok(), "emit Response: {r:?}");
    });
}

fn str_opt(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    match options.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Some(s.to_string()),
        _ => None,
    }
}

#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl MockScreenCast {
    #[zbus(property, name = "version")]
    #[allow(clippy::unused_self)]
    fn version(&self) -> u32 {
        5
    }

    #[zbus(property, name = "AvailableSourceTypes")]
    #[allow(clippy::unused_self)]
    fn source_types(&self) -> u32 {
        3
    }

    #[zbus(property, name = "AvailableCursorModes")]
    #[allow(clippy::unused_self)]
    fn cursor_modes(&self) -> u32 {
        7
    }

    fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let path = request_path(&header, &options)?;
        let token = str_opt(&options, "session_handle_token")
            .ok_or_else(|| fdo::Error::InvalidArgs("session_handle_token missing".into()))?;
        let sender = header
            .sender()
            .map(|s| s.as_str().trim_start_matches(':').replace('.', "_"))
            .unwrap_or_default();
        let session = format!("/org/freedesktop/portal/desktop/session/{sender}/{token}");
        self.log.lock().expect("log").push("CreateSession".into());
        let mut results = HashMap::new();
        results.insert("session_handle", Value::from(session));
        respond(conn, &path, 0, results);
        Ok(OwnedObjectPath::from(ObjectPath::try_from(path).map_err(zbus::Error::from)?))
    }

    fn select_sources(
        &self,
        _session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let path = request_path(&header, &options)?;
        let token = str_opt(&options, "restore_token").unwrap_or_else(|| "-".into());
        let cursor = options.get("cursor_mode").and_then(|v| match &**v {
            Value::U32(n) => Some(*n),
            _ => None,
        });
        self.log
            .lock()
            .expect("log")
            .push(format!("SelectSources restore_token={token} cursor_mode={cursor:?}"));
        respond(conn, &path, 0, HashMap::new());
        Ok(OwnedObjectPath::from(ObjectPath::try_from(path).map_err(zbus::Error::from)?))
    }

    fn start(
        &self,
        _session_handle: ObjectPath<'_>,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let path = request_path(&header, &options)?;
        self.log.lock().expect("log").push("Start".into());
        if *self.behavior.lock().expect("behavior") == Behavior::Cancel {
            respond(conn, &path, 1, HashMap::new());
        } else {
            let mut props: HashMap<String, Value<'static>> = HashMap::new();
            props.insert("source_type".into(), Value::from(1u32));
            props.insert("size".into(), Value::from((W as i32, H as i32)));
            let streams: Vec<(u32, HashMap<String, Value<'static>>)> = vec![(self.node_id, props)];
            let mut results: HashMap<&'static str, Value<'static>> = HashMap::new();
            results.insert("streams", Value::new(streams));
            results.insert("restore_token", Value::from("mock-restore-token-1".to_owned()));
            results.insert("persist_mode", Value::from(2u32));
            respond(conn, &path, 0, results);
        }
        Ok(OwnedObjectPath::from(ObjectPath::try_from(path).map_err(zbus::Error::from)?))
    }

    fn open_pipe_wire_remote(
        &self,
        _session_handle: ObjectPath<'_>,
        _options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<zvariant::OwnedFd> {
        self.log.lock().expect("log").push("OpenPipeWireRemote".into());
        let s = UnixStream::connect(&self.socket).map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok(zvariant::OwnedFd::from(OwnedFd::from(s)))
    }
}

struct Portal {
    behavior: Arc<Mutex<Behavior>>,
    log: Arc<Mutex<Vec<String>>>,
    _conn: Connection,
}

fn start_portal(bus: &Bus, node_id: u32, socket: &Path) -> Portal {
    let behavior = Arc::new(Mutex::new(Behavior::Grant));
    let log = Arc::new(Mutex::new(Vec::new()));
    let mock = MockScreenCast {
        node_id,
        socket: socket.to_path_buf(),
        behavior: behavior.clone(),
        log: log.clone(),
    };
    let conn = async_io::block_on(async {
        bus.service("org.freedesktop.portal.Desktop")
            .serve_at("/org/freedesktop/portal/desktop", mock)
            .expect("serve")
            .build()
            .await
            .expect("portal connection")
    });
    Portal { behavior, log, _conn: conn }
}

fn box_x(img: &RgbImage, row: u32) -> Option<f64> {
    let (mut sum, mut n) = (0u64, 0u64);
    for x in 0..img.width {
        let [r, g, b] = img.pixel(x, row);
        if r > 180 && g < 90 && b < 90 {
            sum += u64::from(x);
            n += 1;
        }
    }
    (n > 10).then(|| sum as f64 / n as f64)
}

fn sw_config(path: &Path) -> RecordConfig {
    let mut c = RecordConfig::new(path);
    c.video = VideoSettings { hw: HwPolicy::SoftwareOnly, ..VideoSettings::default() };
    c
}

fn portal_source(bus: &Bus, token: &Path) -> PortalSource {
    PortalSource::new(SourceConfig { fps: Fps::FPS_30, ..SourceConfig::default() })
        .with_bus_address(bus.address.clone())
        .with_token_path(Some(token.to_path_buf()))
}

#[test]
fn portal_screencast_records_the_pipewire_stream_and_remembers_the_token() {
    let Some(bus) = Bus::start() else { return };
    let Some(pw) = PipeWire::start(&bus) else { return };
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("SKIP: the ffmpeg binary is needed to dump frames");
        return;
    }
    let producer = Producer::start(&pw.socket());
    let portal = start_portal(&bus, producer.node_id, &pw.socket());
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("restore-token");

    // First recording: no token yet, the portal returns one.
    let path = dir.path().join("portal.mp4");
    let src = portal_source(&bus, &token_file);
    let s = RecordingSession::start(sw_config(&path), Box::new(src), vec![], &FfmpegProber)
        .expect("start");
    assert_eq!(s.info().source, "pipewire");
    assert_eq!((s.info().size.width, s.info().size.height), (W, H));
    std::thread::sleep(Duration::from_millis(2500));
    let r = s.stop().unwrap();
    eprintln!("portal recording: {:?}", r.stats);
    assert!(r.stats.encoded > 45, "the stream must deliver frames: {:?}", r.stats);
    assert!((r.duration.as_secs_f64() - 2.5).abs() < 0.4, "{:?}", r.duration);

    let log = portal.log.lock().unwrap().clone();
    assert_eq!(
        log[..4],
        [
            "CreateSession",
            "SelectSources restore_token=- cursor_mode=Some(2)",
            "Start",
            "OpenPipeWireRemote"
        ],
        "{log:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap(),
        "mock-restore-token-1",
        "the token is persisted"
    );

    // Decode: the box moves at the producer's speed.
    let rep = inspect(&path).unwrap();
    let v = rep.video.clone().unwrap();
    assert_eq!((v.width, v.height), (W, H));
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .unwrap();
    let xs: Vec<f64> = out
        .stdout
        .chunks_exact((W * H * 3) as usize)
        .filter_map(|c| box_x(&RgbImage { width: W, height: H, data: c.to_vec() }, H / 2))
        .collect();
    assert!(xs.len() >= 45, "box visible in {} of {} frames", xs.len(), v.frame_count());
    let span = f64::from(W - BOX);
    let mut deltas: Vec<f64> = xs
        .windows(2)
        .map(|w| {
            let d = w[1] - w[0];
            if d < -span / 2.0 { d + span } else { d }
        })
        .filter(|d| *d != 0.0)
        .collect();
    deltas.sort_by(f64::total_cmp);
    let median = deltas[deltas.len() / 2];
    // 300 px/s and consecutive distinct frames at least 1/30 s apart: 10 px or more.
    assert!((5.0..=40.0).contains(&median), "median displacement {median}");
    let moving = xs.windows(2).filter(|w| (w[1] - w[0]).abs() > 2.0).count();
    assert!(moving as f64 > xs.len() as f64 * 0.7, "moved in {moving} of {} steps", xs.len());

    // Second recording: the stored token is offered to the portal.
    let path2 = dir.path().join("portal2.mp4");
    let src = portal_source(&bus, &token_file);
    let s = RecordingSession::start(sw_config(&path2), Box::new(src), vec![], &FfmpegProber)
        .expect("start again");
    std::thread::sleep(Duration::from_millis(600));
    s.stop().unwrap();
    let log = portal.log.lock().unwrap().clone();
    assert!(
        log.iter()
            .any(|l| l == "SelectSources restore_token=mock-restore-token-1 cursor_mode=Some(2)"),
        "second start must reuse the token: {log:?}"
    );
    drop(producer);
}

#[test]
fn portal_cancel_is_reported_as_permission_denied() {
    let Some(bus) = Bus::start() else { return };
    let Some(pw) = PipeWire::start(&bus) else { return };
    let producer = Producer::start(&pw.socket());
    let portal = start_portal(&bus, producer.node_id, &pw.socket());
    *portal.behavior.lock().unwrap() = Behavior::Cancel;
    let dir = tempfile::tempdir().unwrap();
    let mut src = portal_source(&bus, &dir.path().join("tok"));
    let err = src.start(Clock::start()).unwrap_err();
    assert!(matches!(err, SourceError::PermissionDenied(_)), "{err:?}");
    assert!(!dir.path().join("tok").exists());
}

#[test]
fn a_bus_without_a_portal_is_reported_clearly() {
    let Some(bus) = Bus::start() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut src = portal_source(&bus, &dir.path().join("tok"));
    let t = Instant::now();
    let err = src.start(Clock::start()).unwrap_err();
    eprintln!("no portal: {err}");
    assert!(t.elapsed() < Duration::from_secs(30));
    assert!(matches!(err, SourceError::Unavailable(_)), "{err:?}");
}
