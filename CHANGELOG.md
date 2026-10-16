# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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
