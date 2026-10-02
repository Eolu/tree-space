//! GTK/relm4 layer: the panel window, toolbar and file tree.
//!
//! Layout of concerns:
//!
//! * [`app`] — the root [`relm4::SimpleComponent`]: owns the layer-shell
//!   window, the toolbar and tree subcomponents, the status bar and session
//!   state persistence.
//! * [`bookmarks`] — the bookmarks strip docked above/below the panes, plus its
//!   editor dialog.
//! * [`toolbar`] — the "open folder" button and the editable path entry.
//! * [`tree`] — the file tree itself: rendering, keyboard navigation, the
//!   context menu, inline rename, clipboard and trash.
//! * [`props`] — the Properties dialog (metadata and a permissions editor).

pub mod app;
pub mod bookmarks;
pub mod props;
pub mod toolbar;
pub mod tree;

/// The `wlr-layer-shell` namespace every tree-space surface advertises, so
/// compositor rules can target the panel and its dialogs by name instead of the
/// built-in `gtk4-layer-shell` default.
pub(crate) const LAYER_NAMESPACE: &str = "tree-space";