# ssx-core

Portable core of ssx: settings, filename patterns, history database, the workflow engine
(ShareX's "post screenshot / post file / post video" pipeline) and the IPC message types.

No GUI, no OS capture API, no uploader code. The crate builds and is fully testable on Linux,
Windows and macOS; everything it needs from the outside world is a small trait (see
[Services](#services)) that the application layer implements and plugs in.

| Module | What it does |
|---|---|
| `settings` | Versioned TOML `Settings`, workflows, destinations, validation, atomic save, migrations, config/data dirs (`SSX_CONFIG_DIR` override). |
| `pattern` | ShareX-compatible filename/folder patterns (`%y-%mo-%d_%h-%mi-%s`, `%t`, `%i`, `%ra{8}`, ...), sanitising, race-free unique file creation, file-locked `%i` counter. |
| `history` | SQLite (bundled, WAL) history with thumbnails, FTS5 search, migrations, retention, orphan detection. |
| `workflow` | The engine, the service traits, events, reports, and (feature `testing`) recording mocks. |
| `ipc` | Request/response types plus a JSON-lines codec for tray app, CLI and shell shims. |

## Wiring it up in an application

```rust,ignore
use std::sync::Arc;
use ssx_core::{
    history::History,
    pattern::FileCounter,
    settings::{Paths, Settings},
    workflow::{CancelToken, Engine, NullSink, Naming, Services},
};

let paths = Paths::discover()?;                       // honours SSX_CONFIG_DIR
let loaded = Settings::load_or_recover(&paths.settings_file())?;
for w in &loaded.warnings { tracing::warn!("{w}"); }   // unknown keys, migrations, ...
let settings = loaded.settings;

let history = History::open(&paths.history_db())?;
let naming = Naming::system(Arc::new(FileCounter::new(paths.counter_file())));
let engine = Engine::new(settings.clone(), naming);

// Start from the defaults (std file system, process runner, QR renderer, everything else
// "unsupported") and plug in what the application has.
let services = Services {
    capturer: &my_capturer,       // ssx-platform
    uploaders: &my_uploaders,     // ssx-upload
    clipboard: &my_clipboard,
    notifier: &my_notifier,
    history: Some(&history),
    ..Services::with_defaults()
};

let workflow = settings.find_workflow("region").expect("configured");
let cancel = CancelToken::new();                       // give a clone to the UI's Cancel button
let report = engine.post_screenshot(workflow, &services, &NullSink, &cancel);
println!("{}", report.summary());
```

Run it on a worker thread: `run` blocks until the workflow is done (it can be waiting for
the user in an editor or for an upload). Events stream to the `EventSink` you pass in.

## Services

All traits are object-safe and `Send + Sync`, live in `workflow::services`, and are bundled in
`workflow::Services`. Blocking calls take a `CancelToken`; errors are `ServiceError`
(`Cancelled`, `Unsupported`, `NotConfigured`, `Io`, `Failed { retryable }`) whose messages are
shown to the user verbatim.

| Trait | Purpose | Notes |
|---|---|---|
| `Capturer` | region / fullscreen / monitor / window / last region -> `ssx_types::Frame` | The frame **must already be 8-bit sRGB** (tonemap HDR here; `CaptureRequest.hdr` carries the settings). The engine sleeps the capture delay itself. `Cancelled` = user pressed Esc. |
| `Recorder` -> `RecordingSession` | `start(req)` then `stop()` -> file, or `abort()` | The engine picks output dir and file stem. The run waits for the `stop` token you give `post_video`. |
| `Editor` | edit a frame -> `Edited(Frame)` / `Cancelled` | Cancelling cancels the item. |
| `Uploaders` | upload a file (stream it) or bytes to a *named* destination | The engine resolves the name from the settings (workflow override > extension override > default). A result with an empty URL is treated as failed. |
| `UrlShortener` | shorten a URL with a named provider | |
| `Clipboard` | set image / text / file list, read for the `clipboard` input | |
| `Notifier` | notifications, optional `show_qr` | |
| `UrlOpener` | open a URL in the browser | |
| `QrRenderer` | text -> image | `QrCodeRenderer` (default) uses the `qrcode` crate. |
| `CommandRunner` | run a program, **no shell** | `ProcessCommandRunner` (default): direct spawn, timeout, cancel, bounded stderr. |
| `FileSystem` | save / read / remove / stat | `StdFileSystem` (default). |
| `Zipper` | zip a folder to a temp file | The engine deletes the archive afterwards. |
| `SaveDialog`, `Pinner`, `Ocr` | save-as, pin to screen, OCR | |

`Services::with_defaults()` fills every trait the app has not implemented with `Unsupported`,
so each affected step fails cleanly with "X is not supported here" instead of the app failing
to compile or crashing.

`history: Option<&History>` is concrete (SQLite in memory works fine in tests).

## Engine semantics

* Steps run **in the order written** (`after_capture`, then `after_upload`) and each yields a
  `StepReport` (`Succeeded`, `Failed`, `Skipped(reason)`, `Cancelled`).
* **Importance** (`StepKind::importance`): *critical* steps (getting input, editor) end the
  item when they fail; *normal* steps (save, upload, shorten, run command, delete) are recorded
  and the run degrades to `PartialSuccess` if something useful was achieved; *optional* steps
  (clipboard, notification, browser, QR, pin, OCR, history) only produce warnings.
* A **failed upload keeps the local file**, says why, skips URL-dependent steps (`NoUrl`) and
  (if the workflow has `show_notification`) notifies with the error and where the file is.
* `delete_local_file` runs only after a **confirmed** upload, only for files the workflow
  created, never for files passed to `post_file`, and treats "already gone" as success.
  Folder zips are temporary and always removed.
* **Cancellation**: checked before every step and passed into every blocking call. Outcome
  `Cancelled`; remaining steps are skipped-cancelled. What was already saved/uploaded is still
  recorded in the history.
* **`post_file`** with several paths: parallel (`post_file.max_parallel_uploads`), one failure
  never stops the others, results in input order. After-upload steps run once over the
  successful items: `copy_url` puts all URLs on the clipboard newline-joined,
  `show_notification` sends one summary, `run_command` / `open_url` run per item, image-only
  steps and the QR code are skipped as ambiguous.
* `run_command`: each argument is template-expanded **on its own** (`{path}`, `{dir}`,
  `{file_name}`, `{url}`, `{short_url}`, `{thumbnail_url}`, `{deletion_url}`; `{{` `}}` for
  literal braces); values are never re-interpreted and nothing goes through a shell.
* Panics inside a service are caught and reported as a failed step.

## Testing your own code against the engine

Enable the `testing` feature for `workflow::testing`: `TestWorld` wires recording mocks for
every service (one shared, ordered `Log`), an in-memory file system and an in-memory history,
with a fixed clock so file names are deterministic.

## Settings file

`settings.toml` in the config dir. Missing keys take defaults, unknown keys produce warnings,
files from a newer ssx are refused untouched, older ones are migrated (a `.vN.bak` copy is kept).
`Settings::save` validates first and refuses plain-text secrets: uploader secrets are stored as
`keyring:<name>` references, never as values. `[uploaders.<name>]` tables are opaque TOML for
the upload crate to interpret.

## Known limits

* `%mon` / `%w` are English only; `%wy` is the ISO week (see `pattern` docs for the full
  token list and deviations from ShareX).
* IPC paths must be valid UTF-8 (JSON).
* The Windows build of the bundled SQLite needs the MSVC tools; it was not compiled in the
  Linux authoring environment.
