#!/usr/bin/env bash
#
# Install tree-space into a user prefix (~/.local by default) and register it
# with the desktop: application entry, icon, MIME handler for folders, and the
# freedesktop FileManager1 D-Bus service. Run ./uninstall.sh to undo it.
#
# Usage:
#   ./install.sh [--prefix DIR] [--no-build] [--no-desktop]
#
#   --prefix DIR   install under DIR (default: ~/.local)
#   --no-build     do not run `cargo build`; install the existing release binary
#   --no-desktop   install the binary only; skip desktop/MIME/D-Bus integration
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
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
done

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin_dir="${prefix}/bin"

if [ "$build" -eq 1 ]; then
    echo "Building (cargo build --release --locked ${CARGO_FLAGS:-})..."
    # shellcheck disable=SC2086
    cargo build --release --locked ${CARGO_FLAGS:-} --manifest-path "${here}/Cargo.toml"
fi

src_bin="${here}/target/release/tree-space"
if [ ! -x "$src_bin" ]; then
    echo "error: ${src_bin} not found; run without --no-build" >&2
    exit 1
fi

echo "Installing binaries to ${bin_dir}..."
install -Dm755 "$src_bin" "${bin_dir}/tree-space"
install -Dm755 "${here}/target/release/ts" "${bin_dir}/ts"

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
    *":${bin_dir}:"*) ;;
    *)
        cat <<EOF

Note: ${bin_dir} is not on your PATH. Add it, e.g.:
    echo 'export PATH="${bin_dir}:\$PATH"' >> ~/.bashrc
EOF
        ;;
esac

cat <<EOF

Start the panel with:
    tree-space          # or the short alias: ts
    tree-space --help
EOF
