//! The GlobalShortcuts portal backend against a mock `xdg-desktop-portal` on a private
//! D-Bus session bus.
//!
//! The mock implements the interface as the portal spec describes it (request handles and
//! `Response` signals, sessions, `Activated`/`Deactivated`), so this exercises the real
//! `ashpd` client and our worker thread end to end. It cannot say how KDE's or GNOME's
//! actual implementations behave; see the module docs of the backend for what is known.
//!
//! Skips when `dbus-daemon` is missing.
#![cfg(target_os = "linux")]

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command as Process, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Serialize;
use ssx_hotkeys::{Chord, HotkeyError, HotkeyId, HotkeyManager, HotkeyState, PortalHotkeys};
use zbus::{
    Connection, fdo,
    message::Header,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Type, Value, as_value},
};

struct Bus {
    child: Child,
    address: String,
    dir: PathBuf,
}

impl Bus {
    fn start(tag: &str) -> Option<Bus> {
        let dir = std::env::temp_dir().join(format!("ssx-hk-portal-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok()?;
        let socket = dir.join("bus");
        let config = format!(
            "<!DOCTYPE busconfig PUBLIC \"-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN\" \
             \"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd\">\n\
             <busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth>\
             <policy context=\"default\"><allow send_destination=\"*\" eavesdrop=\"true\"/>\
             <allow eavesdrop=\"true\"/><allow own=\"*\"/></policy></busconfig>",
            socket.display()
        );
        let conf = dir.join("bus.conf");
        std::fs::write(&conf, config).ok()?;
        let mut child = match Process::new("dbus-daemon")
            .arg("--nofork")
            .arg("--print-address=1")
            .arg(format!("--config-file={}", conf.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("SKIP: cannot run dbus-daemon ({e}); install the `dbus` package");
                return None;
            }
        };
        let mut line = String::new();
        BufReader::new(child.stdout.take()?).read_line(&mut line).ok()?;
        if line.trim().is_empty() {
            eprintln!("SKIP: dbus-daemon printed no address");
            let _ = child.kill();
            return None;
        }
        Some(Bus { child, address: line.trim().to_owned(), dir })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Default)]
struct Log {
    creates: usize,
    sessions: Vec<String>,
    /// One entry per BindShortcuts call: `(id, preferred_trigger)` per shortcut.
    binds: Vec<Vec<(String, Option<String>)>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Accept,
    /// The user dismissed the compositor's dialog: Response code 1.
    Cancel,
}

struct MockGlobalShortcuts {
    log: Arc<Mutex<Log>>,
    mode: Arc<Mutex<Mode>>,
}

#[derive(Serialize, Type)]
#[zvariant(signature = "dict")]
struct SessionResults {
    #[serde(with = "as_value")]
    session_handle: String,
}

#[derive(Serialize, Type)]
#[zvariant(signature = "dict")]
struct BindResults {
    #[serde(with = "as_value")]
    shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
}

#[derive(Serialize, Type)]
#[zvariant(signature = "dict")]
struct Empty {}

fn token(options: &HashMap<String, OwnedValue>, key: &str) -> fdo::Result<String> {
    match options.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => Ok(s.to_string()),
        _ => Err(fdo::Error::InvalidArgs(format!("{key} missing"))),
    }
}

fn sender_part(header: &Header<'_>) -> fdo::Result<String> {
    Ok(header
        .sender()
        .ok_or_else(|| fdo::Error::Failed("no sender".into()))?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_"))
}

fn respond<T: Serialize + Type + Send + 'static>(conn: &Connection, request_path: String, code: u32, results: T) {
    let conn = conn.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        let r = async_io::block_on(conn.emit_signal(
            None::<&str>,
            request_path.as_str(),
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        ));
        assert!(r.is_ok(), "emit Response: {r:?}");
    });
}

#[zbus::interface(name = "org.freedesktop.portal.GlobalShortcuts")]
impl MockGlobalShortcuts {
    #[zbus(property, name = "version")]
    #[allow(clippy::unused_self)] // signature dictated by the zbus property macro
    fn version(&self) -> u32 {
        1
    }

    fn create_session(
        &self,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let sender = sender_part(&header)?;
        let request = format!("/org/freedesktop/portal/desktop/request/{sender}/{}", token(&options, "handle_token")?);
        let session = format!(
            "/org/freedesktop/portal/desktop/session/{sender}/{}",
            token(&options, "session_handle_token")?
        );
        {
            let mut log = self.log.lock().expect("log");
            log.creates += 1;
            log.sessions.push(session.clone());
        }
        respond(conn, request.clone(), 0, SessionResults { session_handle: session });
        Ok(OwnedObjectPath::from(ObjectPath::try_from(request).map_err(zbus::Error::from)?))
    }

    fn bind_shortcuts(
        &self,
        _session_handle: ObjectPath<'_>,
        shortcuts: Vec<(String, HashMap<String, OwnedValue>)>,
        _parent_window: String,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
    ) -> fdo::Result<OwnedObjectPath> {
        let sender = sender_part(&header)?;
        let request = format!("/org/freedesktop/portal/desktop/request/{sender}/{}", token(&options, "handle_token")?);
        let asked: Vec<(String, Option<String>)> = shortcuts
            .iter()
            .map(|(id, info)| {
                let trig = info.get("preferred_trigger").and_then(|v| match &**v {
                    Value::Str(s) => Some(s.to_string()),
                    _ => None,
                });
                (id.clone(), trig)
            })
            .collect();
        self.log.lock().expect("log").binds.push(asked.clone());
        if *self.mode.lock().expect("mode") == Mode::Cancel {
            respond(conn, request.clone(), 1, Empty {});
        } else {
            let bound = asked
                .into_iter()
                .map(|(id, trig)| {
                    let mut info: HashMap<String, OwnedValue> = HashMap::new();
                    info.insert("description".into(), Value::from(id.clone()).try_to_owned().expect("owned"));
                    // The mock "compositor" reports the trigger in its own words.
                    let desc = format!("bound: {}", trig.unwrap_or_default());
                    info.insert("trigger_description".into(), Value::from(desc).try_to_owned().expect("owned"));
                    (id, info)
                })
                .collect();
            respond(conn, request.clone(), 0, BindResults { shortcuts: bound });
        }
        Ok(OwnedObjectPath::from(ObjectPath::try_from(request).map_err(zbus::Error::from)?))
    }
}

struct Portal {
    conn: Connection,
    log: Arc<Mutex<Log>>,
    mode: Arc<Mutex<Mode>>,
}

impl Portal {
    fn start(bus: &Bus) -> Portal {
        let log = Arc::new(Mutex::new(Log::default()));
        let mode = Arc::new(Mutex::new(Mode::Accept));
        let mock = MockGlobalShortcuts { log: log.clone(), mode: mode.clone() };
        let conn = async_io::block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())
                .expect("address")
                .name("org.freedesktop.portal.Desktop")
                .expect("name")
                .serve_at("/org/freedesktop/portal/desktop", mock)
                .expect("serve")
                .build()
                .await
                .expect("portal connection")
        });
        Portal { conn, log, mode }
    }

    fn current_session(&self) -> String {
        self.log.lock().expect("log").sessions.last().cloned().expect("a session exists")
    }

    fn signal(&self, member: &str, session: &str, shortcut: &str) {
        let path = ObjectPath::try_from(session.to_owned()).expect("path");
        let opts: HashMap<&str, Value<'_>> = HashMap::new();
        async_io::block_on(self.conn.emit_signal(
            None::<&str>,
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.GlobalShortcuts",
            member,
            &(path, shortcut, 1234u64, opts),
        ))
        .expect("emit signal");
    }
}

fn id(s: &str) -> HotkeyId {
    HotkeyId::new(s).expect("id")
}

fn chord(s: &str) -> Chord {
    s.parse().expect("chord")
}

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn registers_through_the_portal_and_delivers_activations() {
    let Some(bus) = Bus::start("basic") else { return };
    let portal = Portal::start(&bus);
    let mut mgr = PortalHotkeys::connect_to(Some(&bus.address)).expect("connect");

    mgr.register(id("capture-region"), chord("Ctrl+Shift+S")).expect("register");
    {
        let log = portal.log.lock().expect("log");
        assert_eq!(log.creates, 1);
        assert_eq!(log.binds, vec![vec![("capture-region".to_owned(), Some("CTRL+SHIFT+s".to_owned()))]]);
    }
    assert_eq!(mgr.triggers().get("capture-region").map(String::as_str), Some("bound: CTRL+SHIFT+s"));
    assert_eq!(mgr.registered(), vec![(id("capture-region"), chord("Ctrl+Shift+S"))]);
    assert_eq!(mgr.backend().to_string(), "xdg-portal");
    assert!(!mgr.reports_release(), "release delivery is not guaranteed on the portal");

    let session = portal.current_session();
    portal.signal("Activated", &session, "capture-region");
    let ev = mgr.events().recv_timeout(WAIT).expect("activation");
    assert_eq!((ev.id, ev.state), (id("capture-region"), HotkeyState::Pressed));

    // Where a portal does send Deactivated, it arrives as Released.
    portal.signal("Deactivated", &session, "capture-region");
    let ev = mgr.events().recv_timeout(WAIT).expect("deactivation");
    assert_eq!(ev.state, HotkeyState::Released);

    // Unknown ids are dropped, known ones keep working afterwards.
    portal.signal("Activated", &session, "not-ours");
    portal.signal("Activated", &session, "capture-region");
    assert_eq!(mgr.events().recv_timeout(WAIT).expect("still alive").id, id("capture-region"));
}

#[test]
fn every_change_rebinds_the_whole_set_in_a_fresh_session() {
    let Some(bus) = Bus::start("rebind") else { return };
    let portal = Portal::start(&bus);
    let mut mgr = PortalHotkeys::connect_to(Some(&bus.address)).expect("connect");

    mgr.register_all(vec![(id("a"), chord("Ctrl+Alt+A")), (id("b"), chord("Print"))]).expect("bind both at once");
    assert_eq!(portal.log.lock().expect("log").creates, 1, "register_all binds in one session");

    mgr.register(id("c"), chord("Super+F5")).expect("add one");
    {
        let log = portal.log.lock().expect("log");
        assert_eq!(log.creates, 2, "portals may allow BindShortcuts only once per session");
        let last: Vec<&str> = log.binds.last().expect("bind").iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(last, ["a", "b", "c"]);
    }

    mgr.unregister(&id("b")).expect("remove one");
    {
        let log = portal.log.lock().expect("log");
        let last: Vec<&str> = log.binds.last().expect("bind").iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(last, ["a", "c"]);
    }
    assert_eq!(mgr.registered().len(), 2);
    // Events for the new session reach the manager.
    let session = portal.current_session();
    portal.signal("Activated", &session, "c");
    assert_eq!(mgr.events().recv_timeout(WAIT).expect("event").id, id("c"));

    // Removing everything closes the session without a bind call.
    let binds_before = portal.log.lock().expect("log").binds.len();
    mgr.unregister(&id("a")).expect("remove a");
    mgr.unregister(&id("c")).expect("remove c");
    assert!(mgr.registered().is_empty());
    assert_eq!(portal.log.lock().expect("log").binds.len(), binds_before + 1, "only the {{c}} rebind");
}

#[test]
fn misuse_and_refusal_leave_the_manager_unchanged() {
    let Some(bus) = Bus::start("refuse") else { return };
    let portal = Portal::start(&bus);
    let mut mgr = PortalHotkeys::connect_to(Some(&bus.address)).expect("connect");
    mgr.register(id("a"), chord("Ctrl+Alt+A")).expect("register");

    assert!(matches!(mgr.register(id("a"), chord("Ctrl+Alt+B")), Err(HotkeyError::DuplicateId(_))));
    assert!(matches!(mgr.register(id("z"), chord("Ctrl+Alt+A")), Err(HotkeyError::DuplicateChord { .. })));
    assert!(matches!(mgr.unregister(&id("nope")), Err(HotkeyError::UnknownId(_))));
    assert_eq!(portal.log.lock().expect("log").creates, 1, "rejected locally, no portal traffic");

    // The user dismisses the compositor's confirmation dialog.
    *portal.mode.lock().expect("mode") = Mode::Cancel;
    let err = mgr.register(id("b"), chord("Ctrl+Alt+B")).expect_err("cancelled");
    assert!(matches!(err, HotkeyError::Backend { .. }), "{err}");
    assert_eq!(mgr.registered(), vec![(id("a"), chord("Ctrl+Alt+A"))]);

    // And it recovers once the user agrees.
    *portal.mode.lock().expect("mode") = Mode::Accept;
    mgr.register(id("b"), chord("Ctrl+Alt+B")).expect("retry");
    assert_eq!(mgr.registered().len(), 2);
}

#[test]
fn a_bus_without_the_portal_is_reported_as_unavailable() {
    let Some(bus) = Bus::start("absent") else { return };
    let err = PortalHotkeys::connect_to(Some(&bus.address)).expect_err("no portal on this bus");
    assert!(matches!(err, HotkeyError::Unavailable { .. }), "{err}");
    assert!(err.to_string().contains("GlobalShortcuts") || err.to_string().contains("portal"), "{err}");
}

#[test]
fn an_unreachable_bus_is_reported_as_unavailable() {
    let err = PortalHotkeys::connect_to(Some("unix:path=/nonexistent/ssx-no-bus")).expect_err("no bus");
    assert!(matches!(err, HotkeyError::Unavailable { .. }), "{err}");
}

#[test]
fn dropping_the_manager_stops_its_thread_promptly() {
    let Some(bus) = Bus::start("drop") else { return };
    let _portal = Portal::start(&bus);
    let mgr = PortalHotkeys::connect_to(Some(&bus.address)).expect("connect");
    let started = std::time::Instant::now();
    drop(mgr);
    assert!(started.elapsed() < Duration::from_secs(2), "drop took {:?}", started.elapsed());
}
