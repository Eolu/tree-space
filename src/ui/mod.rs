//! GTK/relm4 layer: the panel window, toolbar and file tree.
//!
//! Layout of concerns:
//!
//! * [`app`] — the root [`relm4::SimpleComponent`]: owns the layer-shell
//!   window, the toolbar and tree subcomponents, the status bar and session
//!   state persistence.
//! * [`toolbar`] — the "open folder" button and the editable path entry.
//! * [`tree`] — the file tree itself: rendering, keyboard navigation, the
//!   context menu, inline rename, clipboard and trash.
//! * [`props`] — the Properties dialog (metadata and a permissions editor).

pub mod app;
pub mod props;
pub mod toolbar;
pub mod tree;