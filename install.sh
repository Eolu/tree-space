#!/usr/bin/env bash
#
# Install tree-space for the current user.
#
# The binaries are installed with `cargo install`, so they live in cargo's bin
# directory (~/.cargo/bin) — the same copy `cargo install tree-space` and a
# later `cargo install --path .` write. Nothing installs a second copy, so
# there is only ever one binary to keep current.
#
# The desktop integration (application entry, icon, folder/file MIME handler
# and the freedesktop FileManager1 D-Bus service) is installed under a prefix,
# ~/.local by default. Run ./uninstall.sh to undo it.
#
# Usage:
#   ./install.sh [--prefix DIR] [--no-build] [--no-desktop]
#
#   --prefix DIR   install the desktop integration under DIR (default: ~/.local)
#   --no-build     skip the binary install; do the desktop integration only
#   --no-desktop   install the binaries only; skip desktop/MIME/D-Bus integration
#
# Environment:
#   CARGO_FLAGS    extra flags for cargo (e.g. "--no-default-features" to drop
#                  the GStreamer-backed inline audio player)
set -euo pipefail

prefix="${HOME}/.local"
build=1
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
        --no-build)
            build=0
            shift
            ;;
        --no-desktop)
            desktop=0
            shift
            ;;
        -h|--help)
            sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cargo_bin="${CARGO_HOME:-${HOME}/.cargo}/bin"

if [ "$build" -eq 1 ]; then
    echo "Building and installing the binaries with cargo (into ${cargo_bin})..."
    # `cargo install` is the single owner of the binaries: it puts `tree-space`
    # and `ts` in cargo's bin directory, exactly where `cargo install tree-space`
    # and later `cargo install --path .` put them. Installing a copy under the
    # prefix instead would sit earlier on PATH (e.g. ~/.local/bin before
    # ~/.cargo/bin) and go stale the moment the project was rebuilt.
    # shellcheck disable=SC2086
    cargo install --path "${here}" --locked --force ${CARGO_FLAGS:-}
fi

if [ "$desktop" -eq 1 ]; then
    app_dir="${prefix}/share/applications"
    icon_dir="${prefix}/share/icons/hicolor/scalable/apps"
    dbus_dir="${prefix}/share/dbus-1/services"

    echo "Installing desktop entry, icon and D-Bus service..."
    install -Dm644 "${here}/resources/org.tree_space.panel.desktop" \
        "${app_dir}/org.tree_space.panel.desktop"
    install -Dm644 "${here}/resources/icons/hicolor/scalable/apps/org.tree_space.panel.svg" \
        "${icon_dir}/org.tree_space.panel.svg"
    install -Dm644 "${here}/resources/org.freedesktop.FileManager1.service" \
        "${dbus_dir}/org.freedesktop.FileManager1.service"

    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database "$app_dir" >/dev/null 2>&1 || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -qtf "${prefix}/share/icons/hicolor" >/dev/null 2>&1 || true

    # Make tree-space the handler for folders and file:// URIs. This is the
    # "default file manager" step; skip the whole block with --no-desktop.
    if command -v xdg-mime >/dev/null 2>&1; then
        mkdir -p "${XDG_CONFIG_HOME:-${HOME}/.config}"
        xdg-mime default org.tree_space.panel.desktop inode/directory
        xdg-mime default org.tree_space.panel.desktop x-scheme-handler/file
    fi
fi

cat <<EOF

Installed.
EOF

case ":${PATH}:" in
    *":${cargo_bin}:"*) ;;
    *)
        cat <<EOF

Note: ${cargo_bin} is not on your PATH. Add it, e.g.:
    echo 'export PATH="${cargo_bin}:\$PATH"' >> ~/.bashrc
EOF
        ;;
esac

cat <<EOF

Start the panel with:
    tree-space          # or the short alias: ts
    tree-space --help
EOF
