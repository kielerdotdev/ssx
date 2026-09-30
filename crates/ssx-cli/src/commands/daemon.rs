//! `ssx daemon start|stop|status|restart|autostart`: managing the ssx background app.
//!
//! `start` launches `ssx-app` *detached* (its own process group on Unix, `DETACHED_PROCESS` on
//! Windows, standard streams closed) and returns only once it answers a ping, so the next
//! command in a script can rely on it. The program is found through `SSX_APP`, then next to
//! this executable, then `PATH`. `stop` asks it to quit over IPC and waits for the socket to
//! go away: the app finishes a recording and gives running uploads a moment first, so this can
//! take a few seconds.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use ssx_core::ipc::{DaemonStatus, Request, Response};
use ssx_services::helpers::{Discovery, discover};

use crate::{
    app::App,
    autostart::{self, State},
    cli::{AutostartCmd, DaemonCmd},
    error::{CliError, CliResult},
    forward::Daemon,
    output::{err_line, out_line},
};

/// How long `start` waits for the app to answer.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// How long `stop` waits for the app to exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(40);

/// Environment variable naming the `ssx-app` executable.
pub const APP_ENV: &str = "SSX_APP";

/// Where `ssx-app` is.
pub fn find_app() -> Discovery {
    discover("ssx-app", APP_ENV)
}

fn app_or_error() -> CliResult<PathBuf> {
    match find_app() {
        Discovery::Found(p) => Ok(p),
        Discovery::Disabled => {
            Err(CliError::new("the ssx background app is switched off (SSX_APP=none)"))
        }
        Discovery::NotFound => Err(CliError::new("cannot find the ssx-app program")
            .hint("install it next to `ssx` or on PATH, or set SSX_APP to its location")),
    }
}

/// Spawns `ssx-app` detached with the flags that matter for a daemon of this command line.
pub fn spawn_detached(exe: &Path, app: &App) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new(exe);
    if let Some(dir) = &app.global.config_dir {
        cmd.arg("--config-dir").arg(dir);
    }
    if let Some(backend) = &app.global.backend {
        cmd.arg("--backend").arg(backend);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group: a Ctrl-C or a closing terminal aimed at us must not kill it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
        cmd.creation_flags(0x0000_0008 | 0x0000_0200 | 0x0800_0000);
    }
    cmd.spawn()
}

/// The answer to a ping, if the app is up.
fn ping() -> Option<(Daemon, String)> {
    let d = Daemon::connect_always()?;
    let v = d.ping()?;
    Some((d, v))
}

fn start(app: &App) -> CliResult<()> {
    if let Some((_, version)) = ping() {
        err_line(&format!("ssx-app {version} is already running"));
        return Ok(());
    }
    let exe = app_or_error()?;
    let mut child = spawn_detached(&exe, app)
        .map_err(|e| CliError::new(format!("cannot start {}: {e}", exe.display())))?;
    let started = Instant::now();
    loop {
        if let Some((_, version)) = ping() {
            err_line(&format!("ssx-app {version} started (pid {})", child.id()));
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            // A second instance exiting 0 means another one won the race and is now up.
            if status.success() && ping().is_some() {
                return Ok(());
            }
            return Err(CliError::new(format!("ssx-app exited right away ({status})"))
                .hint(format!("its log is in {}", app.paths.data_dir.join("logs").display())));
        }
        if started.elapsed() >= START_TIMEOUT {
            return Err(CliError::new(format!(
                "ssx-app did not answer within {} seconds",
                START_TIMEOUT.as_secs()
            ))
            .hint(format!("its log is in {}", app.paths.data_dir.join("logs").display())));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop() -> CliResult<()> {
    let Some((daemon, _)) = ping() else {
        err_line("ssx-app is not running");
        return Ok(());
    };
    match daemon.call(Request::Quit).map_err(CliError::new)? {
        Response::Ok => {}
        Response::Error { code, message } => {
            return Err(crate::forward::error_from_response(code, &message));
        }
        other => return Err(CliError::new(format!("unexpected answer: {other:?}"))),
    }
    let started = Instant::now();
    while Daemon::connect_always().is_some() {
        if started.elapsed() >= STOP_TIMEOUT {
            return Err(CliError::new(format!(
                "ssx-app is still running {} seconds after being asked to quit",
                STOP_TIMEOUT.as_secs()
            ))
            .hint("it may be finishing a recording or an upload; try again, or end the process"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    err_line("ssx-app stopped");
    Ok(())
}

/// `3725` -> `1h 2m`, `75` -> `1m 15s`, `9` -> `9s`.
pub fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs / 60) % 60),
    }
}

/// The human text of `daemon status`.
pub fn render_status(s: &DaemonStatus) -> String {
    let mut out = format!(
        "ssx-app {} is running (pid {}, up {})\n",
        s.app_version,
        s.pid,
        human_duration(s.uptime_secs)
    );
    out.push_str(&format!("config:   {}\n", s.config_dir));
    out.push_str(&format!("tray:     {}\n", if s.tray { "showing" } else { "not showing" }));
    out.push_str(&format!(
        "hotkeys:  {} registered via {}\n",
        s.hotkeys_registered, s.hotkey_backend
    ));
    for p in &s.hotkey_problems {
        out.push_str(&format!("          problem: {p}\n"));
    }
    if s.recording.active {
        out.push_str(&format!(
            "recording: {} ({})\n",
            s.recording.workflow.as_deref().unwrap_or("?"),
            if s.recording.selecting {
                "choosing a region".to_owned()
            } else {
                format!("{} s", s.recording.elapsed_ms / 1000)
            }
        ));
    }
    for r in &s.active_runs {
        out.push_str(&format!("running:  #{} {} ({} s)\n", r.run_id, r.name, r.running_secs));
    }
    if s.queued_runs > 0 {
        out.push_str(&format!("queued:   {}\n", s.queued_runs));
    }
    if let Some(p) = &s.settings_problem {
        out.push_str(&format!("settings: NOT reloaded: {p}\n"));
    }
    out
}

fn status(json: bool) -> CliResult<()> {
    let Some((daemon, _)) = ping() else {
        if json {
            out_line("{\"running\":false}");
        }
        return Err(
            CliError::new("ssx-app is not running").hint("start it with `ssx daemon start`")
        );
    };
    match daemon.call(Request::Status).map_err(CliError::new)? {
        Response::Status(s) => {
            if json {
                out_line(&serde_json::to_string_pretty(&s)?);
            } else {
                out_line(render_status(&s).trim_end());
            }
            Ok(())
        }
        Response::Error { code, message } => {
            Err(crate::forward::error_from_response(code, &message))
        }
        other => Err(CliError::new(format!("unexpected answer: {other:?}"))),
    }
}

fn autostart_cmd(cmd: &AutostartCmd) -> CliResult<()> {
    let ctx = autostart::Context::system().map_err(|e| CliError::new(e.to_string()))?;
    let fail = |e: autostart::AutostartError| CliError::new(e.to_string());
    match cmd {
        AutostartCmd::Enable => {
            let exe = app_or_error()?;
            let exe = std::fs::canonicalize(&exe).map(crate::app::strip_verbatim).unwrap_or(exe);
            let state = autostart::enable(&ctx, &exe).map_err(fail)?;
            if let State::Enabled { command, location } = state {
                out_line(&format!("ssx will start at login: {command} ({location})"));
            }
            Ok(())
        }
        AutostartCmd::Disable => {
            if autostart::disable(&ctx).map_err(fail)? {
                out_line("ssx will no longer start at login");
            } else {
                out_line("ssx was not set to start at login");
            }
            Ok(())
        }
        AutostartCmd::Status => {
            match autostart::status(&ctx).map_err(fail)? {
                State::Enabled { command, location } => {
                    out_line(&format!("enabled: {command} ({location})"));
                }
                State::Disabled => out_line("disabled"),
                State::Foreign { path } => out_line(&format!(
                    "disabled (a file of your own is in the way: {})",
                    path.display()
                )),
            }
            Ok(())
        }
    }
}

/// Dispatches `ssx daemon ...`.
pub fn run(app: &App, cmd: DaemonCmd) -> CliResult<()> {
    match cmd {
        DaemonCmd::Start => start(app),
        DaemonCmd::Stop => stop(),
        DaemonCmd::Status { json } => status(json),
        DaemonCmd::Restart => {
            stop()?;
            start(app)
        }
        DaemonCmd::Autostart { cmd } => autostart_cmd(&cmd),
    }
}

#[cfg(test)]
mod tests {
    use ssx_core::ipc::{ActiveRunInfo, RecordingStatus};

    use super::*;

    fn status_fixture() -> DaemonStatus {
        DaemonStatus {
            app_version: "0.1.0".into(),
            pid: 4242,
            uptime_secs: 3725,
            tray: true,
            hotkey_backend: "global-hotkey".into(),
            hotkeys_registered: 6,
            hotkey_problems: vec!["Ctrl+Print: already used".into()],
            active_runs: vec![ActiveRunInfo { run_id: 7, name: "Capture".into(), running_secs: 3 }],
            queued_runs: 2,
            recording: RecordingStatus {
                active: true,
                selecting: false,
                workflow: Some("record-screen".into()),
                elapsed_ms: 12_500,
                run_id: Some(7),
            },
            config_dir: "/home/u/.config/ssx".into(),
            settings_problem: Some("bad hotkey".into()),
        }
    }

    #[test]
    fn the_status_text_shows_what_matters() {
        let t = render_status(&status_fixture());
        for want in [
            "ssx-app 0.1.0 is running (pid 4242, up 1h 2m)",
            "tray:     showing",
            "6 registered via global-hotkey",
            "problem: Ctrl+Print: already used",
            "recording: record-screen (12 s)",
            "running:  #7 Capture (3 s)",
            "queued:   2",
            "settings: NOT reloaded: bad hotkey",
        ] {
            assert!(t.contains(want), "missing {want:?} in\n{t}");
        }
        let mut quiet = status_fixture();
        quiet.recording = RecordingStatus::default();
        quiet.active_runs.clear();
        quiet.queued_runs = 0;
        quiet.settings_problem = None;
        quiet.hotkey_problems.clear();
        quiet.tray = false;
        let t = render_status(&quiet);
        assert!(t.contains("not showing") && !t.contains("recording") && !t.contains("queued"));
    }

    #[test]
    fn durations_are_short_and_human() {
        assert_eq!(human_duration(0), "0s");
        assert_eq!(human_duration(59), "59s");
        assert_eq!(human_duration(75), "1m 15s");
        assert_eq!(human_duration(3725), "1h 2m");
        assert_eq!(human_duration(86_400 + 60), "24h 1m");
    }

    #[test]
    fn a_selecting_recording_says_so() {
        let mut s = status_fixture();
        s.recording.selecting = true;
        assert!(render_status(&s).contains("choosing a region"));
    }

    #[test]
    fn a_missing_app_is_a_clear_error() {
        let d = ssx_services::helpers::discover_with("ssx-app", None, None, |_| None);
        assert_eq!(d, Discovery::NotFound);
    }
}
