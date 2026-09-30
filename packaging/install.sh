#!/bin/sh
# Installs ssx from an unpacked release archive. No root needed by default.
#
#   ./install.sh                     install into ~/.local (bin/ssx, bin/ssx-app, ...)
#   ./install.sh --prefix /usr/local install system-wide (run with sudo)
#   ./install.sh --integrate         also start-at-login and file-manager right-click entries
#   ./install.sh --dry-run           print what would happen, change nothing
#
# Nothing here edits your compositor or desktop configuration. Keyboard shortcuts are a separate,
# explicit step: `ssx hotkeys print`, then `ssx hotkeys install`.
set -eu

prefix="${HOME:-}/.local"
integrate=0
dry=0

usage() { sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || { echo "install.sh: --prefix needs a directory" >&2; exit 2; }
            prefix="$2"; shift 2 ;;
        --prefix=*) prefix="${1#--prefix=}"; shift ;;
        --integrate) integrate=1; shift ;;
        --dry-run) dry=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

if [ -z "$prefix" ] || [ "$prefix" = "/.local" ]; then
    echo "install.sh: cannot work out where to install (HOME is not set); use --prefix" >&2
    exit 2
fi

here="$(cd "$(dirname "$0")" && pwd)"
[ -d "$here/bin" ] || { echo "install.sh: $here/bin not found; run this from the unpacked archive" >&2; exit 1; }

run() {
    if [ "$dry" -eq 1 ]; then printf 'would run:'; printf ' %s' "$@"; printf '\n'; else "$@"; fi
}

echo "Installing ssx into $prefix/bin"
run mkdir -p "$prefix/bin"
for f in "$here"/bin/*; do
    name="$(basename "$f")"
    # install(1) writes to a temporary name and renames, so a running ssx-app is not corrupted.
    run install -m 0755 "$f" "$prefix/bin/$name"
done

case ":${PATH:-}:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: $prefix/bin is not on your PATH; add it to your shell profile." ;;
esac

if [ "$integrate" -eq 1 ]; then
    ssx="$prefix/bin/ssx"
    echo "Enabling start at login and file-manager entries"
    run "$ssx" daemon autostart enable
    run "$ssx" shell install
fi

cat <<NEXT

Installed. Next steps:
  ssx doctor                       what was detected on this machine
  ssx daemon start                 tray icon, hotkeys, batching right-click uploads
  ssx hotkeys print                bindings for sway, Hyprland, GNOME or KDE (changes nothing)
  ssx hotkeys install              install them (GNOME and KDE also need --apply)
  ssx-settings-ui                  settings, workflows, uploaders, history
Remove everything with ./uninstall.sh
NEXT
