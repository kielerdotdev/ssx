# ssx-cli

The `ssx` command-line tool. It is a thin front end: parsing (clap), wiring, rendering. The work
is done by `ssx-core` (workflow engine, settings, history) and `ssx-services` (real services).

```text
ssx capture fullscreen -o shot.png          # pixel-exact PNG of all monitors
ssx capture region --rect 100,100,800,600 --upload --copy-url
ssx capture window --active | monitor [--id ID] | last-region
ssx upload --to my-host a.png b.png          # one URL per line on stdout
ssx post-file --coalesce -- FILES...         # what "Upload with ssx" in a file manager runs
ssx run capture-region-edit                  # any workflow from settings.toml
ssx uploaders import my-host.sxcu            # ShareX custom uploaders
ssx history list | search TEXT | show ID | delete ID | prune | open ID
ssx config path | show | validate | edit | set KEY VALUE | reset
ssx hotkeys detect | print [--target sway|hyprland|gnome|kde] | install [--apply] | uninstall
ssx shell status | install [--dry-run] | uninstall
ssx doctor [--json]                          # what works here, and why not
ssx completions bash|zsh|fish|elvish|powershell
```

## Conventions

* **stdout is the result, stderr is everything else.** After a workflow, stdout holds the URLs
  (one per line, input order) if anything was uploaded, otherwise the saved paths; `--json`
  prints one document with everything. Progress, warnings and errors go to stderr.
* **Exit codes:** `0` ok, `1` error (also a workflow that saved but could not upload), `2` usage
  error, `3` cancelled (Ctrl-C, closed the editor).
* **Errors** print as `error: <what> (hint: <what to do>)`.
* `-v/-vv/-vvv` raise the log level, `RUST_LOG` overrides; `NO_COLOR` and `--color` are
  respected. `SSX_CONFIG_DIR` (or `--config-dir`) relocates settings and data.
* **Nothing edits your desktop configuration silently.** `hotkeys install` writes only ssx's own
  include file (sway, Hyprland) and prints the line to add; `--apply` makes the change. GNOME and
  KDE shortcuts need `--apply`. `shell install --dry-run` shows what would be written.
* `config set` edits the TOML text in place (comments and unknown keys survive) and refuses
  anything that would not validate; nothing is written in that case.

## Not available yet

Interactive region selection (`capture region` without `--rect`) needs the overlay crate, which
plugs into `ssx_services::RegionSelector`; until then it fails with an explanation. Recording,
pin-to-screen and OCR are stubs in `ssx-services`. `edit` / `--edit` need the `ssx-editor-ui`
helper (protocol in the `ssx-services` README).

## Tests

Unit tests live next to the code (grammar, renderers, settings editing, hotkey bindings, report
and doctor logic). `tests/` runs the real binary end to end: under a private Xvfb with painted
known colours (pixel-exact capture, crop, formats, upload to a local mock server with an imported
`.sxcu`, history, clipboard through `xclip`, Ctrl-C cancellation), against a headless sway
(`SSX_BACKEND=wayland`), and for uploads of hostile file names, folder zips, forwarding to a
fake running instance over `ssx-ipc`, configuration, hotkeys (golden files), shell integration in
a sandboxed `HOME`, diagnostics and exit codes. Tests skip with a printed reason when Xvfb, sway
or xclip are missing.
