# ssx-hotkeys

Global hotkeys for ssx on every target desktop, with honest fallbacks. No single mechanism
works everywhere, so the crate has three and knows when to use which:

| Desktop | Mechanism | In this crate |
|---|---|---|
| Windows, macOS, **X11** (any DE) | grab the keys in-process | `GlobalHotkeys` (the `global-hotkey` crate) |
| **Wayland**: KDE Plasma, GNOME 48+ | ask the compositor via the XDG GlobalShortcuts portal | `PortalHotkeys` (`ashpd`) |
| **sway** | `bindsym` lines in an include file | `bindings::sway` |
| **Hyprland** | `bind = ...` lines in a `source`d file | `bindings::hyprland` |
| **GNOME** without the portal | `gsettings` custom keybindings | `bindings::gnome` |
| **KDE** without the portal | KDE "command shortcuts" | `bindings::kde` |
| anything else | tell the user to bind `ssx capture ...` themselves | `Strategy::CliOnly` |

```rust
use ssx_hotkeys::{Chord, HotkeyId, HotkeyManager, open_best_manager};

let mut mgr = open_best_manager()?;                       // detects the desktop
mgr.register(HotkeyId::new("capture-region")?, "Ctrl+Shift+S".parse()?)?;
while let Ok(ev) = mgr.events().recv() {
    println!("{} {:?}", ev.id, ev.state);
}
```

`open_best_manager()` returns an error listing the generator strategies when the desktop
has no in-process option (sway, Hyprland, GNOME < 48 ...); use `bindings` then.

Try everything from a terminal:
`cargo run -p ssx-hotkeys --example hotkeys -- detect | print sway | install sway | listen`.

## Chords

`Chord` parses and formats `Ctrl+Shift+S`, `Print`, `Super+Alt+R`, `F13`, `VolumeMute`:
case-insensitive, spaces around `+` allowed, aliases (`Control`, `Win`, `Cmd`, `Meta`, `PgUp`,
`PrtSc`, `Esc`, ...). Formatting is canonical (`Ctrl+Alt+Shift+Super+Key`), so
`parse(format(c)) == c`. Typing keys (letters, digits, Space, arrows, ...) need a modifier;
function, `Print`, `Pause` and media keys may be bare. Each chord converts to
`global_hotkey::HotKey`, an XDG portal trigger, a GNOME accelerator, a Qt sequence (KDE) and
XKB names (sway/Hyprland). A key that is `+` itself is not supported (write `Equal`).

## In-process backends: running them

* **X11**: nothing to do; `global-hotkey` runs its own thread and X connection. It only
  *logs* failures to connect, so `GlobalHotkeys::new()` checks `$DISPLAY` first. Grabs made
  through XWayland fire only while an XWayland window has focus, so this backend is never
  chosen on Wayland sessions.
* **Windows**: create the manager on the thread that runs your Win32 message loop (winit,
  tao and egui loops qualify); events only flow while it pumps messages.
* **macOS**: create it on the main thread, with the main run loop running.
* Create **one** `GlobalHotkeys` per process: the crate's event channel is process-global.
* **Portal** (`PortalHotkeys`): a private thread with its own D-Bus connection; nothing to
  run. The chord is only a *preference*: the compositor may ask the user or assign another
  key (`triggers()` reports what was bound). Some portals allow `BindShortcuts` once per
  session, so each change rebinds the whole set in a fresh session (use `register_all` to
  bind several at once). **Key release events may never arrive** on the portal
  (`reports_release()` is `false`); do not build push-to-hold features on it. Known support:
  KDE Plasma yes, GNOME 48+ yes, Hyprland yes (needs a user `global` bind), wlroots/sway and
  the GTK portal no.

## Generators

All take `&[(Chord, Command)]` where `Command` is a program plus arguments (never a shell
string), and all file writing takes an injectable `Dirs` root and a `CommandRunner`, so tests
never touch the real home directory.

**sway / Hyprland**: `files::write_include_file` writes `~/.config/sway/config.d/ssx.conf`
or `~/.config/hypr/ssx.conf` and returns the exact line to add (`include ...` /
`source = ...`). Your main config is never touched unless you call
`files::install_main_include` (`--apply`), which appends a marked block
(`# >>> ssx hotkeys >>>` ... `# <<< ssx hotkeys <<<`), idempotently, in place (symlinked
dotfiles survive), and refuses to *create* a main config (that would shadow the system
default). `uninstall_main_include` removes exactly that block (byte-for-byte for a file that
ended in a newline); `reload` runs `swaymsg reload` / `hyprctl reload` on request.
`conflict::check_main_config` warns when your config already binds one of the chords
(resolves `set $mod ...` / `$mainMod = ...`).

**GNOME**: `gnome::commands` is the exact `gsettings` command list (and `script` a shell
script). `gnome::apply` runs it: it merges our `.../custom-keybindings/ssx-<slug>/` paths into
the existing `custom-keybindings` list (other entries and order untouched), skips values that
already match (idempotent) and `remove` deletes only `ssx-` entries. GNOME's own bindings
(notably `Print`) clash with custom ones: choose keys GNOME does not use.

**KDE**: the mechanism System Settings uses for *Add Command*: a launcher
`~/.local/share/applications/net.local.ssx-<slug>.desktop` with `NoDisplay=true` and
`X-KDE-GlobalAccel-CommandShortcut=true`, plus `[services][net.local.ssx-<slug>.desktop]
_launch=<Qt sequence>` in `kglobalshortcutsrc` written with `kwriteconfig6` (fallback
`kwriteconfig5`). `kglobalacceld` reads that file at start-up, so shortcuts become active
after `kde::reload` (restarts `plasma-kglobalaccel.service`) or the next login. Keypad keys
have no Qt name and are rejected.

### Quoting

Each target parses the command text differently, so each has its own tested escaping
(`command.rs`): POSIX quoting; **sway** double-quoted words (its parser splits at `;`/`,`,
expands `$var`, and a `bindsym` gets one *extra* unescape level: found by injecting real key
presses); **Hyprland** neutralises `$var` and `#`; **GNOME** GVariant strings around
`g_shell_parse_argv` text; **KDE** Desktop Entry `Exec` rules. Newlines/NUL in a command are
rejected.

## Detection

`detect(&Environment::from_env(), Platform::current())` reads `XDG_CURRENT_DESKTOP`,
`XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`, `DISPLAY`, `SWAYSOCK` and
`HYPRLAND_INSTANCE_SIGNATURE` and returns ordered `Strategy` candidates (e.g. GNOME on
Wayland: `[Portal, GnomeGsettings]`). It is a pure function of an environment snapshot, so
it is unit-tested with a fixture matrix.

## What was verified how

| Piece | Verified against |
|---|---|
| Chords, keys, detection, all generators' text | unit + golden + property tests (`tests/golden/`) |
| `global-hotkey` on X11 | real `Xvfb`, `xdotool key` -> press + release events |
| sway output | real headless sway: every key/modifier spelling parses; a hostile-argument corpus survives sway's parser and `sh` at load time **and when real key presses are injected** (`wtype`); `include` of a path with spaces |
| GNOME output | real `gsettings` with a private compiled copy of the schemas (keyfile backend): exact read-back, merge, idempotency, removal, hostile strings |
| Portal backend | real `ashpd` client against a mock portal on a private `dbus-daemon` (not a real KDE/GNOME) |
| Hyprland output | Hyprland syntax from its wiki; **not run** against Hyprland |
| KDE output | file formats from System Settings' behaviour and fakes; **not run** against Plasma |
| Windows / macOS backends | compile-checked only (`cargo check --target x86_64-pc-windows-msvc / aarch64-apple-darwin`) |

Tests skip with a printed reason when `Xvfb`, `xdotool`, `sway`, `wtype` (key injection),
`dbus-daemon`, `gsettings` or `glib-compile-schemas` are missing.
