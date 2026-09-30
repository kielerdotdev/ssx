# ssx-shell

Adds **right-click entries for ssx** to your file manager, on every OS:

| Entry | Runs | Shown for |
|---|---|---|
| Upload with ssx | `ssx post-file -- <files...>` | any selection, files and folders |
| Edit image with ssx | `ssx edit -- <image>` | images (png, jpg, gif, webp, bmp, tiff, avif, heic) |
| Upload video with ssx | `ssx post-video -- <video>` | videos (mp4, mkv, webm, mov, avi, ...) |

The entries only *start the ssx command line*; there is no logic in them. Files are always
handed over as separate arguments after `--`, never through a shell string, so names with
spaces, quotes, `$`, backticks, newlines or a leading `-` are safe.

Installing is idempotent, and everything is reversible: uninstalling removes exactly what was
installed (files we did not create are never touched or overwritten) and leaves your home
directory as it was.

## What gets installed where

### Linux

| File manager | What is written | Notes |
|---|---|---|
| **Nautilus** (GNOME Files) | `~/.local/share/nautilus-python/extensions/ssx-shell.py` if the `nautilus-python` package is installed, otherwise scripts `~/.local/share/nautilus/scripts/{Upload with ssx,Edit image with ssx,Upload video with ssx}` | The extension gives top-level menu items filtered by file type; it needs `python3-nautilus` (Debian/Ubuntu) / `nautilus-python` (Fedora/Arch) and a restart (`nautilus -q`). Scripts work without dependencies but sit in the right-click **Scripts** submenu and cannot be hidden by file type (the edit/video scripts silently ignore files of the wrong type). Only local files are supported. |
| **Dolphin** (KDE) | `~/.local/share/kio/servicemenus/ssx-{upload,edit,upload-video}.desktop` (mode 755) | Optionally also `~/.local/share/kservices5/ServiceMenus/` for old KDE. The executable bit is required: KDE ignores non-executable service menus in user directories. |
| **Thunar** (Xfce) | Three `<action>` entries merged into `~/.config/Thunar/uca.xml` (ids `ssx-shell-*`) | Your existing custom actions and any other content are preserved byte for byte; uninstall removes only our entries (and the file, if we created it). Restart Thunar (`thunar -q`). |
| **Nemo** (Cinnamon) | `~/.local/share/nemo/actions/ssx-{upload,edit,upload-video}.nemo_action` | |
| **Any file manager** (PCManFM(-Qt), Caja, ...) | `~/.local/share/applications/ssx-{upload,edit,upload-video}.desktop` | Appears in **Open With**. Runs `update-desktop-database` when available. Do not install ssx into a path containing `%` (GLib cannot load such entries). |
| **yazi / ranger / lf** | nothing | `ssx_shell::linux::snippets` returns key-binding snippets to paste into your config. |

Detection uses installed binaries (`nautilus`, `dolphin`, `thunar`, `nemo`), the file
manager's config directories and `XDG_CURRENT_DESKTOP`; undetected file managers are skipped
(pass `force` to install anyway). Flatpak builds cannot write these files; use Open With.

### Windows

| Where | What |
|---|---|
| `HKCU\Software\Classes\*\shell\ssx.upload` and `...\Directory\shell\ssx.upload` | "Upload with ssx" for files and folders |
| `HKCU\Software\Classes\SystemFileAssociations\image\shell\ssx.edit` (+ `.webp/.avif/.heic/.heif`) | "Edit image with ssx" |
| `HKCU\Software\Classes\SystemFileAssociations\video\shell\ssx.upload-video` (+ `.mkv/.webm/...`) | "Upload video with ssx" |
| `%APPDATA%\Microsoft\Windows\SendTo\Upload with ssx.cmd` | **Send to** entry: passes *all* selected files in one launch |

Only `HKEY_CURRENT_USER` is used (no admin rights). Each key has `MultiSelectModel=Player`,
an icon, and a marker value `SsxManaged=1`; keys without the marker are never modified.

On **Windows 11** the classic verbs are under **Show more options** (Shift+F10). A top-level
entry needs a signed sparse package and a COM DLL; that is designed but not built yet, see
[`docs/windows11-context-menu.md`](docs/windows11-context-menu.md).

Explorer starts a static verb with `%1` only, and depending on version launches one process per
selected file. The verb therefore runs `ssx post-file --coalesce -- "%1"`: **the CLI must
accept `--coalesce`**, forward its paths to the running app over `ssx-ipc`, and the app merges
requests arriving within a short window (~400 ms) into one batch. Explorer also hides verbs for
selections above 15 (100 with `Player`) items; use **Send to** for huge selections.

### macOS

`~/Library/Services/{Upload with ssx,Edit image with ssx,Upload video with ssx}.workflow`
(Automator Quick Actions containing one *Run Shell Script* action that receives the files as
arguments). They appear under **right-click > Quick Actions** and Services. The installer runs
`pbs -update`; if an entry is missing enable it in *System Settings > Keyboard > Keyboard
Shortcuts > Services > Files and Folders*.

## Removing

Use the app's uninstall action (`Integrations::uninstall_all`), or delete the files/keys
listed above by hand: everything we write contains the text `ssx-shell-managed` (registry keys
carry `SsxManaged=1`, Thunar entries have ids starting with `ssx-shell-`).

## Using the crate

```rust
use ssx_shell::{Context, Integrations};

let ctx = Context::from_env("/usr/bin/ssx")?;          // absolute path to the ssx CLI
let integrations = Integrations::native();
let report = integrations.install_all(&ctx);            // never aborts on one failure
print!("{report}");                                     // [ok  ] Nautilus ...  installed
```

Add your own entries by pushing an `Action` onto `ctx.actions`. All environment access goes
through `Context` (paths, `PATH`, helper processes) and the `RegistryBackend` trait, so tests
run in a temp directory; see `Context::sandboxed`.

## Verification status

* Verified by tests on Linux: all generated artefacts (golden files), install ->
  re-install -> uninstall leaving the sandbox HOME byte-identical, merging into existing
  `uca.xml`, detection matrix, failure isolation, hostile file names run through real `sh`,
  `dash`, `bash`, GLib (`gio launch`) and the generated nautilus-python extension under real
  Python with stub `gi` modules.
* **Not run on the real target**: the Windows registry backend (`windows-registry`, compile and
  clippy checked for `x86_64-pc-windows-msvc`), Explorer's behaviour with multi-selection,
  macOS Quick Actions (plist structure follows Automator's output and is parse-checked only),
  and the real Nautilus, Dolphin, Thunar, Nemo and PCManFM (generated per their documented
  formats, and for Nemo/GLib against their source/parsers).
