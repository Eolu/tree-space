//! Non-visual filesystem primitives.
//!
//! * [`model`] — a lazy tree model that never touches GTK and accepts directory
//!   listings through the [`model::DirSource`] seam so it can be tested against
//!   an in-memory filesystem.
//! * [`ops`] — file operations (create/rename/copy/move/duplicate/trash)
//!   behind a small abstraction so trashing can be faked in tests.
//! * [`meta`] — metadata for the Properties dialog: a `stat(2)` snapshot,
//!   permission math, owner/group resolution and image dimensions.
//! * [`watcher`] — a `notify` (inotify) wrapper that translates raw
//!   filesystem events into the model's [`Change`] vocabulary.
//!
//! Nothing in this module requires a graphical environment; it is exercised by
//! `cargo test` on CI and developer machines alike.

pub mod meta;
pub mod model;
pub mod ops;
pub mod watcher;

pub use model::{
    Change, DirNode, DirSource, EntryInfo, SortKey, SortOptions, TreeModel, VisibleRow,
};