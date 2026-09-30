# ssx-app

The ssx background daemon: tray icon, global hotkeys, the IPC server the `ssx` CLI and file-manager
entries talk to, the workflow supervisor and the recording control. One instance per user.

```text
ssx daemon start | stop | restart | status [--json]
ssx daemon autostart enable | disable | status      # start at login
ssx-app [--no-tray] [--no-hotkeys] [--foreground] [-v] [--config-dir DIR] [--backend NAME] [--version]
```

You normally never run `ssx-app` yourself: `ssx daemon start` finds it (`SSX_APP`, next to `ssx`,
then `PATH`), starts it detached and waits until it answers; autostart launches it at login.
Starting it a second time asks the running one to show its settings and exits 0.

## Architecture

```text
                 hotkey            tray click          ssx CLI / file manager
                    |                  |                        |
        +-----------v------+   +-------v-------+     +----------v----------+
        | hotkeys_glue     |   | tray_ksni     |     | ipc_server          |
        | (ssx-hotkeys:    |   | tray_native   |     | (ssx-ipc socket,    |
        |  X11 grab /      |   | (menu model,  |     |  one thread per     |
        |  portal / none)  |   |  icons)       |     |  connection)        |
        +---------+--------+   +-------+-------+     +----------+----------+
                  |  Action            |  Action                |  Request
                  +--------------------+-----------+------------+
                                                   v
                                          +--------+---------+     PostFiles
                                          | app::App         |<----- coalesce (400 ms window,
                                          | (requests.rs:    |       2 s cap, deduped, ordered)
                                          |  action -> Job)  |
                                          +--------+---------+
                                                   | Job
                                          +--------v---------+  events  +--------------------+
                                          | daemon::Supervisor|-------->| ui worker          |
                                          | admission, queue, |         |  tray tooltip/icon |
                                          | cancel, shutdown  |         |  notifications     |
                                          +----+---------+----+         +--------------------+
                                               |         |
                          regular runs (<= 4)  |         | one interactive run / one recording
                                               v         v
                                     runtime::EngineRunner (ssx-core Engine)
                                               |
                       ssx-services: capture, OverlaySelector (ssx-overlay), uploaders,
                       history, clipboard, ServiceRecorder (ssx-record)

   reload: SettingsWatcher (notify, 250 ms debounce, mtime polling fallback)
           -> ReloadState::evaluate -> valid: swap settings, re-register hotkeys, rebuild menu
                                       invalid: keep the old settings, one notification
```

Every arrow that crosses a thread boundary is a channel; nothing blocks the tray or the hotkey
thread. With nothing to do every thread is parked (no polling loops except the 250 ms command
latency of the hotkey thread and the settings watcher's fallback).

### Modules

| Module | Role |
|---|---|
| `daemon` | supervisor: pure admission decision (`decide`), queue, cancellation, panic containment, shutdown |
| `coalesce` | `PostFiles` merging, pure state machine plus a timer thread; fake-clock tested |
| `recording` | Idle -> Selecting -> Recording -> Stopping -> Idle; one recording at a time |
| `menu`, `icons` | tray menu / tooltip / icon as pure data; icons are drawn in code |
| `tray`, `tray_ksni`, `tray_native` | per-platform tray backends |
| `hotkeys_glue` | settings -> registrations; conflicts and failures are reported, never dropped |
| `reload` | settings hot-reload decision |
| `requests`, `ipc_server` | IPC requests and menu actions to jobs; the socket handler |
| `runtime` | real services, the engine-driving runner, status snapshot |
| `notify` | notifications with a click target; one-time notices (`notices.json`) |
| `ui` | event reducer: tray view and notifications |
| `app` | assembly, lifecycle, signals |

## Lifecycle

1. **Start.** Arguments, logging (rotating file `<data>/logs/ssx-app.log.YYYY-MM-DD`, seven days;
   terminal only with `--foreground` / `-v`), panic hook. `Instance::acquire`: if another
   instance owns the socket, forward "show settings", print "already running" and exit 0.
2. **Assemble.** Load settings (a broken file is reported and defaults are used; nothing is
   overwritten), discover helpers (`ssx-overlay`, `ssx-editor-ui`, `ssx-settings-ui`; `none`
   in their environment variables disables one), build services and the supervisor, start the
   IPC server, register hotkeys, start the tray.
3. **Serve.** Requests, hotkeys and tray clicks become jobs. One interactive capture (overlay
   or editor) at a time: a second interactive request is refused with `Busy`, not queued. Up to
   four non-interactive runs in parallel, the rest queue. One recording at a time; the same
   hotkey or menu entry toggles (stops) it.
4. **Reload.** Editing `settings.toml` is picked up within about a second. A valid file replaces
   the running settings (hotkeys, menu, workflows); an invalid one keeps the old settings and
   says why once, until the file changes again.
5. **Shutdown.** Quit (tray, IPC, Ctrl-C, SIGTERM/SIGHUP, Windows console close). A running
   recording is stopped gracefully first (15 s to finalise the file), running runs get 5 s, then
   are cancelled (5 s to unwind); hotkeys are unregistered, the tray removed, the socket file
   deleted. In-flight recordings are fragmented MP4 in the daemon, so even a hard kill leaves a
   playable file.

## IPC summary

Line-delimited JSON over the `ssx-ipc` socket (`$XDG_RUNTIME_DIR/ssx/ssx.sock`, a named pipe on
Windows). Types are in `ssx_core::ipc`; every request carries `v` and `seq`.

| Request | Answer |
|---|---|
| `Ping` | `Pong{version}` |
| `Status` | `Status`: pid, uptime, tray, hotkey backend/problems, active and queued runs, recording, settings problem |
| `RunWorkflow{id or name, wait}` | `Accepted{run_id}` or `Finished(summary)` |
| `Capture{kind, mode, ...}` | same as `RunWorkflow` for an ad-hoc capture |
| `PostFiles{paths, action}` | coalesced; every caller of one batch gets the same `Finished` |
| `WaitRun{run_id}` / `CancelRun{run_id}` | `Finished` / `Ok` |
| `StartRecording` / `ToggleRecording` / `StopRecording` / `RecordingStatus` | `Recording(status)` / `Ok` |
| `Show{target}` | opens settings, history or the editor helper |
| `ReloadSettings` | re-reads `settings.toml` now |
| `ListWorkflows`, `Quit` | list / `Ok` |

Errors carry a code: `InvalidRequest`, `VersionMismatch`, `UnknownWorkflow`, `Busy`,
`NotRunning`, `Internal`. `SSX_NO_DAEMON=1` makes the CLI ignore a running daemon.

## Per-desktop notes

| Desktop | Tray | Hotkeys | Notes |
|---|---|---|---|
| KDE Plasma (Wayland/X11) | StatusNotifierItem, works out of the box | X11: key grab. Wayland: GlobalShortcuts portal (Plasma 5.27+/6) asks once | menu via dbusmenu |
| GNOME (Wayland/X11) | needs the *AppIndicator and KStatusNotifierItem Support* extension; without it the icon appears as soon as it is enabled, and ssx says so once | X11: key grab. Wayland: GlobalShortcuts portal (GNOME 48+); otherwise `ssx hotkeys install --apply` binds custom shortcuts to `ssx` commands | mutter has no layer-shell, so the overlay falls back to its X11 backend under XWayland or the capture portal's own picker; check this on a real GNOME session (see the checklist) |
| sway, Hyprland, other wlroots | needs a bar with a tray (waybar `tray`, etc.); otherwise degrades with a notice | no portal: the daemon lists the `ssx run ...` commands once; `ssx hotkeys install` writes the sway/Hyprland include | layer-shell overlay, wlr-screencopy capture |
| Windows | `tray-icon` on a native message loop | `RegisterHotKey` | autostart via `HKCU\...\Run`. Not verified on real Windows, see below |
| macOS | `tray-icon` menu bar item | global hotkeys | screen-recording permission is requested by the OS; not verified on real macOS |

No D-Bus session, no tray, no display: the daemon still runs and serves the CLI; it logs why.

## Verification status

Automated (`cargo test -p ssx-app`; tests skip with a printed reason when a tool is missing):

* **live**: Xvfb (run, region with the real overlay driven by xdotool and checked pixel-exact,
  window and ellipse modes, hotkey press, 3x `post-file --coalesce` as one batch, reload valid and
  invalid, recording to a decodable mp4, SIGTERM finalising a recording, idle CPU/RSS), headless
  sway (wlroots capture, layer-shell overlay, cancel, exclusivity), the real CLI against the real
  daemon (`daemon start/stop/restart/autostart`, `record start/stop`, `run` hand-off).
* **mock**: a private D-Bus session with a mock StatusNotifierWatcher and notification server
  (registration, `GetLayout` equals the menu model, an `Event` runs a workflow, missing watcher
  degrades with one notice and the icon appears when the watcher does, hotkey notice without a
  portal).
* **compile only**: `tray_native` and everything Windows/macOS-specific.

### Manual checklist

Run through this on each desktop before a release. Start with `ssx doctor` and `ssx daemon start`.

- [ ] Tray icon appears within a second; tooltip says "ssx: Ready"; the menu matches the
      workflows in `settings.toml`.
- [ ] Hotkey for a fullscreen workflow captures, saves and notifies; clicking the notification
      opens the file or link.
- [ ] Region hotkey: the overlay appears on every monitor, Esc cancels, Enter confirms, the
      crop matches.
- [ ] Record hotkey starts, the icon turns red and the tooltip counts up; pressing it again
      stops, the file plays.
- [ ] Edit `settings.toml` (change a hotkey): the new one works without a restart. Break the
      file: one notification, the old hotkeys still work.
- [ ] Select several files in the file manager, "Upload with ssx": one batch, one link list.
- [ ] `ssx daemon stop`, then `kill -TERM` during a recording: the file is playable.
- [ ] Log out and in with `ssx daemon autostart enable`: the daemon is up.
- **GNOME**: without the extension the notice appears once; after enabling it the icon shows
  up without restarting; on Wayland the portal shortcut dialog appears once.
- **KDE**: the portal shortcuts appear in System Settings; the tray menu shows shortcuts.
- **Hyprland / sway**: `ssx hotkeys install` output works; overlay covers all outputs.
- **Windows**: tray icon and menu; `RegisterHotKey` conflicts are reported; Explorer
  multi-select "Upload with ssx" batches; the daemon exits at logoff (best effort: the console
  handler ends it, `WM_ENDSESSION` is not handled specially).
- **macOS**: menu bar icon; the screen-recording permission flow; hotkeys.

## Known limits

* `panic = "abort"` in the release profile means the panic hook and per-run panic containment
  only help in other profiles; the workspace manifest decides.
* The overlay helper has no external cancel API; the daemon kills the helper process to cancel.
* The Windows and macOS tray runs the event loop on the main thread and wakes ten times a second
  to poll hotkeys (about 0.1 % CPU); the Linux daemon does not poll.
