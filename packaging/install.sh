#!/bin/sh
# Install md_editor from a binary tarball.
#
#   ./install.sh                      # into ~/.local (no sudo)
#   ./install.sh --prefix /usr/local  # system-wide (needs sudo)
#   ./install.sh --uninstall
#
# Everything is installed relative to PREFIX, so uninstalling is just removing
# the three files this script writes. Kept POSIX sh and dependency-free: the
# tarball has to install on a machine with no Rust, no make and no bash.

set -eu

BIN=md_editor
PREFIX=${PREFIX:-$HOME/.local}
action=install

srcdir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

usage() {
    cat <<EOF
Usage: $0 [--prefix DIR] [--uninstall]

  --prefix DIR   install root (default: \$HOME/.local)
  --uninstall    remove a previous install from PREFIX
EOF
}

while [ $# -gt 0 ]; do
    case $1 in
        --prefix)   PREFIX=${2:?--prefix needs a directory}; shift 2 ;;
        --prefix=*) PREFIX=${1#--prefix=}; shift ;;
        --uninstall) action=uninstall; shift ;;
        -h|--help)  usage; exit 0 ;;
        *)          echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

bindir=$PREFIX/bin
appsdir=$PREFIX/share/applications
icondir=$PREFIX/share/icons/hicolor/scalable/apps

refresh_caches() {
    # Optional: without them the launcher entry only shows up after re-login.
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database -q "$appsdir" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1; then
        gtk-update-icon-cache -qtf "$PREFIX/share/icons/hicolor" 2>/dev/null || true
    fi
}

if [ "$action" = uninstall ]; then
    rm -f "$bindir/$BIN" "$appsdir/$BIN.desktop" "$icondir/$BIN.svg"
    refresh_caches
    echo "Removed $BIN from $PREFIX."
    exit 0
fi

for f in "$BIN" "$BIN.desktop" "$BIN.svg"; do
    [ -f "$srcdir/$f" ] || { echo "missing $f next to $0" >&2; exit 1; }
done

mkdir -p "$bindir" "$appsdir" "$icondir"
cp "$srcdir/$BIN"          "$bindir/$BIN"
chmod 755                  "$bindir/$BIN"
cp "$srcdir/$BIN.desktop"  "$appsdir/$BIN.desktop"
cp "$srcdir/$BIN.svg"      "$icondir/$BIN.svg"
refresh_caches

echo "Installed $BIN to $PREFIX."
case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "Note: $bindir is not on your PATH." ;;
esac
