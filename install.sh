install -Dm644 resources/tree-space.desktop ~/.local/share/applications/tree-space.desktop
install -Dm644 resources/org.freedesktop.FileManager1.service ~/.local/share/dbus-1/services/org.freedesktop.FileManager1.service
xdg-mime default tree-space.desktop inode/directory
xdg-mime default tree-space.desktop x-scheme-handler/file
update-desktop-database ~/.local/share/applications
