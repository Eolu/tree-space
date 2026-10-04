//! tree-space — a dockable, keyboard-first file manager panel for Wayland/Hyprland.
//!
//! The crate is split into three layers:
//!
//! * [`config`] — TOML configuration and XDG state persistence. Pure serde code,
//!   fully unit-tested, no GTK imports.
//! * [`fs`] — the non-visual file-management core:
//!   * [`fs::model`] — a lazy, filesystem-agnostic tree model (expand/collapse,
//!     sorting, filtering, incremental updates from change events). Directory
//!     listings are read through the [`fs::model::DirSource`] trait so tests can
//!     substitute an in-memory filesystem.
//!   * [`fs::ops`] — create/rename/copy/move/duplicate/trash operations behind
//!     the [`fs::ops::TrashService`] abstraction (GIO trash in production, a
//!     recording fake in tests).
//!   * [`fs::watcher`] — `notify` inotify wrapper that maps raw filesystem
//!     events onto the model's [`fs::Change`] vocabulary.
//! * [`ui`] — relm4/GTK4 components. The UI layer is intentionally thin: it
//!   renders [`fs::model::TreeModel`] and forwards user intents back into it.
//!
//! Keeping the model free of GTK imports means the interesting logic can be
//! tested headlessly with `cargo test`.

pub mod config;
pub mod theme;
pub mod highlight;
pub mod fs;
pub mod cmd;
pub mod freedesktop;
pub mod ipc;
pub mod workspace;
pub mod audio;
pub mod preview;
pub mod ui;
