#!/bin/sh
# Removes what install.sh installed: the ssx programs, the login entry and the right-click
# entries. Your settings and history (~/.config/ssx, ~/.local/share/ssx) are NOT touched.
#
#   ./uninstall.sh [--prefix DIR] [--dry-run]
set -eu

prefix="${HOME:-}/.local"
dry=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || { echo "uninstall.sh: --prefix needs a directory" >&2; exit 2; }
            prefix="$2"; shift 2 ;;
        --prefix=*) prefix="${1#--prefix=}"; shift ;;
        --dry-run) dry=1; shift ;;
        -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "uninstall.sh: unknown option: $1" >&2; exit 2 ;;
    esac
done
if [ -z "$prefix" ] || [ "$prefix" = "/.local" ]; then
    echo "uninstall.sh: cannot work out where ssx was installed (HOME is not set); use --prefix" >&2
    exit 2
fi

run() {
    if [ "$dry" -eq 1 ]; then printf 'would run:'; printf ' %s' "$@"; printf '\n'; else "$@"; fi
}

ssx="$prefix/bin/ssx"
if [ -x "$ssx" ]; then
    # Best effort: each of these only removes what ssx itself created, and none of them may
    # stop the file removal below.
    run "$ssx" daemon stop || true
    run "$ssx" daemon autostart disable || true
    run "$ssx" shell uninstall || true
    run "$ssx" hotkeys uninstall || true
fi
for name in ssx ssx-app ssx-overlay ssx-editor-ui ssx-settings-ui; do
    run rm -f "$prefix/bin/$name"
done
echo "Removed the ssx programs from $prefix/bin. Settings and history were left in place."
