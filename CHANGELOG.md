# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Panels can be undocked into ordinary windows. `[panel] mode = "window"` starts
  that way (the default stays `"docked"`), and `Ctrl+Shift+f` — the
  `"Toggle Window Mode"` pane action, rebindable in `[pane_menu]` — toggles the
  focused panel at any time. An undocked panel keeps its panes and docks again
  the same way.
- Dragging a pane by its toolbar grip chooses where it lands: an outer band of a
  screen edge docks it there, the middle of the screen undocks it into its own
  window, and its current edge leaves it alone. A label follows the pointer and
  names the outcome ("Dock left panel", "Undock", "No change", "Cancel"), the
  target panel is outlined, and the dragged pane is outlined when it would
  float.

### Changed

- Permanent delete (Shift+Delete) runs in the background instead of blocking
  the main loop for the whole tree. The rows being removed are struck through
  and dimmed as soon as the confirmation is accepted, the panel stays usable
  while the files disappear, and each row is removed when its delete finishes.
  - Safety: every target is identified (device and inode) at confirmation time
    and the worker refuses to delete a path that no longer names that entry, so
    a file re-created at the same name is never destroyed; a symlink is removed
    rather than its target, and the parent is canonicalized so `..` cannot
    redirect the delete.
  - While a path is being deleted, operations that would read, write or
    transform it are refused, in every pane: paste, drop, new file/folder,
    rename, cut/copy, duplicate, trash and opening the directory. Anything
    dropped into a directory that finishes deleting a moment later would be lost
    otherwise.
  - File-watch events from a tree that is being deleted are ignored (the worker
    reports completion), so deleting a very large expanded directory cannot
    flood the main loop.
- Memory use is substantially lower:
  - The panel asks GTK for the software (cairo) renderer unless `GSK_RENDERER`
    is already set in the environment. On NVIDIA/hybrid systems the default
    Vulkan renderer loads the vendor Vulkan drivers plus `libLLVM`, adding tens
    of MB of resident memory to an always-running panel for a UI that is almost
    entirely text; cairo skips all of it. `GSK_RENDERER=gl ts` (or any other
    value) opts back into GPU rendering.
  - Inline image and GIF thumbnails are decoded at display size rather than the
    image's native size, so a folder of large photos no longer caches
    full-resolution textures (a 4000×3000 JPEG cost ~35 MB as a texture; the
    scaled decode is a fraction of that).
  - The decoded-texture, parsed-document and preview-kind caches are bounded.
  - The tree is virtualised (`GtkListView` with a recycled row factory):
    widgets are built only for the rows on screen. Materialising a GTK widget
    per row costs roughly 20 KB, so a directory with tens of thousands of
    entries — a downloads folder, a `node_modules` — previously spent hundreds
    of MB on rows that were not even visible. A 20 000-entry directory went from
    ~600 MB to ~100 MB resident. Sorting, filtering and lazy expansion are
    unaffected.
- `install.sh` installs the binaries with `cargo install` (into cargo's bin
  directory) instead of copying them under the prefix. The copy it made sat
  earlier on `PATH` than `~/.cargo/bin`, so it shadowed `cargo install` and went
  stale — a panel launched from it kept older behaviour.

### Fixed

- The path entry's completion list starts with the typed directory itself when
  it exists, so typing `/` and pressing Enter opens the root instead of the
  first subdirectory (`/bin`). The typed directory is the row the highlight
  starts on, and typing a complete path now opens it rather than its first
  name match.
- Names sort naturally instead of character by character: runs of digits are
  compared by value, so `chapter9` comes before `chapter10` and `image2` before
  `image10`. Text still compares case-insensitively, and the same order breaks
  ties when sorting by size, modification time or type.
- The context menu dismisses when an item is chosen (it could be left open after
  acting on it), and no longer flashes as it opens.
- The panel no longer panics — aborting the process — when a pane was closed or
  moved while a preview-sync callback was still queued.
- The keyboard cursor stays on the row it names when the row set changes under
  it (a filesystem-watcher event inserting or removing a row above it, a sort
  change, a filter, a directory collapsing). It is an index into the visible
  rows, so it used to drift onto a neighbour while the highlight — kept by path
  — stayed put, which made shortcuts act on the wrong row: `Ctrl+t` on a
  highlighted directory could toggle thumbnails for a different directory (or
  for a file), so its own previews never appeared. The context menu was
  unaffected because it targets the clicked row explicitly.
- The panel gives the keyboard back as soon as the pointer leaves it, including
  when a context menu closes past the panel edge. The claim could previously
  stick, so keys meant for the window you switched to kept going to the panel (a
  `Super+V` paste helper, for example) until the panel was hidden and shown.
- Pasting files copied in another app works when the clipboard offers only
  `text/uri-list`: the panel detected that format but then read the clipboard as
  `text/plain`, so it reported "Clipboard has no files to paste" even with files
  on the clipboard. That message also clears itself after a few seconds instead
  of lingering until the next folder change.
- An in-tree drag only relocates rows onto a **folder** row. Dropping them on a
  file row, or on the blank space below the list, no longer falls back to the
  tree root — which silently moved files out of their directory when a click
  drifted into a drag. Files dragged in from another application still land in
  the open folder.

## [0.2.0] - 2026-10-06

Packaging and distribution release.

### Added

- A second binary, `tree-space`, identical to `ts`. Arch packaging installs
  `/usr/bin/tree-space` because `/usr/bin/ts` is owned by `moreutils`; `cargo
  install tree-space` provides both names.
- `--version` / `-V` flag.
- Application icon (`resources/icons/hicolor/scalable/apps/org.tree_space.panel.svg`).
- `install.sh` and `uninstall.sh` for user-level installs (build, binary,
  desktop entry, icon, MIME handler, optional `FileManager1` service).
- AUR packaging (`packaging/aur`) for `tree-space` and `tree-space-bin`.
- `LICENSE` (MIT) file.

### Changed

- The desktop entry is now `org.tree_space.panel.desktop`, matching the
  `GApplication` id, and its `Exec` uses `tree-space`.
- `Cargo.toml` gained `rust-version`, `authors`, `homepage`, `keywords` and
  `categories`; the screenshots under `docs/` are excluded from the published
  crate.

## [0.1.9] - 2026-10-04

Early public releases (0.1.0 through 0.1.9), published to crates.io; see the
git history for the full set of changes.
