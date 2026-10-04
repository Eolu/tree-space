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

/// Pause and detach a `GtkVideo`'s media stream.
///
/// Detaching alone does not stop a clip: the `GtkMediaStream` (a
/// `GtkMediaFile`) can outlive the widget — a row GTK has removed is freed
/// lazily, and the media cache may still hold it — so it keeps its GStreamer
/// pipeline, audio included, running even after the thumbnail is gone. Pausing
/// first actually stops playback; detaching then releases the widget's
/// reference.
pub(crate) fn stop_video_stream(video: &relm4::gtk::Video) {
    use relm4::gtk::prelude::*;
    if let Some(stream) = video.media_stream() {
        stream.set_playing(false);
    }
    video.set_media_stream(None::<&relm4::gtk::MediaStream>);
}