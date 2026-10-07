# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
