# ssx-settings-ui

The settings and history window of ssx: one native window (egui/eframe with wgpu) with a nav
rail and eight pages. It edits a working copy of `ssx_core::settings::Settings`; **Apply**,
**Revert** and **Save** are in the footer, saving is atomic (the core writes a temp file and
renames it) and blocked while anything fails validation. No restart is needed: the daemon
reloads `settings.toml`.

```
ssx-settings-ui [--page general|capture|workflows|hotkeys|uploaders|history|integration|about]
                [--config-dir DIR] [--json]
```

`--json` prints one line when the window closes: `{"saved":…,"writes":…,"discarded":…,"page":…,"settings_file":…}`.
Exit code `0` normal, `2` could not start. Keyboard: `Ctrl+S` apply, `Ctrl+1`…`Ctrl+8` pages,
`Ctrl+PageUp/PageDown` previous/next page.

| Page | What it does |
| --- | --- |
| General | save folder and per-type sub-folders, filename/folder patterns with a live preview through `ssx_core::pattern`, token cheat sheet (incl. `UNSUPPORTED_TOKENS`), illegal-character warnings, image format/quality, autostart (XDG `.desktop`, Windows `HKCU\…\Run`, macOS LaunchAgent), history retention |
| Capture & HDR | cursor, delay, presets Faithful / Preserve highlights / Custom, operator/peak/knee/exposure/dither, and a live preview of a procedural scRGB scene through `ssx_hdr::to_sdr8` (yours vs faithful vs clipped, with a readout proving UI white stays byte-identical at knee 1.0) |
| Workflows | list/new/duplicate/delete/reorder (drag or Alt+Up/Down), name, hotkey, input kind, after-capture and after-upload steps with options, destination override incl. a separate video destination, templates, reset to defaults |
| Hotkeys | key-capture widget producing an `ssx_hotkeys::Chord`, duplicate and desktop-config conflicts, strategy per session, generated sway/Hyprland/GNOME/KDE snippets (show/copy), explicit opt-in Apply |
| Uploaders | built-ins, `[uploaders.*]` and imported `.sxcu`; per-type destinations; `.sxcu` import by dialog or drag and drop; Imgur/S3/HTTP forms; secrets go to the credential store and are never displayed; test upload with progress and cancel |
| History | searchable, filterable (kind/date/uploader) grid or list with paging, detail pane, copy/open link, open file/folder, upload again, delete (optionally the file), prune, missing-file markers, lazy thumbnails |
| Integration | file-manager menu install/uninstall with a per-file-manager report, `ssx doctor` diagnostics with "Copy report" |
| About | version, licence (GPL-3.0-or-later), where things are, third-party notices |

Screenshots of the real binary (Xvfb, software Vulkan) are in [`docs/`](docs).

## Structure

Each page is a pure `State` plus a thin `ui()`. Everything with logic is in a module of its
own that is tested without a window:

* `model` – working copy, dirty tracking, validation cache, atomic save, external-change
  detection (length + SHA-256 of the file) with Reload / Overwrite.
* `validation`, `nav` – mapping `ValidationIssue` paths to fields and pages, per-page dirty flags.
* `pattern_info`, `hdr_scene`, `preview_engine`, `debounce` – pattern analysis; the synthetic
  HDR scene and readouts; the coalescing off-thread preview worker; the debouncer.
* `hotkey_keys`, `hotkey_plan`, `hotkey_widget` – egui key events → `Chord`, plans for the
  compositor snippets, the widget.
* `reorder`, `reorder_ui`, `workflow_edit` – drag/keyboard reorder arithmetic, workflow editing helpers.
* `uploader_forms`, `uploader_registry`, `secrets` – per-kind field specs, the destination
  registry and test/re-upload tester, the secret vault trait (no way to read a value back).
* `history_view`, `thumbs` – filter → `Query`, paging, actions; LRU thumbnail cache with a
  two-thread lazy decoder.
* `autostart` – generators for the three platforms as pure functions, a trait, a fake.
* `host` – the machine as a bundle of trait objects (`Host::system` / `Host::sandboxed`) so
  tests never touch the real home, keyring, compositor or file manager.

Background work (loading history, orphans, uploads, secrets, diagnostics, the HDR preview,
thumbnails, file dialogs) runs on worker threads and reaches the UI through `task::Slot`,
which wakes the window when a result is ready.

## Verification

| What | How |
| --- | --- |
| Logic (≈250 tests, incl. proptests for reorder, debounce, hotkey round-trips, name uniqueness) | `cargo test -p ssx-settings-ui --lib` |
| Every page through egui events (≈130 tests) | `egui_kittest` + wgpu on lavapipe: `cargo test -p ssx-settings-ui --test ui` |
| Golden images of every page and key states (12) | `cargo test -p ssx-settings-ui --test snapshots`; regenerate with `UPDATE_SNAPSHOTS=1` |
| Real binary in a real X11 window | `tests/xvfb.rs`: Xvfb + lavapipe, `xdotool` keys and clicks, `import` screenshots of every page, a real click edit, Save, exit code and file on disk |
| Real binary on Wayland | `tests/sway.rs`: headless sway, window found by `app_id`, closed with `swaymsg kill`, clean exit and JSON |
| Windows / macOS | `cargo check --target x86_64-pc-windows-msvc -p ssx-settings-ui` (also `clippy --all-targets`) and `--target aarch64-apple-darwin` compile; the C build scripts of dependencies need a stand-in compiler when cross-checking from Linux |

The process-level tests skip with a printed reason when Xvfb, xdotool, ImageMagick, sway or a
Vulkan software driver are missing. They run the binary with a private `HOME`.

`SSX_UI_DUMP=dir cargo test -p ssx-settings-ui --test look --test xvfb` writes PNGs of every
page (in tall windows, and from the real binary) for looking at.

## Known limits and assumptions

* **Key capture.** `egui::Key` has no PrintScreen, Pause or Menu and `Modifiers` has no Super,
  so the widget offers modifier toggles plus a key list next to "Record" (which handles what
  egui does report, and tracks Super from the Super key events).
* **History uploader filter.** `ssx_core::history::Query` has no `uploader` field, so the
  filter is applied by scanning up to 5000 rows (the page says so when it truncates). An
  additive `Query::uploader: Option<String>` (a `WHERE uploader = ?`) would make it exact and cheap.
* **Autostart command.** It starts `ssx-tray --background` next to this executable (or on
  `PATH`); the page warns while that program is not installed.
* **Secrets without a keyring.** When the OS has no credential store the page says values are
  kept in memory only and lost when the window closes; nothing is ever written to `settings.toml`.
* Custom destinations that use `{json:…}`-style `.sxcu` features are checked by
  `ssx_services::upload::config::build`; anything it rejects is shown as the reason.
