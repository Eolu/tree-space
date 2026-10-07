#!/usr/bin/env bash
#
# Undo ./install.sh: remove the tree-space binaries, desktop entry, icon and
# D-Bus service, and restore Nautilus as the folder/file handler if it is
# installed.
#
# Usage:
#   ./uninstall.sh [--prefix DIR] [--no-desktop]
#
#   --prefix DIR   prefix used at install time (default: ~/.local)
#   --no-desktop   only remove the binary; leave desktop integration alone
set -euo pipefail

prefix="${HOME}/.local"
desktop=1

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            prefix="${2:?--prefix needs a directory}"
            shift 2
            ;;
        --prefix=*)
            prefix="${1#*=}"
            shift
            ;;
        --no-desktop)
            desktop=0
            shift
            ;;
        -h|--help)
            sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
done

echo "Removing binaries from ${prefix}/bin..."
rm -f "${prefix}/bin/tree-space" "${prefix}/bin/ts"

if [ "$desktop" -eq 1 ]; then
    app_dir="${prefix}/share/applications"
    echo "Removing desktop entry, icon and D-Bus service..."
    rm -f "${app_dir}/org.tree_space.panel.desktop"
    rm -f "${prefix}/share/icons/hicolor/scalable/apps/org.tree_space.panel.svg"
    rm -f "${prefix}/share/dbus-1/services/org.freedesktop.FileManager1.service"
    # Older installs used a different desktop file name.
    rm -f "${app_dir}/tree-space.desktop"

    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database "$app_dir" >/dev/null 2>&1 || true

    # Hand folder/file handling back to Nautilus, but only if it is actually
    # installed; otherwise leave the user's default untouched.
    nautilus_desktop=""
    for dir in "${prefix}/share/applications" /usr/local/share/applications /usr/share/applications; do
        if [ -f "${dir}/org.gnome.Nautilus.desktop" ]; then
            nautilus_desktop="org.gnome.Nautilus.desktop"
            break
        fi
    done
    if [ -n "$nautilus_desktop" ] && command -v xdg-mime >/dev/null 2>&1; then
        xdg-mime default "$nautilus_desktop" inode/directory || true
        xdg-mime default "$nautilus_desktop" x-scheme-handler/file || true
    fi
fi

echo "Uninstalled."
