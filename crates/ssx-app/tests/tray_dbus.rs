//! The tray on a private D-Bus session bus with mock desktop services.
//!
//! The real `ssx-app` runs against:
//!
//! * a mock `org.kde.StatusNotifierWatcher` (what a KDE panel, waybar or the GNOME AppIndicator
//!   extension provide) that records what registers;
//! * a mock `org.freedesktop.Notifications` server that records every notification.
//!
//! The test then plays the tray host: reads the StatusNotifierItem properties, calls
//! `com.canonical.dbusmenu.GetLayout` and compares it with the pure menu model the daemon
//! builds, and sends the dbusmenu `Event` a click produces. What a mock cannot say is how a
//! real host (Plasma, GNOME Shell) renders the menu; the README has that checklist.
//!
//! Skips when `dbus-daemon` (or, for the workflow click, `Xvfb`) is missing.
#![cfg(target_os = "linux")]

mod common;

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{Daemon, Fixture, expect_error, fixture, skip_unless, wait_until};
use ssx_app::menu::{Action, HotkeyView, Item, UiState, build_menu};
use ssx_core::{
    ipc::{ErrorCode, Request, Response},
    settings::Settings,
};
use zbus::{
    Connection,
    zvariant::{OwnedValue, Value},
};

// ---- a private session bus ------------------------------------------------------------

struct Bus {
    child: Child,
    address: String,
    dir: PathBuf,
}

impl Bus {
    fn start(tag: &str) -> Option<Bus> {
        let dir = std::env::temp_dir().join(format!("ssx-tray-{tag}-{}", std::process::id()));
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
        let mut child = match Command::new("dbus-daemon")
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

    fn connect(&self) -> Connection {
        async_io::block_on(
            zbus::connection::Builder::address(self.address.as_str()).unwrap().build(),
        )
        .expect("connect to the private bus")
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- mock services --------------------------------------------------------------------

#[derive(Default)]
struct WatcherLog {
    /// What `RegisterStatusNotifierItem` was called with, plus the caller's unique name.
    items: Vec<(String, String)>,
}

struct MockWatcher(Arc<Mutex<WatcherLog>>);

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl MockWatcher {
    fn register_status_notifier_item(
        &self,
        service: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().map(ToString::to_string).unwrap_or_default();
        self.0.lock().unwrap().items.push((service, sender));
    }

    fn register_status_notifier_host(&self, _service: String) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.0.lock().unwrap().items.iter().map(|(s, _)| s.clone()).collect()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

/// A running mock watcher; dropping it releases the bus name (the panel "went away").
struct Watcher {
    log: Arc<Mutex<WatcherLog>>,
    _conn: Connection,
}

impl Watcher {
    fn start(bus: &Bus) -> Watcher {
        let log: Arc<Mutex<WatcherLog>> = Arc::default();
        let conn = async_io::block_on(
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .name("org.kde.StatusNotifierWatcher")
                .unwrap()
                .serve_at("/StatusNotifierWatcher", MockWatcher(Arc::clone(&log)))
                .unwrap()
                .build(),
        )
        .expect("serve the watcher");
        Watcher { log, _conn: conn }
    }

    fn items(&self) -> Vec<(String, String)> {
        self.log.lock().unwrap().items.clone()
    }
}

#[derive(Debug, Clone)]
struct Toast {
    summary: String,
    body: String,
}

struct MockNotifications(Arc<Mutex<Vec<Toast>>>);

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl MockNotifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: String,
        _replaces_id: u32,
        _icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        _timeout: i32,
    ) -> u32 {
        let mut log = self.0.lock().unwrap();
        log.push(Toast { summary, body });
        u32::try_from(log.len()).unwrap_or(u32::MAX)
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "actions".into()]
    }

    fn close_notification(&self, _id: u32) {}

    fn get_server_information(&self) -> (String, String, String, String) {
        ("ssx-mock".into(), "ssx".into(), "0".into(), "1.2".into())
    }
}

struct Notifications {
    log: Arc<Mutex<Vec<Toast>>>,
    _conn: Connection,
}

impl Notifications {
    fn start(bus: &Bus) -> Notifications {
        let log: Arc<Mutex<Vec<Toast>>> = Arc::default();
        let conn = async_io::block_on(
            zbus::connection::Builder::address(bus.address.as_str())
                .unwrap()
                .name("org.freedesktop.Notifications")
                .unwrap()
                .serve_at("/org/freedesktop/Notifications", MockNotifications(Arc::clone(&log)))
                .unwrap()
                .build(),
        )
        .expect("serve notifications");
        Notifications { log, _conn: conn }
    }

    fn all(&self) -> Vec<Toast> {
        self.log.lock().unwrap().clone()
    }

    fn mentioning(&self, word: &str) -> Vec<Toast> {
        self.all()
            .into_iter()
            .filter(|t| format!("{} {}", t.summary, t.body).to_lowercase().contains(word))
            .collect()
    }
}

// ---- playing the tray host --------------------------------------------------------------

/// One dbusmenu node as the host sees it.
#[derive(Debug)]
struct Node {
    id: i32,
    props: HashMap<String, Value<'static>>,
    children: Vec<Node>,
}

impl Node {
    fn str(&self, k: &str) -> Option<String> {
        match self.props.get(k) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.props.get(k) {
            Some(Value::Bool(b)) => *b,
            _ => default,
        }
    }

    fn is_separator(&self) -> bool {
        self.str("type").as_deref() == Some("separator")
    }
}

fn unwrap_variant<'a>(v: &'a Value<'a>) -> &'a Value<'a> {
    match v {
        Value::Value(inner) => unwrap_variant(inner),
        other => other,
    }
}

fn parse_node(v: &Value<'_>) -> Node {
    let Value::Structure(s) = unwrap_variant(v) else { panic!("a dbusmenu node is a struct: {v:?}") };
    let fields = s.fields();
    let Value::I32(id) = unwrap_variant(&fields[0]) else { panic!("node id: {:?}", fields[0]) };
    let Value::Dict(d) = unwrap_variant(&fields[1]) else { panic!("props: {:?}", fields[1]) };
    let mut props = HashMap::new();
    for (k, val) in d.iter() {
        if let Value::Str(k) = unwrap_variant(k) {
            props.insert(k.to_string(), unwrap_variant(val).try_to_owned().unwrap().into());
        }
    }
    let Value::Array(a) = unwrap_variant(&fields[2]) else { panic!("children: {:?}", fields[2]) };
    Node { id: *id, props, children: a.iter().map(parse_node).collect() }
}

struct Host {
    conn: Connection,
    dest: String,
    item_path: String,
}

impl Host {
    /// Attaches to what registered with the watcher (`service` may be a bus name, or
    /// `name/path`).
    fn attach(bus: &Bus, registered: &(String, String)) -> Host {
        let (service, sender) = registered;
        let (dest, item_path) = match service.split_once('/') {
            Some((d, p)) if !d.is_empty() => (d.to_owned(), format!("/{p}")),
            Some((_, p)) => (sender.clone(), format!("/{p}")),
            None => (service.clone(), "/StatusNotifierItem".to_owned()),
        };
        Host { conn: bus.connect(), dest, item_path }
    }

    fn proxy(&self, path: &str, iface: &str) -> zbus::Proxy<'_> {
        async_io::block_on(zbus::Proxy::new(
            &self.conn,
            self.dest.clone(),
            path.to_owned(),
            iface.to_owned(),
        ))
        .expect("proxy")
    }

    fn item_prop<T>(&self, name: &str) -> T
    where
        T: TryFrom<OwnedValue>,
        T::Error: Into<zbus::Error>,
    {
        let p = self.proxy(&self.item_path, "org.kde.StatusNotifierItem");
        async_io::block_on(p.get_property::<T>(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    fn menu_path(&self) -> String {
        let p: zbus::zvariant::OwnedObjectPath = self.item_prop("Menu");
        p.as_str().to_owned()
    }

    fn layout(&self) -> Node {
        let p = self.proxy(&self.menu_path(), "com.canonical.dbusmenu");
        let reply = async_io::block_on(p.call_method(
            "GetLayout",
            &(0i32, -1i32, Vec::<String>::new()),
        ))
        .expect("GetLayout");
        let body = reply.body();
        let whole: zbus::zvariant::Structure<'_> = body.deserialize().expect("layout body");
        let root = whole.fields()[1].clone();
        parse_node(&root)
    }

    fn click(&self, id: i32) {
        let p = self.proxy(&self.menu_path(), "com.canonical.dbusmenu");
        async_io::block_on(p.call_method("Event", &(id, "clicked", Value::from(0i32), 0u32)))
            .expect("Event");
    }
}

// ---- the tests --------------------------------------------------------------------------

fn start_with_bus(f: &mut Fixture, bus: &Bus, desktop: &str) -> Daemon {
    f.env.set("DBUS_SESSION_BUS_ADDRESS", bus.address.clone());
    f.env.set("XDG_CURRENT_DESKTOP", desktop);
    Daemon::start(&f.env, &["--no-hotkeys"])
}

/// What the daemon's menu should be for this fixture: the model built from the same settings
/// with what the test environment disables (no overlay, no settings window, no hotkeys).
fn expected_menu(f: &Fixture) -> ssx_app::menu::Menu {
    let loaded = Settings::load(&f.env.cfg.join("settings.toml")).expect("settings load");
    let ui = UiState {
        overlay_available: false,
        settings_ui_available: false,
        hotkeys: HotkeyView { enabled: false, backend: None, problems: 0 },
        ..UiState::default()
    };
    build_menu(&loaded.settings, &ui)
}

#[test]
fn the_tray_registers_serves_the_model_as_dbusmenu_and_a_click_runs_a_workflow() {
    if skip_unless(&["dbus-daemon", "Xvfb"]) {
        return;
    }
    let Some(bus) = Bus::start("menu") else { return };
    let Some(mut f) = fixture() else { return };
    let watcher = Watcher::start(&bus);
    let notes = Notifications::start(&bus);
    let d = start_with_bus(&mut f, &bus, "KDE");

    // 1. The item registers with the watcher, and the daemon reports a tray.
    let registered = wait_until(Duration::from_secs(15), || watcher.items().first().cloned())
        .unwrap_or_else(|| panic!("no StatusNotifierItem registered\n{}", f.env.daemon_log()));
    let Response::Status(st) = f.env.request(&Request::Status) else { panic!("status") };
    assert!(st.tray, "{st:?}");
    let host = Host::attach(&bus, &registered);

    // 2. The item properties.
    assert_eq!(host.item_prop::<String>("Id"), "ssx");
    assert_eq!(host.item_prop::<String>("Category"), "ApplicationStatus");
    assert_eq!(host.item_prop::<String>("Status"), "Active");
    let pixmaps: Vec<(i32, i32, Vec<u8>)> = host.item_prop("IconPixmap");
    assert!(!pixmaps.is_empty());
    for (w, h, data) in &pixmaps {
        assert_eq!(data.len(), (*w * *h * 4) as usize, "ARGB32 {w}x{h}");
        assert!(data.chunks(4).any(|px| px[0] != 0), "some opaque pixel in {w}x{h}");
    }
    let tip: (String, Vec<(i32, i32, Vec<u8>)>, String, String) = host.item_prop("ToolTip");
    assert_eq!(tip.2, "ssx: Ready");

    // 3. GetLayout mirrors the model row for row.
    let layout = host.layout();
    let model = expected_menu(&f);
    let got: Vec<String> = layout
        .children
        .iter()
        .map(|n| {
            if n.is_separator() {
                "---".to_owned()
            } else {
                format!(
                    "{}|{}|{:?}",
                    n.str("label").unwrap_or_default(),
                    n.bool_or("enabled", true),
                    n.props.get("toggle-state")
                )
            }
        })
        .collect();
    let want: Vec<String> = model
        .items
        .iter()
        .map(|i| match i {
            Item::Separator => "---".to_owned(),
            Item::Header(t) => format!("{t}|false|None"),
            Item::Entry(e) => format!(
                "{}|{}|{}",
                e.label,
                e.enabled,
                match e.checked {
                    Some(c) => format!("Some(I32({}))", i32::from(c)),
                    None => "None".to_owned(),
                }
            ),
        })
        .collect();
    assert_eq!(got, want, "the dbusmenu layout is the menu model");

    // 4. A click (the dbusmenu Event) runs the workflow and the file appears.
    let entry = model
        .entries()
        .find(|e| matches!(&e.action, Action::RunWorkflow(id) if id == "shot-local"))
        .expect("the workflow is in the menu");
    let row = model
        .items
        .iter()
        .position(|i| matches!(i, Item::Entry(e) if e.action == entry.action))
        .unwrap();
    let node = &layout.children[row];
    assert_eq!(node.str("label").as_deref(), Some(entry.label.as_str()));
    let before = std::fs::read_dir(&f.save).map_or(0, Iterator::count);
    host.click(node.id);
    let saved = wait_until(Duration::from_secs(15), || {
        (std::fs::read_dir(&f.save).map_or(0, Iterator::count) > before).then_some(())
    });
    assert!(saved.is_some(), "the click ran the workflow\n{}", f.env.daemon_log());

    // 5. Nothing was announced: the tray was fine and notifications are off.
    assert!(notes.mentioning("tray").is_empty(), "{:?}", notes.all());

    // 6. Quit from the menu ends the daemon and the item goes away.
    let quit = model
        .items
        .iter()
        .position(|i| matches!(i, Item::Entry(e) if e.action == Action::Quit))
        .expect("a Quit row");
    let mut d = d;
    host.click(layout.children[quit].id);
    assert!(d.wait_exit(Duration::from_secs(20)).is_some_and(|s| s.success()), "Quit exits 0");
}

#[test]
fn without_a_watcher_the_daemon_degrades_says_so_once_and_the_icon_appears_later() {
    if skip_unless(&["dbus-daemon", "Xvfb"]) {
        return;
    }
    let Some(bus) = Bus::start("nowatcher") else { return };
    let Some(mut f) = fixture() else { return };
    // The notice goes through the settings-independent path; the fixture turns workflow
    // notifications off, which must not silence it.
    let notes = Notifications::start(&bus);
    let mut d = start_with_bus(&mut f, &bus, "GNOME");

    // 1. One notification that names the tray and the fix for GNOME.
    let toast = wait_until(Duration::from_secs(15), || notes.mentioning("tray").first().cloned())
        .unwrap_or_else(|| {
            panic!("no notification about the missing tray\n{}", f.env.daemon_log())
        });
    let text = format!("{} {}", toast.summary, toast.body).to_lowercase();
    assert!(text.contains("appindicator"), "GNOME advice names the extension: {text}");

    // 2. The daemon is fully usable meanwhile: IPC and a workflow.
    let Response::Status(st) = f.env.request(&Request::Status) else { panic!("status") };
    assert!(!st.tray, "no tray yet: {st:?}");
    let s = common::finished(f.env.request(&Request::RunWorkflow {
        id: None,
        name: Some("local".into()),
        wait: true,
    }));
    assert_eq!(s.outcome, ssx_core::workflow::Outcome::Success, "{}", s.message);

    // 3. The panel starts later: the icon shows up without restarting ssx.
    let watcher = Watcher::start(&bus);
    let registered = wait_until(Duration::from_secs(20), || watcher.items().first().cloned())
        .unwrap_or_else(|| {
            panic!("the tray did not appear when the watcher did\n{}", f.env.daemon_log())
        });
    let host = Host::attach(&bus, &registered);
    assert_eq!(host.item_prop::<String>("Id"), "ssx");
    assert!(!host.layout().children.is_empty());

    // 4. A restart without a watcher does not repeat the notice (it is once per user).
    f.env.request(&Request::Quit);
    assert!(d.wait_exit(Duration::from_secs(20)).is_some());
    drop(watcher);
    let count = notes.mentioning("tray").len();
    assert_eq!(count, 1, "{:?}", notes.all());
    let mut d2 = start_with_bus(&mut f, &bus, "GNOME");
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(notes.mentioning("tray").len(), 1, "shown once: {:?}", notes.all());
    f.env.request(&Request::Quit);
    assert!(d2.wait_exit(Duration::from_secs(20)).is_some());
}

#[test]
fn no_session_bus_at_all_is_not_an_error() {
    let Some(mut f) = fixture() else { return };
    f.env.set("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/ssx-no-bus");
    let mut d = Daemon::start(&f.env, &["--no-hotkeys"]);
    let Response::Status(st) = f.env.request(&Request::Status) else { panic!("status") };
    assert!(!st.tray);
    // Still serving: a bogus request is answered with a proper error, not a hang.
    let r = f.env.request(&Request::RunWorkflow {
        id: None,
        name: Some("no-such-workflow".into()),
        wait: true,
    });
    expect_error(&r, ErrorCode::UnknownWorkflow);
    assert_eq!(f.env.request(&Request::Quit), Response::Ok);
    assert!(d.wait_exit(Duration::from_secs(20)).is_some_and(|s| s.success()));
    let log = f.env.daemon_log();
    assert!(log.contains("no tray icon") || log.contains("D-Bus"), "{log}");
}

#[test]
fn hotkeys_without_a_portal_are_announced_once_with_the_cli_commands() {
    if skip_unless(&["dbus-daemon"]) {
        return;
    }
    let Some(bus) = Bus::start("hotkeys") else { return };
    let notes = Notifications::start(&bus);
    // A Wayland session whose portal has no GlobalShortcuts (there is no portal at all).
    let mut env = common::TestEnv::new();
    env.set("DBUS_SESSION_BUS_ADDRESS", bus.address.clone());
    env.set("XDG_SESSION_TYPE", "wayland");
    env.set("WAYLAND_DISPLAY", "wayland-ssx-none");
    env.set("XDG_CURRENT_DESKTOP", "sway");
    let mut d = Daemon::start(&env, &["--no-tray"]);

    let toast = wait_until(Duration::from_secs(15), || notes.mentioning("hotkeys").first().cloned())
        .unwrap_or_else(|| panic!("no hotkey notice\n{}", env.daemon_log()));
    assert!(toast.body.contains("ssx hotkeys install"), "{}", toast.body);
    assert!(toast.body.contains("ssx run"), "the CLI commands are listed: {}", toast.body);
    let Response::Status(st) = env.request(&Request::Status) else { panic!("status") };
    assert_eq!(st.hotkey_backend, "none");
    assert_eq!(st.hotkeys_registered, 0);

    // Once: a second start does not repeat it.
    assert_eq!(env.request(&Request::Quit), Response::Ok);
    assert!(d.wait_exit(Duration::from_secs(20)).is_some());
    let mut d2 = Daemon::start(&env, &["--no-tray"]);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(notes.mentioning("hotkeys").len(), 1, "{:?}", notes.all());
    assert_eq!(env.request(&Request::Quit), Response::Ok);
    assert!(d2.wait_exit(Duration::from_secs(20)).is_some());
}

#[test]
fn an_idle_daemon_with_a_tray_costs_next_to_nothing() {
    if skip_unless(&["dbus-daemon", "Xvfb"]) {
        return;
    }
    let Some(bus) = Bus::start("idle") else { return };
    let Some(mut f) = fixture() else { return };
    let watcher = Watcher::start(&bus);
    let d = start_with_bus(&mut f, &bus, "KDE");
    assert!(wait_until(Duration::from_secs(15), || watcher.items().first().cloned()).is_some());
    std::thread::sleep(Duration::from_secs(2));
    let secs: u64 = std::env::var("SSX_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(8);
    let (t0, _) = d.proc_usage().expect("proc");
    std::thread::sleep(Duration::from_secs(secs));
    let (t1, rss) = d.proc_usage().expect("proc");
    let cpu = (t1 - t0) as f64 / 100.0 / secs as f64 * 100.0;
    eprintln!("idle with tray over {secs} s: CPU {cpu:.3} % of one core, RSS {rss} KiB");
    assert!(cpu < 1.0, "idle CPU {cpu} %");
}
