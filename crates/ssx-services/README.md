# ssx-services

The real implementations of `ssx-core`'s service traits. `ssx-core` runs workflows against 15
small traits and knows nothing about screens, HTTP or desktops; this crate is the application
layer that plugs the real thing in, once, for the CLI and the tray app.

| Trait(s) | Type | Built on | Notes |
|---|---|---|---|
| `Capturer` | `ScreenCapturer` | `ssx-platform` `ScreenSource` | Lazy backend detection; per-request HDR settings; last region persisted; interactive region through an injected `RegionSelector`. |
| `Uploaders`, `UrlShortener` | `UploadService` | `ssx-upload` | Registry from built-ins, `<config>/uploaders/*.sxcu` and `[uploaders.*]` tables; retries; cancellation and progress bridged from `CancelToken`. |
| `Clipboard` | `SystemClipboard` | `arboard`, `wl-copy`, `xclip` | Chain of backends; GNOME/KDE file-list formats. |
| `Notifier`, `UrlOpener` | `DesktopNotifier`, `SystemOpener` | `notify-rust`, `opener` | Only `http(s)` links are opened. |
| `CommandRunner` | `SystemCommandRunner` | `std::process` | No shell, timeout, cancellation, bounded output. |
| `Zipper` | `FolderZipper` | `zip` | Refuses symlinks that escape the folder; size/count/depth limits. |
| `Editor` | `ExternalEditor` | `ssx-editor-ui` helper | See the protocol below; `Unsupported` when the helper is absent. |
| `SaveDialog` | `NativeSaveDialog` (feature `save-dialog`) | `rfd` | Off by default. |
| `Recorder`, `Pinner`, `Ocr` | `StubRecorder`, `StubPinner`, `StubOcr` | – | Fail with a message naming the future crate. |
| `FileSystem`, `QrRenderer` | `ssx-core`'s own defaults | – | Re-used, not duplicated. |

`ProductionServices::new(settings, paths, options)` builds all of them (cheaply and infallibly:
everything expensive is lazy) and `.services(history)` lends them out as the `Services` bundle
the engine takes. The owner is a struct rather than a function returning `Services<'static>`
because the bundle borrows; `ProductionServices::leak` exists for processes that want
`'static`.

## HDR settings

`hdr::tonemap_settings` converts core's `HdrConfig` to `ssx-hdr`'s `TonemapSettings`. The two
crates differ in one place that matters: core's `exposure` is **EV stops**, `ssx-hdr`'s is a
**linear multiplier**, so the conversion is `2^EV`. `peak`, `knee`, `operator` and `dither` are
the same quantities; a knee of `0` is raised to `0.01` because `ssx-hdr` requires `> 0`. Unit
tests cover the numbers and a pixel-level test proves one stop halves/doubles linear light.

## Uploader configuration

`[uploaders.<name>]` tables have a `type`:

| `type` | Required keys |
|---|---|
| `imgur` | `client_id`, or `access_token = "keyring:<name>"` |
| `s3` | `bucket`, and for the default `preset = "custom"` an `endpoint`; presets `aws`, `r2`, `b2`, `wasabi`, `minio`; `access_key_id` / `secret_access_key` as `keyring:` references |
| `http` | `url` (with `{filename}`); `method`, `body = "raw" \| "multipart"`, `auth`, `auth_secret = "keyring:..."`, `result` |
| `local` | none (`dir` copies, `base_url` rewrites the URL) |
| `sxcu` | `file` (relative to the config directory) |
| `shortener` | `service = "is.gd" \| "v.gd" \| "tinyurl"`, or `endpoint` |

Built-in names that need no table: `local`, `is.gd`, `v.gd`, `tinyurl`. Unknown keys are rejected
with the list of valid ones. A destination that fails to load stays in the registry as *broken*
with the reason instead of failing the whole service.

## Secrets

Secrets are `keyring:<name>` references in the settings and resolved at upload time. Lookup
order: environment variable `SSX_SECRET_<NAME>` (upper-cased, other characters become `_`), the
OS credential store (Secret Service, Credential Manager, Keychain via `keyring`), then memory.
With no credential store (headless machines) writes stay in memory with a one-time warning; the
process never fails because of it.

## Clipboard

X11 and Wayland clipboards are served by the *source* process, so a short-lived CLI that copies
with `arboard` and exits loses the contents. The CLI therefore prefers `wl-copy` / `xclip`
(they fork and keep serving); a long-running app keeps `arboard` first
(`ClipboardOptions::prefer_external`). `arboard` writes file lists as `text/uri-list` on Linux
(what KDE and Qt apps read); GNOME Files, Nemo, Caja and Thunar want
`x-special/gnome-copied-files`, which is written through the external tools.

## Editor helper protocol

```text
ssx-editor-ui --output <out.png> <in.png>
```

Exit `0` with `<out.png>` written = accepted; exit `3` (or exit `0` without output) = the user
cancelled; anything else = failure, with the stderr tail shown. Found via `SSX_EDITOR_UI`, next
to the running executable, then `PATH`.

## Verification

`cargo test -p ssx-services` runs everything headless: fake `CaptureBackend`s, local wiremock
servers for uploads (streaming, progress, cancellation, retries), real subprocesses for the
command runner and the editor protocol, real zip files, a fake clipboard chain and recording
tool runners. What is not covered here: the real `arboard` on a running desktop, real
notification daemons and Windows/macOS behaviour (compile-checked only); `ssx-cli`'s end-to-end
tests cover `xclip` under Xvfb.
