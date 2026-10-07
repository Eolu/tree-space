# tree-space

Installed by the Arch package at `/usr/share/tree-space/README.md`.

tree-space is a dockable, keyboard-first file manager panel for Wayland
compositors that implement `wlr-layer-shell` (Hyprland, sway, river, niri, and
other wlroots-based compositors). On a compositor without it (GNOME, KDE) the
panel runs as a floating window instead of a dock.

Start the panel:

    tree-space

The binary is `/usr/bin/tree-space`. The short alias `ts` is **not** installed
by this package, because `/usr/bin/ts` belongs to `moreutils`.

Configuration lives in `~/.config/tree-space/` (`config.toml`, `main.css`,
`bookmarks.toml`) and is written on first launch. A commented reference copy of
the default config is at `/usr/share/tree-space/default-config.toml`.

## Making tree-space your default file manager

The package installs the desktop entry and icon but leaves your defaults alone.
To route folders and `file://` links to tree-space:

    xdg-mime default org.tree_space.panel.desktop inode/directory
    xdg-mime default org.tree_space.panel.desktop x-scheme-handler/file

To also answer the `org.freedesktop.FileManager1` D-Bus calls that portals and
Electron apps make (for example VS Code's "Open Containing Folder"), install
the dormant service file into your session bus:

    install -Dm644 /usr/share/tree-space/org.freedesktop.FileManager1.service \
        ~/.local/share/dbus-1/services/org.freedesktop.FileManager1.service

It is deliberately not installed system-wide, so it never shadows Nautilus for
other users. Remove that file and re-run the `xdg-mime` commands with your
previous file manager's desktop file to undo this.

## License

MIT. See `/usr/share/licenses/tree-space/LICENSE`.
