//! The file tree: rendering, keyboard navigation, context menu, inline rename,
//! clipboard and trash.
//!
//! Architecture notes:
//!
//! * Rendering is *stateless*: [`TreeMsg`] mutates the model and `update_view`
//!   rebuilds the whole [`gtk::ListBox`] from [`TreeModel::visible_rows`].
//!   A tree this size does not need widget recycling, and a full rebuild keeps
//!   row state trivially consistent.
//! * Selection lives as paths in the model, not in `ListBox` selection, so it
//!   survives rebuilds as the tree expands and collapses.
//! * Keyboard input is captured *in the capture phase* on the scrolled window,
//!   so arrows work before the user clicks a row. While an inline rename entry
//!   is focused the capture defers entirely to that entry.
//! * Filesystem events are debounced (buffered + flushed once after a quiet
//!   window) so bulk operations do not re-render once per event.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime};

use crate::audio::AudioPlayer;
use crate::config::{
    BuiltinAction, ContextAction, ContextMenu, PanelConfig, PanelSide, ShortcutTarget,
    TreeConfig, action_command,
};
use crate::fs::meta::format_size;
use crate::fs::model::{Change, SortKey, StdDirSource, TreeModel, VisibleRow};
use crate::fs::ops::FileOps;
use crate::fs::watcher::{RecursiveMode, RecommendedWatcher, Watcher, spawn as spawn_watcher};
use crate::highlight::{self, Span, TokenClass};
use crate::preview::{
    self, ArchiveData, DocumentData, DocumentLines, ParseStatus, PreviewKind, TableData,
};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use relm4::gtk::{gdk, gio, glib, pango, prelude::*};
use relm4::prelude::*;

/// Clipboard payload: the operation and the source paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardOp {
    Copy,
    Cut,
}

/// Prefix-based type-ahead state: printable keys typed in quick succession
/// accumulate into a search prefix that jumps the cursor to the next matching
/// row. Reset after [`TYPEAHEAD_TIMEOUT`].
#[derive(Debug, Default)]
struct TypeAhead {
    /// The accumulated prefix (already lowercased).
    prefix: String,
    /// When the last key was typed; a gap longer than the timeout resets the
    /// buffer instead of extending it.
    last: Option<std::time::Instant>,
}

/// How long a type-ahead prefix stays live after the last keystroke.
const TYPEAHEAD_TIMEOUT: Duration = Duration::from_millis(700);
/// Guard against runaway prefixes from a stuck key.
const TYPEAHEAD_MAX: usize = 32;

impl TypeAhead {
    /// Feed `ch`, returning the prefix to search for (empty when the key is not
    /// part of a useful search, e.g. a control character).
    fn push(&mut self, ch: char, now: std::time::Instant) -> Option<String> {
        if !ch.is_alphanumeric() && ch != '_' && ch != '-' && ch != '.' && ch != ' ' {
            return None;
        }
        let expired = self
            .last
            .is_some_and(|last| now.duration_since(last) > TYPEAHEAD_TIMEOUT);
        if expired {
            self.prefix.clear();
        }
        if self.prefix.len() >= TYPEAHEAD_MAX {
            self.prefix.clear();
        }
        self.prefix.push(ch.to_ascii_lowercase());
        self.last = Some(now);
        Some(self.prefix.clone())
    }

    /// Clear the buffer (called when the cursor moves, a rename starts, ...).
    fn clear(&mut self) {
        self.prefix.clear();
        self.last = None;
    }
}

/// The index of the next row (cyclically, *after* the current cursor) whose name
/// starts with `prefix`, or `None` when nothing matches.
fn typeahead_target(rows: &[VisibleRow], prefix: &str, cursor: Option<usize>) -> Option<usize> {
    if prefix.is_empty() || rows.is_empty() {
        return None;
    }
    let len = rows.len();
    let start = cursor.map_or(0, |c| (c + 1) % len);
    for offset in 0..len {
        let index = (start + offset) % len;
        if rows[index].name.to_ascii_lowercase().starts_with(prefix) {
            return Some(index);
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Clipboard {
    op: ClipboardOp,
    paths: Vec<PathBuf>,
}

/// What a drag-and-drop onto `target` should do, decided purely from the paths
/// (no filesystem access), so it can be unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DropPlan {
    /// Nothing to do (empty after filtering, or a no-op drop).
    Ignore,
    /// Perform the copy immediately.
    Copy(Vec<PathBuf>),
    /// Move the selection (immediately or after confirmation, per config).
    Move(Vec<PathBuf>),
    /// Reject with this message (moving a directory into itself).
    Reject(String),
}

/// Decide what dropping `sources` onto `target` does. `sources` are filtered to
/// those that are not the target itself and do not already live in it.
fn resolve_drop(target: &Path, sources: Vec<PathBuf>, copy: bool) -> DropPlan {
    let sources: Vec<PathBuf> = sources
        .into_iter()
        .filter(|src| src != target && src.parent() != Some(target))
        .collect();
    if sources.is_empty() {
        return DropPlan::Ignore;
    }
    if sources.iter().any(|src| target.starts_with(src)) {
        return DropPlan::Reject("Cannot move a folder into itself".to_string());
    }
    if copy { DropPlan::Copy(sources) } else { DropPlan::Move(sources) }
}

/// The inclusive index range between two rows, regardless of order. Pure so the
/// shift-extension logic can be unit-tested without a live widget tree.
fn index_range(a: usize, b: usize) -> std::ops::RangeInclusive<usize> {
    if a <= b { a..=b } else { b..=a }
}

/// The paths a drag started on `pressed` should carry. When `pressed` is part
/// of the current multi-selection, the whole selection moves together (the
/// usual file-manager behavior); otherwise only the pressed row is dragged.
/// Pure so it can be unit-tested.
fn drag_paths(selected: &[PathBuf], pressed: &Path) -> Vec<PathBuf> {
    if selected.len() > 1 && selected.iter().any(|p| p == pressed) {
        selected.to_vec()
    } else {
        vec![pressed.to_path_buf()]
    }
}

/// Serialize `paths` as a `text/uri-list` payload (CRLF-separated `file://`
/// URIs), for the drag content provider. Pure and unit-testable.
fn format_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for path in paths {
        if let Ok(uri) = glib::filename_to_uri(path, None) {
            out.push_str(&uri);
            out.push_str("\r\n");
        }
    }
    out
}

/// The drag content provider for `paths`: a `gdk::FileList` (which GDK
/// serializes to the app-native file formats many GTK apps prefer) unioned
/// with an explicit `text/uri-list` byte payload (which browsers, terminals and
/// non-GTK apps read). Advertising both maximizes compatibility.
fn provider_for_paths(paths: &[PathBuf]) -> gdk::ContentProvider {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    let list = gdk::FileList::from_array(&files);
    let native = gdk::ContentProvider::for_value(&list.to_value());
    let uris = format_uri_list(paths);
    let bytes = glib::Bytes::from(uris.as_bytes());
    let text = gdk::ContentProvider::for_bytes(URI_LIST_MIME, &bytes);
    gdk::ContentProvider::new_union(&[native, text])
}

/// Whether a pointer move from `start` to `now` (both row-local) has grown past
/// the DND threshold, in either axis. Mirrors GTK's own
/// `gtk_drag_check_threshold_double` so a drag starts at the same distance a
/// `DragSource` would use. The threshold comes from the widget's settings
/// (`gtk-dnd-drag-threshold`, 8px by default); a cast guards against a
/// nonsensical negative value.
pub(crate) fn past_drag_threshold(row: &gtk::Widget, start: (f64, f64), now: (f64, f64)) -> bool {
    let threshold = row
        .settings()
        .gtk_dnd_drag_threshold()
        .max(1) as f64;
    (now.0 - start.0).abs() > threshold || (now.1 - start.1).abs() > threshold
}

/// A small drag icon showing how many rows are being dragged. `1` renders as a
/// single-file glyph, more as an "N items" badge.
fn drag_badge(count: usize) -> gtk::Widget {
    let label = gtk::Label::new(Some(&if count <= 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }));
    label.add_css_class("tree-drag-badge");
    label.upcast()
}

/// Where a press landed, so a subsequent pointer move can decide whether the
/// press has grown into a drag. Recorded on every left press and consumed by
/// the scrolled-window motion controller.
#[derive(Debug, Clone)]
struct DragOrigin {
    /// The press point in scrolled-window coordinates.
    x: f64,
    y: f64,
}

/// Start a DND drag of `paths` originating from `widget`, manually.
///
/// GTK's own [`gtk::DragSource`] cannot start a drag from this panel: the
/// window is a wlr-layer-shell surface, and its internal drag gesture is never
/// *recognized* here, so `gdk_drag_begin` is never called (confirmed against
/// `gtkdragsource.c` and a `WAYLAND_DEBUG=1` trace, which showed pointer motion
/// arriving but no `wl_data_device.start_drag`). The layer surface itself is
/// fine — calling [`gdk::Drag::begin`] directly from it makes the compositor
/// accept the drag — so we reproduce the handful of steps
/// `gtk_drag_source_drag_begin` performs instead of relying on its gesture.
///
/// Returns `true` if the drag actually began.
pub(crate) fn begin_row_drag(
    widget: &gtk::Widget,
    paths: &[PathBuf],
    start: (f64, f64),
    actions: gdk::DragAction,
    on_finished: impl Fn(gdk::Drag, bool) + 'static,
    on_cancelled: impl Fn(gdk::Drag, gdk::DragCancelReason) + 'static,
) -> bool {
    let Some(native) = widget.native() else {
        return false;
    };
    let Some(surface) = native.surface() else {
        return false;
    };
    let Some(device) = widget
        .display()
        .default_seat()
        .and_then(|seat| seat.pointer())
    else {
        return false;
    };

    // Pointer offset within the widget, in surface coordinates, so the drag
    // icon lines up under the cursor (mirrors GTK's own `dx`/`dy` math).
    let (dx, dy) = match (
        surface.device_position(&device),
        widget.compute_point(
            &native,
            &relm4::gtk::graphene::Point::new(start.0 as f32, start.1 as f32),
        ),
    ) {
        (Some((px, py, _)), Some(p)) => ((px - p.x() as f64).round(), (py - p.y() as f64).round()),
        _ => (0.0, 0.0),
    };

    let content = provider_for_paths(paths);
    let Some(drag) = gdk::Drag::begin(&surface, &device, &content, actions, dx, dy) else {
        return false;
    };

    let icon = gtk::DragIcon::for_drag(&drag);
    icon.set_child(Some(&drag_badge(paths.len())));

    {
        let on_finished = Rc::new(on_finished);
        let drag = drag.clone();
        drag.connect_dnd_finished(move |drag| {
            let delete = drag.selected_action() == gdk::DragAction::MOVE;
            on_finished(drag.clone(), delete);
        });
    }
    {
        let on_cancelled = Rc::new(on_cancelled);
        let drag = drag.clone();
        drag.connect_cancel(move |drag, reason| on_cancelled(drag.clone(), reason));
    }
    true
}

/// Everything the tree needs that only the app knows.
pub struct TreeInit {
    /// Per-tree visual configuration.
    pub config: TreeConfig,
    /// The parent window, used to anchor dialogs.
    pub parent: gtk::Window,
    /// Configurable per-row context-menu rules.
    pub menu: ContextMenu,
    /// Which side of the screen this pane's dock is on (drives the
    /// "In {opposite} panel" menu label).
    pub side: PanelSide,
    /// Panel geometry, used to size inline thumbnails to the column width.
    pub panel: PanelConfig,
}

/// User intent for the tree component.
#[derive(Debug, Clone)]
pub enum TreeMsg {
    OpenRoot(PathBuf),
    /// Reveal `path` in the already-open tree: move the cursor to it, select it,
    /// and scroll it into view. The path must be a visible row (a child of the
    /// current root); when it is not visible nothing changes.
    SelectPath(PathBuf),
    /// Move keyboard focus into the tree, so a freshly shown or freshly split
    /// pane receives shortcuts immediately instead of waiting for a click.
    Focus,
    SetFilter(String),
    /// The panel geometry changed (interactive width resize): re-measure the
    /// column so inline thumbnails follow the new width.
    SetPanel(PanelConfig),
    /// The pane moved to a dock on the other side: update the side used for
    /// context-menu labels ("In {left|right} panel").
    SetSide(PanelSide),

    MoveUp,
    MoveDown,
    /// Shift+Up / Shift+Down: move the cursor `delta` rows and extend the
    /// selection from the anchor to the new cursor. With no anchor yet, the
    /// current cursor becomes the anchor.
    ExtendSelection(isize),
    /// Ctrl+Shift+Up / Ctrl+Shift+Down: move the cursor `delta` rows without
    /// collapsing the selection, extending it to include the new cursor while
    /// keeping the existing anchor. Falls back to [`Self::ExtendSelection`] when
    /// there is no anchor.
    MoveExtendingSelection(isize),
    CollapseCursor,
    ExpandCursor,
    ActivateCursor,
    Rename,

    Activate(PathBuf),
    Toggle(PathBuf),
    Select(PathBuf),
    /// Ctrl+click: add/remove `path` from the selection without disturbing
    /// the rest of it.
    ToggleSelect(PathBuf),
    /// Shift+click: select every row between the last selection anchor and
    /// `path`, inclusive.
    RangeSelect(PathBuf),
    /// A plain left-click on a row: select it and, for a directory, toggle it
    /// immediately on every press. For files, a second quick click opens the
    /// file (double-click detection happens in the handler, since the click
    /// gesture is rebuilt with the rows).
    RowPress { path: PathBuf, is_dir: bool, ctrl: bool, shift: bool },
    /// A left button released on a row. Used only to finish a click whose
    /// selection collapse was deferred at press time (see
    /// [`Self::RowPress`] and the tree's `pending_click`): if no drag started,
    /// the deferred press is applied now.
    RowRelease { path: PathBuf },
    /// A row drag has begun (`DragSource::drag-begin`). Marks the in-flight
    /// drag so a click's release does not treat it as a plain click.
    DragStarted,
    /// A drag that left the app finished as a MOVE: the receiving application
    /// took the data, so the sources are moved to the trash. (An in-tree move
    /// is performed by the drop target instead and never reaches this.)
    DroppedAsMove(Vec<PathBuf>),
    /// Left-click on the blank area below the rows: clear the selection and the
    /// keyboard cursor, so hotkeys act on the open directory rather than a row.
    Deselect,
    Menu(PathBuf),
    /// Right-click on the blank area below the rows: open the context menu for
    /// the currently open directory (the tree root), anchored at the click.
    MenuAt { x: f64, y: f64 },
    RenameAt(PathBuf),
    /// Trash a single, explicit path (used by the context menu, which always
    /// targets the row it was opened on regardless of the current selection).
    Trash(PathBuf),
    /// Trash the current selection (used by the Delete key).
    DeleteSelected,
    ConfirmTrash(Vec<PathBuf>),
    /// Permanently delete a single explicit path (context menu target).
    PermanentDelete(PathBuf),
    /// Permanently delete the current selection (Shift+Delete).
    PermanentDeleteSelected,
    /// Actually perform the permanent delete after the user confirmed the dialog.
    ConfirmPermanentDelete(Vec<PathBuf>),
    /// Run a configured custom command against `path` (via `{path}`/`{dir}`
    /// substitution in the config template).
    RunCommand { command: String, path: PathBuf },
    /// Open the row's path as a brand-new split pane (directories only).
    OpenSplit(PathBuf),
    /// Open the directory as a pane in the *opposite* side's dock.
    OpenInOppositePanel(PathBuf),
    /// Pick an application for the row and open it with that app.
    OpenWith(PathBuf),
    /// Open the file with its default application.
    OpenWithDefault(PathBuf),
    /// Create a symlink to the row next to it.
    CreateLink(PathBuf),
    /// Show the row's properties dialog (metadata and a permissions editor).
    Properties(PathBuf),
    /// Show a summary properties dialog for a multi-row selection (count, total
    /// size, and the individual paths).
    PropertiesSelected(Vec<PathBuf>),
    /// Copy every path in the list to the system clipboard as text, one per
    /// line (multi-select form of [`Self::CopyPath`]).
    CopyPaths(Vec<PathBuf>),
    /// Copy every path relative to the open directory, one per line
    /// (multi-select form of [`Self::CopyRelativePath`]).
    CopyRelativePaths(Vec<PathBuf>),
    /// Run a configured custom command once per path, in visual top-to-bottom
    /// order (multi-select form of [`Self::RunCommand`]).
    RunCommandEach { command: String, paths: Vec<PathBuf> },
    /// Rename `path` to the bare file name `name` (used by the Properties
    /// dialog's editable Name field).
    RenameTo { path: PathBuf, name: String },
    /// Toggle the inline thumbnail preview for the row (an image, or every
    /// image inside a directory).
    ToggleThumbnail(PathBuf),
    /// Clicked an inline thumbnail: play/pause a video, or animate a GIF.
    ToggleThumbnailPlay(PathBuf),
    /// A configured shortcut fired against the row under the keyboard cursor.
    RunShortcut { target: ShortcutTarget },
    /// Run a configured shortcut by accelerator string (e.g. `Ctrl+c`, `Down`),
    /// as if the key were pressed. Used by the IPC `--key` command so external
    /// button decks can drive the tree without it holding keyboard focus.
    RunAccelerator(String),
    Duplicate(PathBuf),
    /// Ctrl+D: duplicate whatever is at the keyboard cursor.
    RequestDuplicateCursor,
    /// Ctrl+A: select every visible row.
    SelectAllRows,
    /// A printable key was typed in the tree: type-ahead jumps the cursor to
    /// the next row whose name starts with the accumulated prefix.
    TypeAhead(char),
    CopyPath(PathBuf),
    CopyRelativePath(PathBuf),
    /// Add the directory row to the app's bookmarks list.
    AddBookmark(PathBuf),

    RenameCommit,
    RenameCancel,

    Copy,
    Cut,
    Paste,
    /// Paths decoded from the system clipboard, delivered asynchronously by
    /// [`read_clipboard_files`] after a Paste with no internal clipboard.
    PastePaths(Vec<PathBuf>),
    NewFile,
    NewFolder,

    /// Toggle dotfile visibility in the current tree.
    ToggleHidden,
    /// Set the primary sort key (and keep the current direction).
    SetSortKey(SortKey),
    /// Flip ascending/descending order.
    ToggleSortDirection,

    FsChange(Change),
    FlushChanges,
    RequestOpenFolder,

    /// A file was dropped from outside the tree (or dragged from elsewhere in
    /// it) onto directory `target`. Copies happen immediately; moves are
    /// confirmed first (see [`TreeMsg::DropIntoConfirmed`]). `copy` is set by
    /// holding Ctrl during the drop.
    DropInto { target: PathBuf, sources: Vec<PathBuf>, copy: bool },
    /// A drop landed on empty space below the rows (or a non-directory row);
    /// resolved to the current tree root.
    DropIntoRoot { sources: Vec<PathBuf>, copy: bool },
    /// The user confirmed a drag-to-move; perform it.
    DropIntoConfirmed { target: PathBuf, sources: Vec<PathBuf> },
    /// Pause every playing preview, keeping the widgets, so a tree that is
    /// hidden (bookmarks view, panel hidden, workspace switched away) stops
    /// making noise. Unlike [`Self::Shutdown`], the previews survive and are
    /// still usable when the tree is shown again.
    Suspend,
    /// Drop every thumbnail and stop all media playback ahead of app exit.
    Shutdown,
    /// Do nothing. Produced for actions that are dispatched elsewhere (the
    /// bookmark-only builtins never run against a tree row).
    Noop,
}

/// Notable events the tree reports upwards.
#[derive(Debug)]
pub enum TreeOutput {
    /// A transient message for the status bar.
    Status(String),
    /// The tree's root changed.
    RootChanged(PathBuf),
    /// The user asked to browse for a new root folder.
    OpenFolderRequested,
    /// The user asked to open `path` as a new split pane.
    OpenSplit(PathBuf),
    /// The user asked to open `path` in the dock on the opposite side.
    OpenOpposite(PathBuf),
    /// A pane-level builtin (`Split View`, `Open Folder...`, `Collapse`, ...) fired
    /// from a keyboard shortcut. The tree cannot perform it, so it forwards the
    /// action upward for the app to handle.
    PaneAction(BuiltinAction),
    /// The user asked to bookmark `path` (a directory).
    AddBookmark(PathBuf),
}

/// Rendered state of the tree component.
pub struct Tree {
    tree: Option<TreeModel>,
    ops: FileOps,
    config: TreeConfig,
    parent: gtk::Window,
    menu: ContextMenu,
    side: PanelSide,

    /// Paths the user turned "View Thumbnail" on: a file previews only itself,
    /// a directory previews every previewable file beneath it. Non-persistent —
    /// cleared when a directory collapses or the root changes.
    thumbnails: HashSet<PathBuf>,
    /// Detected preview kind per path, so the content sniffing that classifies
    /// each file runs once rather than on every rebuild. `None` caches a file
    /// that has no preview.
    preview_kinds: HashMap<PathBuf, Option<PreviewKind>>,
    /// Parsed inline documents (text, tables, structured data, archives) keyed
    /// by path, so a rebuild never re-reads or re-parses. `None` caches a file
    /// that failed to load.
    documents: HashMap<PathBuf, Option<DocumentData>>,
    /// Decoded textures for the active thumbnails, so rebuilds (which run on
    /// every selection change) don't re-decode images.
    thumb_cache: HashMap<PathBuf, gdk::Texture>,
    /// Decoded media streams for video thumbnails (kept across rebuilds so a
    /// playing clip isn't restarted by a redraw).
    media: HashMap<PathBuf, gtk::MediaFile>,
    /// The live `GtkVideo` for each video thumbnail. Held so teardown can
    /// detach the stream from the widget: a removed row can stay alive until GTK
    /// re-focuses, and a widget still holding a stream would keep playing.
    /// Repopulated on each render.
    video_widgets: HashMap<PathBuf, gtk::Video>,
    /// Audio players (GStreamer `playbin`, not GtkMediaFile — see `crate::audio`),
    /// kept across rebuilds so playback continues through a redraw.
    audio: HashMap<PathBuf, Rc<AudioPlayer>>,
    /// Paths whose thumbnail is currently playing (video or animated GIF).
    playing: HashSet<PathBuf>,
    /// GIF frame iterators currently animating, advanced by `gif_tick`.
    gif_anims: Rc<RefCell<HashMap<PathBuf, GifAnim>>>,
    /// The live `GtkPicture` for each GIF thumbnail, so `gif_tick` can advance
    /// it without rebuilding every row. Repopulated on each render.
    gif_widgets: Rc<RefCell<HashMap<PathBuf, gtk::Picture>>>,
    /// The GIF animation ticker; present while any GIF is playing.
    gif_tick: Rc<RefCell<Option<glib::SourceId>>>,
    /// The live controls for each shown audio player, so `audio_tick` can keep
    /// its seek bar, clock and play/pause icon in sync with the stream.
    /// Repopulated on each render.
    audio_widgets: Rc<RefCell<HashMap<PathBuf, AudioWidgets>>>,
    /// The audio player ticker; present while any audio player is shown.
    audio_tick: Rc<RefCell<Option<glib::SourceId>>>,
    /// Width in px available to a depth-0 row, used to size thumbnails.
    content_width: i32,

    rows: Vec<VisibleRow>,
    cursor: Option<usize>,
    selected: Vec<PathBuf>,
    /// Base row for the next Shift+click range (last plain- or Ctrl-clicked
    /// row). `None` means "no anchor yet"; a Shift+click with no anchor
    /// behaves like a plain click.
    anchor: Option<PathBuf>,
    /// Path and time of the last plain row press. Row widgets (and their click
    /// gestures) are rebuilt on every message, so double-click detection can't
    /// live in the gesture; this survives across rebuilds.
    last_press: Option<(PathBuf, std::time::Instant)>,

    renaming: Option<PathBuf>,
    rename_entry: Option<gtk::Entry>,
    clipboard: Option<Clipboard>,

    pending: Vec<Change>,
    flush_source: Option<glib::SourceId>,
    _watcher: Option<RecommendedWatcher>,
    /// Directories currently registered with `_watcher` (kept in sync with the
    /// model's expanded+loaded set, so the watch joins are lazy and cheap).
    watched: HashSet<PathBuf>,

    /// Popover for the row context menu, parented to the row built by the most
    /// recent rebuild.
    popover: Option<gtk::Popover>,
    /// Set by a context-menu request; consumed by the next rebuild, which
    /// anchors the popover to the freshly-built row (rebuilds destroy rows, so
    /// the popover must be attached afterwards).
    menu_target: Option<PathBuf>,
    /// When a menu is requested for the open directory (which has no row of its
    /// own), the click point to anchor the popover at, in `scrolled` coords.
    menu_point: Option<(f64, f64)>,

    /// Shared with the key-capture closure so it can defer to the rename entry.
    renaming_state: Rc<Cell<bool>>,

    /// Set by a message that only changed the selection/cursor (no structural
    /// change). `update_with_view` then updates the `tree-row-selected` class on
    /// the existing rows instead of rebuilding every widget — the single
    /// biggest interactive win on large directories.
    selection_dirty: bool,
    /// Type-ahead buffer: printable keys typed in quick succession jump the
    /// cursor to the next row whose name starts with the accumulated prefix.
    typeahead: TypeAhead,
    /// Row index to bring into view on the next widget pass (set by
    /// [`TreeMsg::SelectPath`], which has no widget access of its own).
    scroll_to: Option<usize>,
    /// A focus request arrived before any row existed (a freshly-opened
    /// directory may still be loading); take the keyboard once the first row is
    /// built.
    focus_pending: bool,

    /// The paths a drag started from a row should carry. Updated on every row
    /// press (from the selection at that moment) and read by each row's
    /// `DragSource::prepare`, so a drag of a multi-selection keeps the whole
    /// set even though the click that starts it may collapse the selection.
    drag_payload: Rc<RefCell<Vec<PathBuf>>>,
    /// True from `drag-begin` until `drag-end`. Lets a click's `released`
    /// handler tell a click apart from a drag, so selecting-on-click can be
    /// deferred until we know a drag did not start.
    drag_active: Rc<Cell<bool>>,
    /// Set on a plain press onto a row that is already part of a multi-row
    /// selection; the selection collapse is deferred to `RowRelease` so a drag
    /// can still carry the whole selection.
    pending_click: Option<PathBuf>,
    /// Set by a drop handler that accepted an in-tree drag, so `drag-end` knows
    /// the move/copy was already performed here and must not also act on the
    /// source (which GDK's `delete_data` would suggest for a MOVE).
    drag_landed_internal: Rc<Cell<bool>>,
    /// The press that may grow into a drag (see [`begin_row_drag`]). Recorded on
    /// every left press; cleared on release, on drag start, and when a fresh
    /// press supersedes it.
    drag_origin: Rc<RefCell<Option<DragOrigin>>>,
    /// Whether the primary button is currently held anywhere in the tree.
    /// Rows are rebuilt whenever the selection changes (which a press does), so
    /// this cannot live on the row widget: the motion controller on the *new*
    /// row must still see that a press is in flight.
    drag_button_down: Rc<Cell<bool>>,
}

#[relm4::component(pub)]
impl Component for Tree {
    type Init = TreeInit;
    type Input = TreeMsg;
    type Output = TreeOutput;
    type CommandOutput = ();

    view! {
        scrolled = gtk::ScrolledWindow {
            set_vexpand: true,
            set_hexpand: true,
            set_min_content_width: 160,
            add_css_class: "tree-scroll",

            #[name = "list"]
            gtk::ListBox {
                set_selection_mode: gtk::SelectionMode::None,
                set_activate_on_single_click: false,
            }
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let renaming_state = Rc::new(Cell::new(false));
        let keys = compile_shortcuts(&init.menu);
        let widgets = view_output!();

        let controller = gtk::EventControllerKey::new();
        controller.set_propagation_phase(gtk::PropagationPhase::Capture);
        let capture_sender = sender.clone();
        let capture_state = renaming_state.clone();
        let capture_keys = keys.clone();
        let capture_focus = widgets.scrolled.clone();
        controller.connect_key_pressed(move |_ctrl, key, _keycode, state| {
            // A focused inline text preview owns its keys: selection, copy and
            // cursor movement must reach the `GtkTextView`, not the tree's
            // shortcuts (Ctrl+C, Ctrl+A, arrows...).
            let preview_focused = capture_focus
                .root()
                .and_then(|root| gtk::prelude::RootExt::focus(&root))
                .is_some_and(|focus| focus.has_css_class("tree-preview-text"));
            handle_key(
                key,
                state,
                capture_state.get(),
                preview_focused,
                &capture_keys,
                &capture_sender,
            )
        });
        widgets.scrolled.add_controller(controller);

        // Drops that land on empty space below the rows (or bubble up from a
        // non-directory row, which has no drop target of its own) resolve to
        // the tree's current root. Attached once here — unlike the per-row
        // drop targets rebuilt with their rows, `list` itself persists across
        // rebuilds, so adding this in `rebuild` would pile up duplicates.
        let root_drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::MOVE | gdk::DragAction::COPY);
        let root_drop_sender = sender.clone();
        root_drop.connect_drop(move |target, value, _x, _y| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let sources: Vec<PathBuf> = list.files().iter().filter_map(gio::File::path).collect();
            if sources.is_empty() {
                return false;
            }
            let copy = target.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
            root_drop_sender.input(TreeMsg::DropIntoRoot { sources, copy });
            true
        });
        widgets.list.add_controller(root_drop);

        // The blank area below the rows: left-click clears the selection (so
        // hotkeys act on the open directory), right-click opens that
        // directory's context menu. Attached to the scrolled window (which
        // receives the click even when it falls outside the row list); clicks
        // that land on a row are left to the row's own handler.
        let blank_scrolled = widgets.scrolled.clone();
        let blank_click = gtk::GestureClick::new();
        blank_click.set_button(0);
        blank_click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let blank_sender = sender.clone();
        blank_click.connect_pressed(move |gesture, _n_press, x, y| {
            // Super+right is the panel-resize drag: release the sequence so the
            // window's drag gesture can claim it instead of opening the menu.
            if gesture.current_button() == 3
                && gesture.current_event_state().contains(gdk::ModifierType::SUPER_MASK)
            {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            let on_row = blank_scrolled
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|widget| in_list_row(&widget));
            if on_row {
                return;
            }
            match gesture.current_button() {
                1 => blank_sender.input(TreeMsg::Deselect),
                3 => blank_sender.input(TreeMsg::MenuAt { x, y }),
                _ => {}
            }
        });
        widgets.scrolled.add_controller(blank_click);

        let model = Tree {
            tree: None,
            ops: FileOps::new(),
            config: init.config,
            parent: init.parent,
            menu: init.menu,
            side: init.side,
            thumbnails: HashSet::new(),
            preview_kinds: HashMap::new(),
            documents: HashMap::new(),
            thumb_cache: HashMap::new(),
            media: HashMap::new(),
            video_widgets: HashMap::new(),
            audio: HashMap::new(),
            playing: HashSet::new(),
            gif_anims: Rc::new(RefCell::new(HashMap::new())),
            gif_widgets: Rc::new(RefCell::new(HashMap::new())),
            gif_tick: Rc::new(RefCell::new(None)),
            audio_widgets: Rc::new(RefCell::new(HashMap::new())),
            audio_tick: Rc::new(RefCell::new(None)),
            content_width: thumbnail_content_width(init.panel),
            rows: Vec::new(),
            cursor: None,
            selected: Vec::new(),
            anchor: None,
            last_press: None,
            renaming: None,
            rename_entry: None,
            clipboard: None,
            pending: Vec::new(),
            flush_source: None,
            _watcher: None,
            watched: HashSet::new(),
            popover: None,
            menu_target: None,
            menu_point: None,
            renaming_state,
            selection_dirty: false,
            typeahead: TypeAhead::default(),
            scroll_to: None,
            focus_pending: false,
            drag_payload: Rc::new(RefCell::new(Vec::new())),
            drag_active: Rc::new(Cell::new(false)),
            pending_click: None,
            drag_landed_internal: Rc::new(Cell::new(false)),
            drag_origin: Rc::new(RefCell::new(None)),
            drag_button_down: Rc::new(Cell::new(false)),
        };

        // Drag-out. GTK's `DragSource` never starts here (its gesture is not
        // recognized on a layer-shell surface), so the drag is detected and
        // begun by hand. Everything lives on the scrolled window via a legacy
        // event controller: a press starts a potential drag, a move past the
        // DND threshold with the button still held issues `gdk_drag_begin`, and
        // the release ends it. This cannot use the row's click gesture or a
        // per-row controller, because a press rebuilds the rows — destroying any
        // row-attached controller before the release arrives (which previously
        // left the button "stuck down" and started a drag on the next move).
        {
            let legacy = gtk::EventControllerLegacy::new();
            legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
            let origin = model.drag_origin.clone();
            let payload = model.drag_payload.clone();
            let active = model.drag_active.clone();
            let landed = model.drag_landed_internal.clone();
            let down = model.drag_button_down.clone();
            let drag_root = widgets.scrolled.clone();
            let s = sender.clone();
            legacy.connect_event(move |_controller, event| {
                match event.event_type() {
                    gdk::EventType::ButtonPress => {
                        let button = event
                            .downcast_ref::<gdk::ButtonEvent>()
                            .map(|b| b.button())
                            .unwrap_or(0);
                        if button == 1
                            && let Some((x, y)) = event.position()
                        {
                            // A press on the selectable text preview is not a
                            // file drag: arming one here would turn a drag to
                            // select text into a DND drag. Leave it to the view.
                            let on_text = drag_root
                                .pick(x, y, gtk::PickFlags::DEFAULT)
                                .is_some_and(|widget| in_text_preview(&widget));
                            if !on_text {
                                down.set(true);
                                *origin.borrow_mut() = Some(DragOrigin { x, y });
                            }
                        }
                    }
                    gdk::EventType::ButtonRelease => {
                        let button = event
                            .downcast_ref::<gdk::ButtonEvent>()
                            .map(|b| b.button())
                            .unwrap_or(0);
                        if button == 1 {
                            down.set(false);
                            if !active.get() {
                                *origin.borrow_mut() = None;
                            }
                        }
                    }
                    gdk::EventType::MotionNotify => {
                        if active.get() || !down.get() {
                            return glib::Propagation::Proceed;
                        }
                        let Some((x, y)) = event.position() else {
                            return glib::Propagation::Proceed;
                        };
                        let start = {
                            let guard = origin.borrow();
                            match guard.as_ref() {
                                Some(o) => (o.x, o.y),
                                None => return glib::Propagation::Proceed,
                            }
                        };
                        if !past_drag_threshold(drag_root.upcast_ref(), start, (x, y)) {
                            return glib::Propagation::Proceed;
                        }
                        let paths = payload.borrow().clone();
                        if paths.is_empty() {
                            return glib::Propagation::Proceed;
                        }
                        // One press starts at most one drag.
                        *origin.borrow_mut() = None;
                        let finished = {
                            let active = active.clone();
                            let landed = landed.clone();
                            let s = s.clone();
                            let payload = payload.clone();
                            move |_drag: gdk::Drag, delete: bool| {
                                active.set(false);
                                // The receiver took the data as a MOVE and no
                                // in-tree drop handler ran: trash the sources.
                                if delete && !landed.replace(false) {
                                    let paths = payload.borrow().clone();
                                    if !paths.is_empty() {
                                        s.input(TreeMsg::DroppedAsMove(paths));
                                    }
                                }
                            }
                        };
                        let cancelled = {
                            let active = active.clone();
                            let landed = landed.clone();
                            move |_drag: gdk::Drag, _reason: gdk::DragCancelReason| {
                                active.set(false);
                                landed.set(false);
                            }
                        };
                        if begin_row_drag(
                            drag_root.upcast_ref(),
                            &paths,
                            start,
                            gdk::DragAction::MOVE | gdk::DragAction::COPY,
                            finished,
                            cancelled,
                        ) {
                            active.set(true);
                            s.input(TreeMsg::DragStarted);
                        }
                    }
                    _ => {}
                }
                glib::Propagation::Proceed
            });
            widgets.scrolled.add_controller(legacy);
        }

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        message: Self::Input,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        let _ = root;
        // Focus is a pure widget-side request: grab it and stop, without
        // touching the model or rebuilding the rows.
        if let TreeMsg::Focus = message {
            // The rows are the focusable widgets (the list/scrolled are not),
            // so focus the row at the keyboard cursor — falling back to the
            // first row, then the list.
            if widgets.list.row_at_index(0).is_none() {
                // A freshly-opened directory has not loaded yet; take focus once
                // the first row is built (see the tail of this method).
                self.focus_pending = true;
                return;
            }
            let index = self.cursor.unwrap_or(0) as i32;
            if let Some(row) = widgets
                .list
                .row_at_index(index)
                .or_else(|| widgets.list.row_at_index(0))
            {
                row.grab_focus();
            } else if !widgets.list.grab_focus() {
                widgets.scrolled.grab_focus();
            }
            return;
        }
        self.handle_message(message, sender.clone());
        // A launch-time reveal asked for its row to be brought into view. Do it
        // once the model was updated and before any early-return on the
        // selection fast path, so it is never skipped.
        if let Some(index) = self.scroll_to.take() {
            scroll_row_into_view(widgets, index);
        }
        // The model's expanded/loaded set changed; align the lazily-watched
        // set of directories with it (startup only ever watches the root, so
        // opening a tree is instant).
        self.reconcile_watches();
        if self.selection_dirty {
            // The selection/cursor moved but the rows themselves are unchanged:
            // just restyle the affected rows instead of rebuilding every widget.
            self.selection_dirty = false;
            if update_selection_classes(self, widgets) {
                return;
            }
        }
        <Self as Component>::update_view(self, widgets, sender.clone());
        rebuild(self, widgets, sender);
        // A focus request that arrived while the directory was still loading:
        // the first row now exists, so take the keyboard.
        if self.focus_pending && widgets.list.row_at_index(0).is_some() {
            self.focus_pending = false;
            let index = self.cursor.unwrap_or(0) as i32;
            if let Some(row) = widgets
                .list
                .row_at_index(index)
                .or_else(|| widgets.list.row_at_index(0))
            {
                row.grab_focus();
            }
        }
    }
}

impl Tree {
    fn handle_message(&mut self, msg: TreeMsg, sender: ComponentSender<Self>) {
        match msg {
            TreeMsg::OpenRoot(path) => self.open_root(&path, &sender),
            TreeMsg::SelectPath(path) => self.reveal_path(path),
            // Handled in `update_with_view` (it is a pure widget-side grab).
            TreeMsg::Focus => {}
            TreeMsg::SetFilter(filter) => {
                if let Some(tree) = self.tree.as_mut() {
                    tree.set_filter(&filter);
                    self.refresh_rows();
                }
            }
            TreeMsg::SetPanel(panel) => self.content_width = thumbnail_content_width(panel),
            TreeMsg::SetSide(side) => self.side = side,
            TreeMsg::Suspend => self.suspend_media(),
            TreeMsg::Shutdown => self.shutdown(),
            TreeMsg::Noop => {}

            TreeMsg::MoveUp => self.cursor_delta(-1),
            TreeMsg::MoveDown => self.cursor_delta(1),
            TreeMsg::ExtendSelection(delta) => self.cursor_extend(delta, false),
            TreeMsg::MoveExtendingSelection(delta) => self.cursor_extend(delta, true),
            TreeMsg::CollapseCursor => {
                if let Some(path) = self.cursor_path()
                    && self.tree.as_ref().is_some_and(|t| t.is_expanded(&path))
                {
                    self.collapse_dir(&path);
                }
            }
            TreeMsg::ExpandCursor => {
                if let Some(path) = self.cursor_path() {
                    self.expand_dir(&path, &sender);
                }
            }
            TreeMsg::ActivateCursor => {
                if let Some(path) = self.cursor_path() {
                    self.activate(&path, &sender);
                }
            }
            TreeMsg::Rename => {
                if let Some(path) = self.cursor_path() {
                    self.renaming = Some(path);
                    self.renaming_state.set(true);
                }
            }

            TreeMsg::Activate(path) => self.activate(&path, &sender),
            TreeMsg::Toggle(path) => self.toggle_dir(&path, &sender),
            TreeMsg::Select(path) => {
                self.select(&path);
                self.anchor = Some(path);
                self.selection_dirty = true;
            }
            TreeMsg::ToggleSelect(path) => {
                if let Some(i) = self.rows.iter().position(|r| r.path == path) {
                    self.cursor = Some(i);
                }
                if let Some(i) = self.selected.iter().position(|p| p == &path) {
                    self.selected.remove(i);
                } else {
                    self.selected.push(path.clone());
                }
                self.anchor = Some(path);
                self.selection_dirty = true;
            }
            TreeMsg::RangeSelect(path) => {
                self.range_select(&path);
                self.selection_dirty = true;
            }
            TreeMsg::RowPress { path, is_dir, ctrl, shift } => {
                // The press point and button state are tracked by the drag
                // controller on the scrolled window (which survives the rebuild
                // this message triggers). Here we only snapshot what a drag
                // starting from this row should carry.
                if ctrl {
                    sender.input(TreeMsg::ToggleSelect(path));
                    return;
                }
                if shift {
                    sender.input(TreeMsg::RangeSelect(path));
                    return;
                }
                // Snapshot what a drag starting from this row should carry: the
                // whole selection when the row is already part of it, otherwise
                // just the row. Fixed now (before any collapse below), because
                // the drag's `prepare` runs later.
                *self.drag_payload.borrow_mut() = drag_paths(&self.selected, &path);

                // A plain press on a row already part of a multi-row selection
                // defers the collapse to release, so a drag can still carry the
                // whole selection. Everything else collapses now.
                if self.selected.len() > 1 && self.selected.iter().any(|p| p == &path) {
                    self.pending_click = Some(path.clone());
                    if is_dir {
                        self.toggle_dir(&path, &sender);
                    }
                    return;
                }
                self.pending_click = None;
                self.press_default(&path, is_dir, &sender);
            }
            TreeMsg::RowRelease { path } => {
                // The press did not become a drag; it can no longer start one.
                *self.drag_origin.borrow_mut() = None;
                self.drag_button_down.set(false);
                // The click finished without a drag: apply the deferred press
                // (collapse the multi-selection onto the pressed row).
                if self.drag_active.get() {
                    return;
                }
                if self.pending_click.take().as_deref() == Some(path.as_path()) {
                    let is_dir = self
                        .rows
                        .iter()
                        .find(|r| r.path == path)
                        .map(|r| r.is_dir)
                        .unwrap_or(false);
                    self.press_default(&path, is_dir, &sender);
                }
            }
            TreeMsg::DragStarted => {
                // A drag is in flight: cancel any deferred click collapse.
                self.drag_active.set(true);
                self.pending_click = None;
                *self.drag_origin.borrow_mut() = None;
                self.drag_button_down.set(false);
            }
            TreeMsg::DroppedAsMove(paths) => {
                // A drag left the app as a MOVE: the receiver took the data, so
                // the sources are trashed. Best-effort and non-interactive
                // (the drag already completed); failures are surfaced.
                match self.ops.trash(&paths) {
                    Ok(()) => {
                        for path in &paths {
                            self.apply_change(Change::Removed { path: path.clone() });
                        }
                        let n = paths.len();
                        self.status(format!("Moved {n} item(s) out of the panel"), &sender);
                    }
                    Err(err) => self.status(format!("Could not finish move: {err}"), &sender),
                }
            }
            TreeMsg::Deselect => {
                self.selected.clear();
                self.cursor = None;
                self.anchor = None;
                self.typeahead.clear();
                self.selection_dirty = true;
            }
            TreeMsg::Menu(path) => self.open_menu(&path),
            TreeMsg::MenuAt { x, y } => {
                if let Some(root) = self.tree.as_ref().map(|t| t.root().to_path_buf()) {
                    self.open_menu_at(&root, (x, y));
                }
            }
            TreeMsg::RenameAt(path) => {
                self.select(&path);
                self.renaming = Some(path);
                self.renaming_state.set(true);
            }
            TreeMsg::Trash(path) => self.confirm_trash(vec![path], sender.clone()),
            TreeMsg::DeleteSelected => {
                let paths = self.selected_for_clipboard();
                if !paths.is_empty() {
                    self.confirm_trash(paths, sender.clone());
                }
            }
            TreeMsg::PermanentDelete(path) => {
                self.confirm_permanent_delete(vec![path], sender.clone());
            }
            TreeMsg::PermanentDeleteSelected => {
                let paths = self.selected_for_clipboard();
                if !paths.is_empty() {
                    self.confirm_permanent_delete(paths, sender.clone());
                }
            }
            TreeMsg::ConfirmPermanentDelete(paths) => {
                if let Some(renaming) = self.renaming.as_ref()
                    && paths.contains(renaming)
                {
                    self.renaming = None;
                    self.renaming_state.set(false);
                }
                match self.ops.delete_permanently(&paths) {
                    Ok(()) => {
                        for path in paths {
                            self.apply_change(Change::Removed { path });
                        }
                        self.selected.clear();
                        self.cursor_snap();
                    }
                    Err(err) => self.status(err.to_string(), &sender),
                }
            }
            TreeMsg::ConfirmTrash(paths) => {
                if let Some(renaming) = self.renaming.as_ref()
                    && paths.contains(renaming)
                {
                    self.renaming = None;
                    self.renaming_state.set(false);
                }
                match self.ops.trash(&paths) {
                    Ok(()) => {
                        for path in paths {
                            self.apply_change(Change::Removed { path });
                        }
                        self.selected.clear();
                        self.cursor_snap();
                    }
                    // `OpsError::Trash`'s own `Display` already reads "could
                    // not move to trash: ..."; do not prefix it again.
                    Err(err) => self.status(err.to_string(), &sender),
                }
            }
            TreeMsg::Duplicate(path) => match self.ops.duplicate(&path) {
                Ok(new_path) => {
                    self.apply_change(Change::Created { path: new_path.clone() });
                    self.select(&new_path);
                }
                Err(err) => self.status(format!("Could not duplicate: {err}"), &sender),
            },
            TreeMsg::RequestDuplicateCursor => {
                if let Some(path) = self.cursor_path() {
                    sender.input(TreeMsg::Duplicate(path));
                }
            }
            TreeMsg::CopyPath(path) => {
                set_clipboard_text(&path.display().to_string());
                self.status("Copied path to clipboard".to_string(), &sender);
            }
            TreeMsg::CopyRelativePath(path) => {
                let relative = self
                    .tree
                    .as_ref()
                    .and_then(|t| path.strip_prefix(t.root()).ok())
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                set_clipboard_text(&relative);
                self.status("Copied relative path to clipboard".to_string(), &sender);
            }
            TreeMsg::AddBookmark(path) => {
                let _ = sender.output(TreeOutput::AddBookmark(path));
            }
            TreeMsg::RunCommand { command, path } => {
                let cmd = action_command(&command, &path);
                match spawn_command(&cmd) {
                    Ok(()) => self.status(format!("Ran {command} on {}", path.display()), &sender),
                    Err(err) => self.status(format!("Could not run {command}: {err}"), &sender),
                }
            }
            TreeMsg::OpenSplit(path) => {
                let _ = sender.output(TreeOutput::OpenSplit(path));
            }
            TreeMsg::OpenInOppositePanel(path) => {
                let _ = sender.output(TreeOutput::OpenOpposite(path));
            }
            TreeMsg::OpenWith(path) => open_with_dialog(&self.parent.clone(), &path, &sender),
            TreeMsg::OpenWithDefault(path) => match open_in_app(&path) {
                Ok(app) => {
                    self.status(format!("Opened {} with {app}", path.display()), &sender);
                }
                Err(err) => self.status(err, &sender),
            },
            TreeMsg::CreateLink(path) => match self.ops.create_link(&path) {
                Ok(new_path) => {
                    self.apply_change(Change::Created { path: new_path.clone() });
                }
                Err(err) => self.status(format!("Could not create link: {err}"), &sender),
            },
            TreeMsg::Properties(path) => {
                crate::ui::props::show_properties_dialog(&self.parent.clone(), &path, &sender);
            }
            TreeMsg::PropertiesSelected(paths) => {
                crate::ui::props::show_multi_properties_dialog(&self.parent.clone(), &paths);
            }
            TreeMsg::CopyPaths(paths) => {
                let text = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                set_clipboard_text(&text);
                self.status(format!("Copied {} paths to clipboard", paths.len()), &sender);
            }
            TreeMsg::CopyRelativePaths(paths) => {
                let root = self.tree.as_ref().map(|t| t.root().to_path_buf());
                let text = paths
                    .iter()
                    .map(|p| {
                        root.as_deref()
                            .and_then(|root| p.strip_prefix(root).ok())
                            .map(|rel| rel.display().to_string())
                            .unwrap_or_else(|| p.display().to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                set_clipboard_text(&text);
                self.status(
                    format!("Copied {} relative paths to clipboard", paths.len()),
                    &sender,
                );
            }
            TreeMsg::RunCommandEach { command, paths } => {
                let mut failures = 0usize;
                for path in &paths {
                    let cmd = action_command(&command, path);
                    if spawn_command(&cmd).is_err() {
                        failures += 1;
                    }
                }
                let total = paths.len();
                if failures == 0 {
                    self.status(format!("Ran {command} on {total} item(s)"), &sender);
                } else {
                    self.status(
                        format!("Ran {command} on {total} item(s); {failures} failed"),
                        &sender,
                    );
                }
            }
            TreeMsg::RenameTo { path, name } => match self.ops.rename(&path, &name) {
                Ok(new_path) => {
                    self.apply_change(Change::Renamed {
                        from: path,
                        to: new_path.clone(),
                    });
                    self.select(&new_path);
                }
                Err(err) => self.status(format!("Could not rename: {err}"), &sender),
            },
            TreeMsg::ToggleThumbnail(path) => {
                if self.thumbnails.contains(&path) {
                    // Toggled off: drop the preview(s) and their textures.
                    self.clear_thumbnails_under(&path);
                } else {
                    self.thumbnails.insert(path.clone());
                    // Turning a directory on expands it so its images show.
                    if path.is_dir() {
                        self.expand_dir(&path, &sender);
                    }
                }
            }
            TreeMsg::ToggleThumbnailPlay(path) => self.toggle_thumbnail_play(&path),
            TreeMsg::RunShortcut { target } => self.run_shortcut(target, &sender),
            TreeMsg::RunAccelerator(accel) => {
                if let Some((key, mods)) = parse_accelerator(&accel) {
                    let keys = compile_shortcuts(&self.menu);
                    if let Some(msg) = resolve_key(key, mods, &keys) {
                        sender.input(msg);
                    }
                }
            }

            TreeMsg::SelectAllRows => {
                if !self.rows.is_empty() {
                    self.selected = self.rows.iter().map(|r| r.path.clone()).collect();
                    self.cursor = Some(0);
                    self.anchor = self.rows.first().map(|r| r.path.clone());
                    self.selection_dirty = true;
                }
            }
            TreeMsg::TypeAhead(ch) => {
                let now = std::time::Instant::now();
                if let Some(prefix) = self.typeahead.push(ch, now)
                    && let Some(index) = typeahead_target(&self.rows, &prefix, self.cursor)
                {
                    let path = self.rows[index].path.clone();
                    self.cursor = Some(index);
                    self.set_single_selection(path);
                    self.selection_dirty = true;
                }
            }

            TreeMsg::RenameCommit => {
                let text = self
                    .rename_entry
                    .as_ref()
                    .map(|e| e.text().to_string())
                    .unwrap_or_default();
                let Some(old) = self.renaming.take() else {
                    return;
                };
                self.renaming_state.set(false);
                self.rename_entry = None;
                let trimmed = text.trim().to_string();
                let unchanged = old
                    .file_name()
                    .map(|n| n.to_string_lossy() == trimmed.as_str())
                    .unwrap_or(false);
                if trimmed.is_empty() || unchanged {
                    return;
                }
                match self.ops.rename(&old, &trimmed) {
                    Ok(new_path) => {
                        self.apply_change(Change::Renamed {
                            from: old,
                            to: new_path.clone(),
                        });
                        self.select(&new_path);
                    }
                    Err(err) => self.status(format!("Could not rename: {err}"), &sender),
                }
            }
            TreeMsg::RenameCancel => {
                self.renaming = None;
                self.renaming_state.set(false);
                self.rename_entry = None;
            }

            TreeMsg::Copy => {
                let paths = self.selected_for_clipboard();
                if !paths.is_empty() {
                    self.clipboard = Some(Clipboard { op: ClipboardOp::Copy, paths: paths.clone() });
                    self.status(format!("Copying {} item(s)", paths.len()), &sender);
                }
            }
            TreeMsg::Cut => {
                let paths = self.selected_for_clipboard();
                if !paths.is_empty() {
                    self.clipboard = Some(Clipboard { op: ClipboardOp::Cut, paths: paths.clone() });
                    self.status(format!("Cutting {} item(s)", paths.len()), &sender);
                }
            }
            TreeMsg::Paste => self.paste(&sender),
            TreeMsg::PastePaths(paths) => self.paste_paths(paths, &sender),

            TreeMsg::NewFile => self.create_entry(false, &sender),
            TreeMsg::NewFolder => self.create_entry(true, &sender),

            TreeMsg::ToggleHidden => self.toggle_hidden(&sender),
            TreeMsg::SetSortKey(key) => self.set_sort_key(key, &sender),
            TreeMsg::ToggleSortDirection => self.toggle_sort_direction(&sender),

            TreeMsg::FsChange(change) => {
                // Consolidate bursts into a single re-render.
                self.pending.push(change);
                if self.flush_source.is_none() {
                    let s = sender.clone();
                    let id = glib::timeout_add_local_once(
                        Duration::from_millis(100),
                        move || {
                            s.input(TreeMsg::FlushChanges);
                        }
                    );
                    self.flush_source = Some(id);
                }
            }
            TreeMsg::FlushChanges => {
                self.flush_source = None;
                let changes = std::mem::take(&mut self.pending);
                for change in changes {
                    self.apply_change(change);
                }
                self.cursor_snap();
            }
            TreeMsg::RequestOpenFolder => {
                self.status("Pick a folder to browse...".to_string(), &sender);
                let _ = sender.output(TreeOutput::OpenFolderRequested);
            }

            TreeMsg::DropInto { target, sources, copy } => {
                self.request_drop_into(target, sources, copy, &sender);
            }
            TreeMsg::DropIntoRoot { sources, copy } => {
                if let Some(root) = self.tree.as_ref().map(|t| t.root().to_path_buf()) {
                    self.request_drop_into(root, sources, copy, &sender);
                }
            }
            TreeMsg::DropIntoConfirmed { target, sources } => {
                self.drop_into(&target, &sources, false, &sender);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// key handling
// ---------------------------------------------------------------------------

/// The modifier bits that matter for accelerator matching (everything except
/// the lock/numeric-pad/altgr noise GTK sprinkles into key state).
fn accel_mods(m: gdk::ModifierType) -> gdk::ModifierType {
    m & (gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::HYPER_MASK
        | gdk::ModifierType::META_MASK)
}

/// A configured accelerator, parsed and ready to match against key events.
#[derive(Clone)]
struct KeyBinding {
    key: gdk::Key,
    mods: gdk::ModifierType,
    target: ShortcutTarget,
}

/// Parse every configured shortcut into an easily matched form. Unparseable
/// accelerators are skipped (the menu still shows them, but the key does
/// nothing).
fn compile_shortcuts(menu: &ContextMenu) -> Rc<Vec<KeyBinding>> {
    let mut out = Vec::new();
    for (accel, target) in menu.shortcuts() {
        if let Some((key, mods)) = parse_accelerator(&accel) {
            out.push(KeyBinding { key, mods, target });
        }
    }
    Rc::new(out)
}

/// Parse a shortcut into a GTK accelerator.
///
/// GTK's own parser only understands its native `<Control>x` / `<Shift>Delete`
/// syntax and silently rejects the friendlier `Ctrl+x` / `Shift+Delete` form,
/// so normalize the latter first. Both spellings are accepted.
fn parse_accelerator(accel: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
    gtk::accelerator_parse(normalize_accelerator(accel))
}

/// Rewrite a `+`-joined accelerator (`Ctrl+Shift+m`) as GTK's `<Control><Shift>m`
/// form. Strings without a `+` (already native, or a bare key) pass through.
fn normalize_accelerator(accel: &str) -> String {
    if !accel.contains('+') {
        return normalize_key_name(accel);
    }
    let mut mods = String::new();
    let mut key = String::new();
    for token in accel.split('+') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "primary" => mods.push_str("<Control>"),
            "shift" => mods.push_str("<Shift>"),
            "alt" => mods.push_str("<Alt>"),
            "super" | "win" | "mod4" => mods.push_str("<Super>"),
            "meta" => mods.push_str("<Meta>"),
            "hyper" => mods.push_str("<Hyper>"),
            _ => key = normalize_key_name(token),
        }
    }
    format!("{mods}{key}")
}

/// Map friendly key names GTK does not know onto its canonical ones (`Enter`
/// is `Return`), so an IPC `--key Enter` reaches the tree.
fn normalize_key_name(name: &str) -> String {
    if name.eq_ignore_ascii_case("enter") {
        "Return".to_owned()
    } else {
        name.to_owned()
    }
}

/// Resolve a key press to the tree message it should run: structural
/// navigation first (not configurable), then the configured context-menu
/// accelerators. `None` means nothing matched. Shared by real key presses and
/// the IPC `--key` command.
fn resolve_key(key: gdk::Key, state: gdk::ModifierType, keys: &[KeyBinding]) -> Option<TreeMsg> {
    use gdk::Key;

    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);

    // Structural navigation is not configurable; these always win. The arrow
    // keys carry the shift/ctrl state so shift extends the selection.
    let structural = match key {
        Key::Up if ctrl && shift => Some(TreeMsg::MoveExtendingSelection(-1)),
        Key::Down if ctrl && shift => Some(TreeMsg::MoveExtendingSelection(1)),
        Key::Up if shift => Some(TreeMsg::ExtendSelection(-1)),
        Key::Down if shift => Some(TreeMsg::ExtendSelection(1)),
        Key::Up => Some(TreeMsg::MoveUp),
        Key::Down => Some(TreeMsg::MoveDown),
        Key::Left => Some(TreeMsg::CollapseCursor),
        Key::Right => Some(TreeMsg::ExpandCursor),
        Key::Return | Key::KP_Enter | Key::space => Some(TreeMsg::ActivateCursor),
        Key::Escape => Some(TreeMsg::RenameCancel),
        Key::o | Key::O if ctrl && !shift => Some(TreeMsg::RequestOpenFolder),
        Key::a | Key::A if ctrl && !shift => Some(TreeMsg::SelectAllRows),
        _ => None,
    };
    if let Some(msg) = structural {
        return Some(msg);
    }

    // Everything else comes from the configured accelerators
    // (cut/copy/paste/rename/delete/new tab/...), fired against the row under
    // the keyboard cursor.
    keys.iter()
        .find(|binding| {
            key.to_lower() == binding.key.to_lower()
                && accel_mods(state) == accel_mods(binding.mods)
        })
        .map(|binding| TreeMsg::RunShortcut {
            target: binding.target.clone(),
        })
}

fn handle_key(
    key: gdk::Key,
    state: gdk::ModifierType,
    renaming: bool,
    preview_focused: bool,
    keys: &[KeyBinding],
    sender: &ComponentSender<Tree>,
) -> glib::Propagation {
    // While a rename entry or an inline text preview is focused, let it win all
    // keys (the text view handles selection, copy and cursor movement itself).
    if renaming || preview_focused {
        return glib::Propagation::Proceed;
    }

    if let Some(msg) = resolve_key(key, state, keys) {
        sender.input(msg);
        return glib::Propagation::Stop;
    }

    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    // No binding matched. A bare printable key (no ctrl/alt/super) starts or
    // extends a type-ahead search.
    if !ctrl
        && !state.contains(gdk::ModifierType::ALT_MASK)
        && !state.contains(gdk::ModifierType::SUPER_MASK)
        && let Some(ch) = key.to_unicode()
        && ch.is_ascii_graphic()
    {
        sender.input(TreeMsg::TypeAhead(ch));
        return glib::Propagation::Stop;
    }

    glib::Propagation::Proceed
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// The rows are unchanged; only the selection/cursor moved. Restyle the
/// existing `ListBoxRow`s in place (O(rows) class toggles, no widget
/// construction). Returns `false` when the row set no longer matches the model
/// (so the caller falls back to a full rebuild).
fn update_selection_classes(
    tree: &mut Tree,
    widgets: &mut <Tree as Component>::Widgets,
) -> bool {
    // If the row count drifted (a background change), rebuild instead.
    let mut existing = 0;
    let mut child = widgets.list.first_child();
    while let Some(current) = child {
        if current.is::<gtk::ListBoxRow>() {
            existing += 1;
        }
        child = current.next_sibling();
    }
    if existing != tree.rows.len() {
        return false;
    }
    for (index, row) in tree.rows.iter().enumerate() {
        let Some(list_row) = widgets.list.row_at_index(index as i32) else {
            return false;
        };
        let selected = tree.cursor == Some(index) || tree.selected.contains(&row.path);
        if selected {
            list_row.add_css_class("tree-row-selected");
        } else {
            list_row.remove_css_class("tree-row-selected");
        }
    }
    true
}

/// Scroll `widgets.scrolled` so row `index` is visible. Uses the row's
/// allocation relative to the list and nudges the vertical adjustment only when
/// the row lies outside the current viewport (so a visible reveal does not
/// jerk the scroll position).
fn scroll_row_into_view(widgets: &<Tree as Component>::Widgets, index: usize) {
    let Some(row) = widgets.list.row_at_index(index as i32) else {
        return;
    };
    // Suppose the row is allocated at `y` within the list; convert to the
    // viewport and scroll only if it falls outside `[0, page_size)`.
    let adj = widgets.scrolled.vadjustment();
    let page = adj.page_size();
    if page <= 0.0 {
        return;
    }
    // Bounds of the row in the list's coordinate space, which is the space the
    // scroll adjustment measures against.
    let Some(bounds) = row.compute_bounds(&widgets.list) else {
        return;
    };
    let y = bounds.y() as f64;
    let height = bounds.height() as f64;
    let top = adj.value();
    if y < top {
        adj.set_value(y);
    } else if y + height > top + page {
        adj.set_value((y + height - page).max(0.0));
    }
}

fn rebuild(tree: &mut Tree, widgets: &mut <Tree as Component>::Widgets, sender: ComponentSender<Tree>) {
    let list = &widgets.list;

    // The context-menu popover is parented to one of the rows below (see
    // `build_menu`). `ListBox::remove_all` destroys every row unconditionally;
    // if the popover were still attached to one, GTK would finalize a widget
    // that still has a child, which corrupts widget state and reliably
    // crashes the process a little later (surfaces as
    // `gtk_accessible_get_accessible_role: assertion 'GTK_IS_ACCESSIBLE
    // (self)' failed` and friends). Every rebuild — not just the ones
    // triggered by opening a new menu or root — must detach it first.
    if let Some(previous) = tree.popover.take() {
        previous.unparent();
    }

    // Detach the list while it is rebuilt: with several thousand rows, each
    // `append` into a list that is already inside a `ScrolledWindow` forces a
    // re-measure, making the rebuild O(n²). Reattaching once at the end keeps
    // insertion linear.
    widgets.scrolled.set_child(None::<&gtk::Widget>);
    list.remove_all();

    if tree.tree.is_none() {
        widgets.scrolled.set_child(Some(list));
        return;
    }

    // Swapping the rows out lets us mutate other `Tree` fields in the loop
    // without a borrow conflict.
    let rows = std::mem::take(&mut tree.rows);
    let mut menu_row: Option<gtk::ListBoxRow> = None;
    tree.rename_entry = None;
    // The previous render's GIF pictures are about to be destroyed; drop them
    // so the ticker can't paint into freed widgets (they're re-added below).
    tree.gif_widgets.borrow_mut().clear();
    // Likewise the previous render's audio players: `audio_tick` must never
    // touch controls that this rebuild is about to free.
    tree.audio_widgets.borrow_mut().clear();
    // Stop media for rows that no longer render a preview, whatever the reason
    // (collapsed, filtered out, preview turned off, view switched). A row that
    // is still listed but whose thumbnail was closed must not keep its stream.
    let visible: HashSet<PathBuf> = rows
        .iter()
        .filter(|row| tree.wants_preview(row))
        .map(|row| row.path.clone())
        .collect();
    tree.retain_visible_media(&visible);

    for (index, row) in rows.iter().enumerate() {
        let list_row = gtk::ListBoxRow::new();
        list_row.set_focusable(true);
        list_row.set_can_focus(true);
        list_row.add_css_class("tree-row");

        let row_indent = row.depth as i32 * 16 + 4;
        // A vertical container so an inline thumbnail can sit below the row.
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_hexpand(true);
        list_row.set_child(Some(&container));

        let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        hbox.set_margin_start(row_indent);
        hbox.set_hexpand(true);
        container.append(&hbox);

        let icon = gtk::Image::from_icon_name(icon_name(row));
        icon.set_pixel_size(tree.config.icon_size as i32);
        icon.set_valign(gtk::Align::Center);
        hbox.append(&icon);

        if tree.renaming.as_ref() == Some(&row.path) {
            // The entry is recreated on every rebuild (rows are rebuilt from
            // scratch) and the live instance is handed back to the model so a
            // later commit can read the text.
            let entry = gtk::Entry::new();
            entry.set_text(&row.name);
            entry.set_hexpand(true);
            entry.add_css_class("tree-rename-entry");
            {
                let s = sender.clone();
                entry.connect_activate(move |_| s.input(TreeMsg::RenameCommit));
            }
            let esc_controller = gtk::EventControllerKey::new();
            let s = sender.clone();
            esc_controller.connect_key_pressed(move |_, key, _code, _state| {
                if key == gdk::Key::Escape {
                    s.input(TreeMsg::RenameCancel);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            entry.add_controller(esc_controller);
            hbox.append(&entry);
            tree.rename_entry = Some(entry.clone());
            entry.grab_focus();
        } else {
            let label = gtk::Label::new(Some(&row.name));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(pango::EllipsizeMode::Middle);
            label.set_tooltip_text(Some(&row.path.to_string_lossy()));
            hbox.append(&label);
        }

        if tree.cursor == Some(index) || tree.selected.contains(&row.path) {
            list_row.add_css_class("tree-row-selected");
        }

        if row.matches {
            list_row.add_css_class("tree-row-match");
        }

        if tree.menu_target.as_ref() == Some(&row.path) {
            menu_row = Some(list_row.clone());
        }

        // Inline preview (a previewable file, or every one under a directory
        // the user turned on). Sized to the column, never enlarged.
        if tree.wants_preview(row) {
            tree.append_preview(&row.path, row_indent, &container, &sender);
        }

        let path = row.path.clone();
        let release_path = row.path.clone();
        let is_dir = row.is_dir;
        let click = gtk::GestureClick::new();
        // `GtkGestureSingle::button` defaults to 1 (primary only); without
        // this, right-clicks never reach `connect_pressed` at all and the
        // context menu is unreachable.
        click.set_button(0);
        let s = sender.clone();
        let focus_row = list_row.clone();
        click.connect_pressed(move |gesture, _n_press, x, y| {
            // Clicks on an inline player's controls, or on the selectable text
            // preview, belong to that widget. Handling them here would select
            // the row and rebuild it, destroying the widget before its click
            // completes — so skip any click that lands inside one.
            if let Some(widget) = focus_row.pick(x, y, gtk::PickFlags::DEFAULT)
                && in_inline_media(&widget)
            {
                // A click on the selectable text preview must keep focus on the
                // text view. The list box moves focus back to the tree once the
                // event settles, so restore it on the next idle (after that has
                // happened). Media controls do not need focus.
                if let Some(view) = inline_text_view(&widget) {
                    glib::idle_add_local_once(move || {
                        view.grab_focus();
                    });
                }
                return;
            }
            // Nothing else claims keyboard focus for us: without this, a
            // click on a row that follows focus being anywhere else (the
            // filter entry, another app entirely) leaves every keyboard
            // shortcut firing into whatever was focused before, not the tree.
            focus_row.grab_focus();
            let button = gesture.current_button();
            let state = gesture.current_event_state();
            if button == 3 {
                // Super+right is the panel-resize drag: release the sequence so
                // the window's drag gesture can claim it instead of opening the
                // row menu.
                if state.contains(gdk::ModifierType::SUPER_MASK) {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                s.input(TreeMsg::Menu(path.clone()));
                return;
            }
            if button != 1 {
                return;
            }
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            // Directories toggle on the first press, so the action must be
            // immediate; a following second press is swallowed in the handler.
            s.input(TreeMsg::RowPress { path: path.clone(), is_dir, ctrl, shift });
        });
        {
            // A release with no drag finishes a press whose selection collapse
            // was deferred (see `RowPress`), so a multi-selection click still
            // collapses once we know no drag is in flight.
            let s = sender.clone();
            let release_row = list_row.clone();
            click.connect_released(move |gesture, _n_press, x, y| {
                if gesture.current_button() != 1 {
                    return;
                }
                // A click on the selectable text preview keeps focus there; the
                // list box moves focus back to the tree on release, so reclaim
                // it once that has settled.
                if let Some(widget) = release_row.pick(x, y, gtk::PickFlags::DEFAULT)
                    && let Some(view) = inline_text_view(&widget)
                {
                    glib::idle_add_local_once(move || {
                        view.grab_focus();
                    });
                    return;
                }
                s.input(TreeMsg::RowRelease { path: release_path.clone() });
            });
        }
        list_row.add_controller(click);

        // Drag-out is driven by a single motion controller on the scrolled
        // window (installed in `init`), not by per-row controllers: a press
        // rebuilds the rows, which would destroy any controller attached to the
        // pressed row before the drag could start. See `begin_row_drag`.

        // Drop target: only directories accept drops (files within the tree
        // are moved/copied into whichever directory row they land on).
        // Drop target: only directories accept drops (files within the tree
        // are moved/copied into whichever directory row they land on).
        if row.is_dir {
            let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::MOVE | gdk::DragAction::COPY);
            let s = sender.clone();
            let target_dir = row.path.clone();
            let active = tree.drag_active.clone();
            let landed = tree.drag_landed_internal.clone();
            drop.connect_drop(move |target, value, _x, _y| {
                let Ok(list) = value.get::<gdk::FileList>() else {
                    return false;
                };
                let sources: Vec<PathBuf> = list.files().iter().filter_map(gio::File::path).collect();
                if sources.is_empty() {
                    return false;
                }
                let copy = target.current_event_state().contains(gdk::ModifierType::CONTROL_MASK);
                // An in-tree drag landing here is handled by `drop_into`; mark
                // it so `drag-end` does not also act on the source.
                if active.get() {
                    landed.set(true);
                }
                s.input(TreeMsg::DropInto { target: target_dir.clone(), sources, copy });
                true
            });
            list_row.add_controller(drop);
        }

        list.append(&list_row);
    }

    tree.rows = rows;
    widgets.scrolled.set_child(Some(list));

    // The context menu must anchor to a row that is actually attached, so it
    // is built after the loop instead of inside the menu handler. The open
    // directory has no row of its own, so that menu anchors at the click point.
    if let Some(row) = menu_row {
        let target = tree.menu_target.take().expect("menu_row set implies a target");
        let selection = tree.selected.clone();
        tree.popover = Some(build_menu(&row, &target, &tree.menu, &selection, tree.side, &sender));
        tree.menu_point = None;
    } else if let (Some(target), Some(at)) = (tree.menu_target.take(), tree.menu_point.take()) {
        let selection = tree.selected.clone();
        tree.popover = Some(build_menu_at(&widgets.scrolled, at, &target, &tree.menu, &selection, tree.side, &sender));
    }
    tree.menu_target = None;
    tree.menu_point = None;
}

/// The builtin an action carries, if it is one (plain string or table form).
fn action_builtin(action: &ContextAction) -> Option<BuiltinAction> {
    match action {
        ContextAction::Builtin(b) => Some(*b),
        ContextAction::Entry(e) => Some(e.action),
        ContextAction::Command(_) | ContextAction::Submenu(_) => None,
    }
}

/// The label shown for `action` on a row. A few builtins compute a dynamic
/// label from the row's path and the side of the screen the tree lives on:
///   * "Open With {default app}" — the default application's display name;
///   * "In {right|left} panel" — the opposite side of the dock.
pub(crate) fn menu_label(action: &ContextAction, path: &Path, side: PanelSide) -> String {
    let dynamic = match action_builtin(action) {
        Some(BuiltinAction::OpenWithDefault) => default_app_label(path),
        Some(BuiltinAction::InOppositePanel) => Some(format!("In {} panel", side.opposite().name())),
        // Not every preview is a picture: audio has a player, text a snippet, an
        // archive its contents. Let those say what they show.
        Some(BuiltinAction::ViewThumbnail) => preview::detect(path)
            .and_then(PreviewKind::menu_label)
            .map(str::to_owned),
        _ => None,
    };
    dynamic.unwrap_or_else(|| action.label())
}

/// The human-readable form of a configured accelerator ("Ctrl+X", "F2", ...).
/// Shared with the hamburger menu so both render shortcut hints identically.
pub(crate) fn accel_display(accel: &str) -> Option<String> {
    let (key, mods) = parse_accelerator(accel)?;
    Some(gtk::accelerator_get_label(key, mods).to_string())
}

/// Lay out the configured [`ContextMenu`] rules for `path` as a vertical box.
/// `selection` is the tree's current selection; when it holds more than one
/// path the menu is built for a multi-selection (the `multi` rule wins and
/// selection-wide actions are offered).
fn build_menu_box(
    path: &Path,
    menu: &ContextMenu,
    selection: &[PathBuf],
    side: PanelSide,
    sender: &ComponentSender<Tree>,
) -> gtk::Box {
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu_box.add_css_class("tree-menu");
    let path = path.to_path_buf();
    let actions = menu.actions_for_selection(&path, selection.len());
    append_menu_items(&menu_box, &actions, &path, selection, side, sender);
    menu_box
}

/// Append `actions` (and, recursively, any submenus) to `menu_box`. Shared by
/// the top-level context menu and its nested submenus.
fn append_menu_items(
    menu_box: &gtk::Box,
    actions: &[ContextAction],
    path: &Path,
    selection: &[PathBuf],
    side: PanelSide,
    sender: &ComponentSender<Tree>,
) {
    let multi = selection.len() > 1;
    for action in actions {
        // Hidden items are skipped entirely — their shortcut stays live, but
        // they never appear in the menu (nor do hidden separators/submenus).
        if action.is_hidden() {
            continue;
        }

        // Separators render as a divider; they never dispatch.
        if action_builtin(action) == Some(BuiltinAction::Separator) {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.add_css_class("tree-menu-separator");
            menu_box.append(&separator);
            continue;
        }

        // Drop actions that can't describe this row (a rule may list them; the
        // rows they apply to just never match). In a multi-selection, actions
        // that only make sense for one row are hidden too.
        if let Some(builtin) = action_builtin(action) {
            if builtin.is_directory_only() && !path.is_dir() {
                continue;
            }
            if multi && builtin.is_single_row_only() {
                continue;
            }
            if matches!(builtin, BuiltinAction::OpenWithDefault) && path.is_dir() {
                continue;
            }
            // A preview is only offered for a directory (its children) or a
            // file we know how to preview.
            if builtin == BuiltinAction::ViewThumbnail
                && !path.is_dir()
                && preview::detect(path).is_none()
            {
                continue;
            }
        }

        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row_box.add_css_class("tree-menu-item");

        // A submenu row opens a nested popover instead of dispatching.
        if let ContextAction::Submenu(sub) = action {
            let button = submenu_row_button(&sub.label);
            let popover = build_submenu_popover(&sub.items, path, selection, side, sender);
            let child = popover.clone();
            let anchor = button.clone();
            button.connect_clicked(move |_| {
                // Re-anchor on each open; `closed` unparents so GTK never tears
                // down a widget that still owns a parented popover.
                child.set_parent(&anchor);
                child.popup();
            });
            let child = popover.clone();
            popover.connect_closed(move |_| child.unparent());
            row_box.append(&button);
            row_box.append(&submenu_arrow());
            menu_box.append(&row_box);
            continue;
        }

        let label = menu_label(action, path, side);
        let msg = match action {
            ContextAction::Builtin(b) => action_message(*b, path, selection),
            ContextAction::Entry(e) => action_message(e.action, path, selection),
            ContextAction::Command(cmd) => {
                if multi {
                    TreeMsg::RunCommandEach {
                        command: cmd.command.clone(),
                        paths: selection.to_vec(),
                    }
                } else {
                    TreeMsg::RunCommand {
                        command: cmd.command.clone(),
                        path: path.to_path_buf(),
                    }
                }
            }
            ContextAction::Submenu(_) => unreachable!("handled above"),
        };

        // A button with an explicit label so the text can be left-aligned
        // (the shortcut hint, if any, stays pinned to the right).
        let button = gtk::Button::new();
        button.set_halign(gtk::Align::Fill);
        button.set_hexpand(true);
        let label_widget = gtk::Label::new(Some(&label));
        label_widget.set_xalign(0.0);
        label_widget.set_hexpand(true);
        button.set_child(Some(&label_widget));
        let s = sender.clone();
        button.connect_clicked(move |_| s.input(msg.clone()));
        row_box.append(&button);

        if let Some(shortcut) = action.shortcut()
            && let Some(display) = accel_display(&shortcut)
        {
            let hint = gtk::Label::new(Some(&display));
            hint.add_css_class("tree-menu-shortcut");
            hint.set_halign(gtk::Align::End);
            hint.set_valign(gtk::Align::Center);
            row_box.append(&hint);
        }

        menu_box.append(&row_box);
    }
}

/// A full-width, left-aligned button for a submenu's label row.
fn submenu_row_button(label: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    button.set_child(Some(&text));
    button
}

/// The trailing "▸" glyph shown on a submenu row.
fn submenu_arrow() -> gtk::Label {
    let arrow = gtk::Label::new(Some("\u{25b8}"));
    arrow.add_css_class("tree-menu-submenu-arrow");
    arrow.set_halign(gtk::Align::End);
    arrow.set_valign(gtk::Align::Center);
    arrow
}

/// Build a nested popover for a submenu, parented to its row's button and
/// recursively laid out from `items`.
fn build_submenu_popover(
    items: &[ContextAction],
    path: &Path,
    selection: &[PathBuf],
    side: PanelSide,
    sender: &ComponentSender<Tree>,
) -> gtk::Popover {
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu_box.add_css_class("tree-menu");
    append_menu_items(&menu_box, items, path, selection, side, sender);

    let popover = gtk::Popover::new();
    popover.add_css_class("tree-menu-popover");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Right);
    popover.set_child(Some(&menu_box));
    popover
}

/// Show `menu_box` in a popover parented to `anchor`, optionally pointing at
/// `point` (in `anchor`'s coordinate space) rather than the anchor's edge.
fn show_menu(
    anchor: &impl IsA<gtk::Widget>,
    menu_box: &gtk::Box,
    point: Option<(f64, f64)>,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class("tree-menu-popover");
    popover.set_child(Some(menu_box));
    popover.set_autohide(true);
    popover.set_parent(anchor);
    if let Some((x, y)) = point {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    }
    popover.popup();
    popover
}

/// Build a context-menu popover anchored to `row` for `path`.
fn build_menu(
    row: &gtk::ListBoxRow,
    path: &Path,
    menu: &ContextMenu,
    selection: &[PathBuf],
    side: PanelSide,
    sender: &ComponentSender<Tree>,
) -> gtk::Popover {
    let menu_box = build_menu_box(path, menu, selection, side, sender);
    show_menu(row, &menu_box, None)
}

/// Build a context-menu popover for `path` anchored at the click point `at`
/// within `anchor` (used for the open directory, which has no row of its own).
fn build_menu_at(
    anchor: &impl IsA<gtk::Widget>,
    at: (f64, f64),
    path: &Path,
    menu: &ContextMenu,
    selection: &[PathBuf],
    side: PanelSide,
    sender: &ComponentSender<Tree>,
) -> gtk::Popover {
    let menu_box = build_menu_box(path, menu, selection, side, sender);
    show_menu(anchor, &menu_box, Some(at))
}

/// The [`TreeMsg`] a builtin context-menu action resolves to for `path`, given
/// the current `selection`. When `selection` holds more than one path the
/// selection-wide forms are produced (trash/delete/copy-path act on every
/// selected row); the single-row actions are filtered out before this is
/// reached, so their arms are only used in single-selection mode.
fn action_message(action: BuiltinAction, path: &Path, selection: &[PathBuf]) -> TreeMsg {
    if selection.len() > 1 {
        match action {
            BuiltinAction::Trash => return TreeMsg::DeleteSelected,
            BuiltinAction::DeletePermanently => return TreeMsg::PermanentDeleteSelected,
            BuiltinAction::CopyPath => return TreeMsg::CopyPaths(selection.to_vec()),
            BuiltinAction::CopyRelativePath => return TreeMsg::CopyRelativePaths(selection.to_vec()),
            BuiltinAction::Properties => return TreeMsg::PropertiesSelected(selection.to_vec()),
            _ => {}
        }
    }
    builtin_message(action, path)
}

/// The [`TreeMsg`] a builtin context-menu action resolves to for `path`.
fn builtin_message(action: BuiltinAction, path: &Path) -> TreeMsg {
    match action {
        BuiltinAction::Open => TreeMsg::Activate(path.to_path_buf()),
        BuiltinAction::OpenSplit => TreeMsg::OpenSplit(path.to_path_buf()),
        BuiltinAction::InNewPanel => TreeMsg::OpenSplit(path.to_path_buf()),
        BuiltinAction::InOppositePanel => TreeMsg::OpenInOppositePanel(path.to_path_buf()),
        BuiltinAction::OpenWith => TreeMsg::OpenWith(path.to_path_buf()),
        BuiltinAction::OpenWithDefault => TreeMsg::OpenWithDefault(path.to_path_buf()),
        BuiltinAction::ViewThumbnail => TreeMsg::ToggleThumbnail(path.to_path_buf()),
        BuiltinAction::NewFile => TreeMsg::NewFile,
        BuiltinAction::NewFolder => TreeMsg::NewFolder,
        BuiltinAction::Duplicate => TreeMsg::Duplicate(path.to_path_buf()),
        BuiltinAction::CreateLink => TreeMsg::CreateLink(path.to_path_buf()),
        BuiltinAction::Cut => TreeMsg::Cut,
        BuiltinAction::Copy => TreeMsg::Copy,
        BuiltinAction::Paste => TreeMsg::Paste,
        BuiltinAction::Rename => TreeMsg::RenameAt(path.to_path_buf()),
        BuiltinAction::CopyPath => TreeMsg::CopyPath(path.to_path_buf()),
        BuiltinAction::CopyRelativePath => TreeMsg::CopyRelativePath(path.to_path_buf()),
        BuiltinAction::AddBookmark => TreeMsg::AddBookmark(path.to_path_buf()),
        BuiltinAction::Properties => TreeMsg::Properties(path.to_path_buf()),
        BuiltinAction::Trash => TreeMsg::Trash(path.to_path_buf()),
        BuiltinAction::DeletePermanently => TreeMsg::PermanentDelete(path.to_path_buf()),
        BuiltinAction::ToggleHidden => TreeMsg::ToggleHidden,
        BuiltinAction::SortByName => TreeMsg::SetSortKey(SortKey::Name),
        BuiltinAction::SortBySize => TreeMsg::SetSortKey(SortKey::Size),
        BuiltinAction::SortByModified => TreeMsg::SetSortKey(SortKey::Modified),
        BuiltinAction::SortByType => TreeMsg::SetSortKey(SortKey::Type),
        BuiltinAction::ToggleSortAscending => TreeMsg::ToggleSortDirection,
        // Pane-level actions are dispatched by the toolbar, never by a row menu.
        BuiltinAction::OpenFolder
        | BuiltinAction::Filter
        | BuiltinAction::SplitView
        | BuiltinAction::Up
        | BuiltinAction::Back
        | BuiltinAction::Forward
        | BuiltinAction::Collapse
        | BuiltinAction::ClosePane
        | BuiltinAction::ToggleBookmarks
        | BuiltinAction::MoveToPreviousWorkspace
        | BuiltinAction::MoveToNextWorkspace
        | BuiltinAction::MoveToWorkspace
        | BuiltinAction::NewBookmark
        | BuiltinAction::NewBookmarkFolder => unreachable!("pane action in a row context menu"),
        // Bookmark-only actions are handled by the bookmarks view, never a tree.
        BuiltinAction::EditBookmark | BuiltinAction::DeleteBookmark => TreeMsg::Noop,
        BuiltinAction::Separator => unreachable!("separators are rendered, not dispatched"),
    }
}

/// Whether `action` can be run against a bare path with no tree row behind it
/// (used by the bookmarks view, whose entries point at directories that may not
/// be visible in any tree). Actions that need the tree's model or selection —
/// new file/folder, cut/copy/paste, thumbnails — are excluded.
pub(crate) fn is_path_safe(action: BuiltinAction) -> bool {
    matches!(
        action,
        BuiltinAction::Open
            | BuiltinAction::OpenSplit
            | BuiltinAction::InNewPanel
            | BuiltinAction::InOppositePanel
            | BuiltinAction::OpenWith
            | BuiltinAction::OpenWithDefault
            | BuiltinAction::Duplicate
            | BuiltinAction::CreateLink
            | BuiltinAction::CopyPath
            | BuiltinAction::CopyRelativePath
            | BuiltinAction::Properties
            | BuiltinAction::Trash
            | BuiltinAction::DeletePermanently
            | BuiltinAction::AddBookmark
    )
}

/// The [`TreeMsg`] that runs path-safe `action` against `path` for a caller
/// with no tree row (the bookmarks view). `None` for actions without a
/// path-based form; `Open` is handled by the app directly and never reaches
/// here.
pub(crate) fn path_action_message(action: BuiltinAction, path: &Path) -> Option<TreeMsg> {
    if !is_path_safe(action) || action == BuiltinAction::Open {
        return None;
    }
    Some(builtin_message(action, path))
}

/// Extensions that map to a package/archive icon in the row list.
const ARCHIVE_EXTENSIONS: [&str; 8] = ["zip", "tar", "gz", "xz", "bz2", "zst", "7z", "rar"];

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| extensions.iter().any(|candidate| ext.eq_ignore_ascii_case(candidate)))
}

fn icon_name(row: &VisibleRow) -> &'static str {
    if row.is_dir {
        return if row.expanded { "folder-open-symbolic" } else { "folder-symbolic" };
    }
    // Match on the extension only (case-insensitively) rather than lowercasing
    // the whole path string, which allocated a new `String` for every row on
    // every rebuild.
    if has_extension(&row.path, &["pdf"]) {
        "application-pdf-symbolic"
    } else if has_extension(&row.path, &["jpg", "jpeg", "png", "gif", "svg", "webp", "bmp", "avif"])
    {
        "image-x-generic-symbolic"
    } else if has_extension(&row.path, &["mp3", "flac", "wav", "ogg", "m4a", "opus"]) {
        "audio-x-generic-symbolic"
    } else if has_extension(&row.path, &["mp4", "mkv", "webm", "mov", "avi"]) {
        "video-x-generic-symbolic"
    } else if has_extension(&row.path, &ARCHIVE_EXTENSIONS) {
        "package-x-generic-symbolic"
    } else {
        "text-x-generic-symbolic"
    }
}

// ---------------------------------------------------------------------------
// model operations
// ---------------------------------------------------------------------------

/// A GIF animation being played inline. The iterator keeps its animation
/// alive and yields the frame for a given wall-clock time.
struct GifAnim {
    iter: gdk::gdk_pixbuf::PixbufAnimationIter,
}

/// The live controls of one inline audio player. `audio_tick` reads `player`
/// and writes the other widgets, so the bar/clock/icon stay correct even as
/// the row is rebuilt.
struct AudioWidgets {
    /// Weak so the widgets never outlive (and keep playing) a discarded preview;
    /// [`Tree::audio`] is the sole strong owner.
    player: Weak<AudioPlayer>,
    play: gtk::Button,
    seek: gtk::Scale,
    time: gtk::Label,
    /// True while the user is dragging the seek bar, so the ticker doesn't
    /// yank the handle out from under them.
    seeking: Rc<Cell<bool>>,
}

/// Builtins that act on a specific row. With nothing selected (the blank area
/// was clicked), a bound hotkey for one of these does nothing rather than
/// acting on the open directory.
fn needs_selection(action: BuiltinAction) -> bool {
    matches!(
        action,
        BuiltinAction::Rename
            | BuiltinAction::Duplicate
            | BuiltinAction::CreateLink
            | BuiltinAction::Cut
            | BuiltinAction::Copy
            | BuiltinAction::OpenWith
            | BuiltinAction::Trash
            | BuiltinAction::DeletePermanently
    )
}

/// Whether `widget` is, or is inside, a tree row. Used to tell a click on a row
/// from a click on the blank space below the rows.
fn in_list_row(widget: &gtk::Widget) -> bool {
    let mut current = Some(widget.clone());
    while let Some(widget) = current {
        if widget.downcast_ref::<gtk::ListBoxRow>().is_some() {
            return true;
        }
        current = widget.parent();
    }
    false
}

/// Whether `widget` is, or is inside, an inline media widget (a video or the
/// audio player's controls), so the row leaves such clicks to that widget.
fn in_inline_media(widget: &gtk::Widget) -> bool {
    let mut current = Some(widget.clone());
    while let Some(widget) = current {
        if widget.downcast_ref::<gtk::Video>().is_some()
            || widget.has_css_class("tree-audio")
            || widget.has_css_class("tree-preview-scroll")
            || widget.has_css_class("tree-preview-text")
        {
            return true;
        }
        current = widget.parent();
    }
    false
}

/// Whether `widget` is the inline text preview (or its scroller), which owns
/// pointer drags for text selection rather than starting a file drag.
fn in_text_preview(widget: &gtk::Widget) -> bool {
    let mut current = Some(widget.clone());
    while let Some(widget) = current {
        if widget.has_css_class("tree-preview-text") || widget.has_css_class("tree-preview-scroll") {
            return true;
        }
        current = widget.parent();
    }
    false
}

/// The inline text preview under `widget`, if the widget is one or a descendant
/// of one.
fn inline_text_view(widget: &gtk::Widget) -> Option<gtk::TextView> {
    let mut current = Some(widget.clone());
    while let Some(widget) = current {
        if widget.has_css_class("tree-preview-text") {
            return widget.downcast::<gtk::TextView>().ok();
        }
        current = widget.parent();
    }
    None
}

/// Width in px available to a depth-0 thumbnail: the panel's inner width, minus
/// the scrollbar and a little padding.
fn thumbnail_content_width(panel: PanelConfig) -> i32 {
    (panel.width as i32 - 2 * panel.margin as i32 - 24).max(64)
}

/// Target (width, height) for a thumbnail: scaled to `available` width while
/// keeping the aspect ratio, but never enlarged past the image's natural size.
fn thumbnail_size(texture: &gdk::Texture, available: i32) -> (i32, i32) {
    let natural_w = texture.width().max(1);
    let natural_h = texture.height().max(1);
    let available = available.max(32);
    if natural_w <= available {
        return (natural_w, natural_h);
    }
    let height = (natural_h as i64 * available as i64 / natural_w as i64).max(1) as i32;
    (available, height)
}

/// A thumbnail `GtkPicture` sized to `width`/`height`, aligned under the row's
/// label.
fn thumbnail_picture(
    texture: &gdk::Texture,
    margin_start: i32,
    width: i32,
    height: i32,
) -> gtk::Picture {
    let picture = gtk::Picture::for_paintable(texture);
    picture.add_css_class("tree-thumbnail");
    picture.set_can_shrink(true);
    picture.set_content_fit(gtk::ContentFit::ScaleDown);
    picture.set_halign(gtk::Align::Start);
    picture.set_size_request(width, height);
    picture.set_margin_start(margin_start);
    picture.set_margin_bottom(6);
    picture
}

/// `m:ss` (or `h:mm:ss` past an hour) for a media timestamp in microseconds.
/// Unknown (`<= 0`) durations render as `-:--`.
fn format_time(micros: i64) -> String {
    if micros <= 0 {
        return "-:--".to_owned();
    }
    let total = micros / 1_000_000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Build the inline audio player for an audio file: play/pause, a seek bar, a
/// position/duration clock and a volume slider. The `GtkMediaFile` is shared
/// with `AudioWidgets` so `audio_tick` can drive the bar and clock.
fn build_audio_player(player: &Rc<AudioPlayer>, margin_start: i32) -> (gtk::Box, AudioWidgets) {
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.add_css_class("tree-audio");
    controls.set_margin_start(margin_start);
    controls.set_margin_bottom(6);
    controls.set_hexpand(true);

    let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
    play.add_css_class("flat");
    play.set_tooltip_text(Some("Play"));
    play.set_valign(gtk::Align::Center);
    {
        // Hold the player weakly: the row widgets outlive the `Tree`'s own
        // caches long enough to matter (a removed row can stay alive until GTK
        // re-focuses), and a strong handle here would keep the sink playing
        // after the preview was discarded.
        let player = Rc::downgrade(player);
        play.connect_clicked(move |button| {
            let Some(player) = player.upgrade() else {
                return;
            };
            let playing = player.is_playing();
            player.set_playing(!playing);
            button.set_icon_name(if playing {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            });
        });
    }
    controls.append(&play);

    // The ticker sets the bar's value programmatically (which does *not* emit
    // `change-value`), so seeking on `change-value` can't feed back on itself.
    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 1.0);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    seek.set_valign(gtk::Align::Center);
    seek.set_size_request(60, -1);
    let seeking = Rc::new(Cell::new(false));
    {
        let player = Rc::downgrade(player);
        let seeking = seeking.clone();
        seek.connect_change_value(move |_, _, value| {
            // Pause the ticker's clock while the user drags the handle.
            seeking.set(true);
            let flag = seeking.clone();
            glib::timeout_add_local_once(Duration::from_millis(200), move || flag.set(false));
            if let Some(player) = player.upgrade() {
                player.seek(value.round() as i64);
            }
            glib::Propagation::Proceed
        });
    }
    controls.append(&seek);

    let time = gtk::Label::new(Some("0:00 / -:--"));
    time.add_css_class("tree-audio-time");
    time.set_valign(gtk::Align::Center);
    time.set_width_chars(13);
    time.set_xalign(1.0);
    controls.append(&time);

    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    volume.set_draw_value(false);
    volume.set_value(player.volume());
    volume.set_valign(gtk::Align::Center);
    volume.set_size_request(56, -1);
    volume.set_tooltip_text(Some("Volume"));
    {
        let player = Rc::downgrade(player);
        volume.connect_value_changed(move |scale| {
            if let Some(player) = player.upgrade() {
                player.set_volume(scale.value());
            }
        });
    }
    controls.append(&volume);

    let widgets = AudioWidgets {
        player: Rc::downgrade(player),
        play,
        seek,
        time,
        seeking,
    };
    (controls, widgets)
}

/// Widest a table cell is allowed to grow before it ellipsizes.
const PREVIEW_CELL_CHARS: i32 = 12;

/// Tallest the inline text box grows before it scrolls internally, in pixels
/// (roughly 18 rows).
const PREVIEW_TEXT_MAX_HEIGHT: i32 = 260;

/// Append a scrollable, selectable monospaced text view for a document (or a
/// structured file's source). The view wraps long lines and is syntax
/// highlighted from the spans cached with the document.
fn append_text_lines(container: &gtk::Box, margin_start: i32, lines: &DocumentLines) {
    if lines.lines.is_empty() {
        return;
    }
    let mut text = lines.lines.join("\n");
    if lines.more {
        text.push_str("\n…");
    }

    let view = gtk::TextView::new();
    view.add_css_class("tree-preview-text");
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_can_focus(true);
    view.set_focus_on_click(true);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(6);
    view.set_right_margin(6);
    view.set_top_margin(3);
    view.set_bottom_margin(3);
    let buffer = view.buffer();
    buffer.set_text(&text);
    apply_syntax(&buffer, &lines.syntax);

    // A small box that scrolls internally rather than growing the row: it is as
    // tall as its content up to the cap, then scrolls. Nested inside the tree's
    // scroller, GTK hands wheel events to the inner view until it hits an edge.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.add_css_class("tree-preview-scroll");
    scrolled.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_max_content_height(PREVIEW_TEXT_MAX_HEIGHT);
    scrolled.set_child(Some(&view));
    scrolled.set_margin_start(margin_start);
    scrolled.set_margin_bottom(6);
    container.append(&scrolled);
}

/// Paint the cached syntax spans onto `buffer` as foreground-colored tags. One
/// tag is created per token class actually used.
fn apply_syntax(buffer: &gtk::TextBuffer, spans: &[Span]) {
    if spans.is_empty() {
        return;
    }
    let palette = highlight::palette();
    let table = buffer.tag_table();
    let mut tags: HashMap<TokenClass, gtk::TextTag> = HashMap::new();
    for span in spans {
        let tag = tags.entry(span.class).or_insert_with(|| {
            let tag = gtk::TextTag::new(None);
            tag.set_foreground(Some(palette.color(span.class)));
            table.add(&tag);
            tag
        });
        let start = buffer.iter_at_offset(span.start as i32);
        let end = buffer.iter_at_offset(span.end as i32);
        buffer.apply_tag(tag, &start, &end);
    }
}

/// Append the validity line under a structured preview.
fn append_status(container: &gtk::Box, margin_start: i32, status: &ParseStatus) {
    if status.detail.is_empty() {
        return;
    }
    let label = gtk::Label::new(Some(&status.detail));
    label.add_css_class("tree-preview-status");
    label.add_css_class(if status.ok { "ok" } else { "error" });
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_wrap(true);
    label.set_wrap_mode(pango::WrapMode::WordChar);
    label.set_margin_start(margin_start);
    label.set_margin_bottom(6);
    container.append(&label);
}

/// Append a CSV/TSV preview as a grid. The first row is styled as a header; the
/// grid shrinks column by column (cells ellipsize) to fit the panel.
fn append_table(container: &gtk::Box, margin_start: i32, table: &TableData) {
    if table.rows.is_empty() {
        return;
    }
    let grid = gtk::Grid::new();
    grid.add_css_class("tree-preview-table");
    grid.set_margin_start(margin_start);
    grid.set_margin_bottom(6);
    grid.set_column_spacing(10);
    grid.set_row_spacing(1);
    for (r, row) in table.rows.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            let label = gtk::Label::new(Some(cell));
            label.set_xalign(0.0);
            label.set_ellipsize(pango::EllipsizeMode::End);
            label.set_max_width_chars(PREVIEW_CELL_CHARS);
            if r == 0 {
                label.add_css_class("tree-preview-header");
            }
            grid.attach(&label, c as i32, r as i32, 1, 1);
        }
    }
    container.append(&grid);
    if table.more {
        let more = gtk::Label::new(Some("…"));
        more.add_css_class("tree-preview-status");
        more.set_xalign(0.0);
        more.set_margin_start(margin_start);
        container.append(&more);
    }
}

/// Append an archive's table of contents (or a one-line summary when its format
/// can't be listed).
fn append_archive(container: &gtk::Box, margin_start: i32, archive: &ArchiveData) {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 1);
    list.add_css_class("tree-preview-archive");
    list.set_margin_start(margin_start);
    list.set_margin_bottom(6);
    match archive {
        ArchiveData::Unsupported(reason) => {
            let label = gtk::Label::new(Some(reason));
            label.add_css_class("tree-preview-status");
            label.set_xalign(0.0);
            label.set_wrap(true);
            list.append(&label);
        }
        ArchiveData::Listed(listing) => {
            for entry in &listing.entries {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let name = gtk::Label::new(Some(&entry.name));
                name.add_css_class("tree-preview-archive-name");
                name.set_xalign(0.0);
                name.set_hexpand(true);
                name.set_ellipsize(pango::EllipsizeMode::Middle);
                row.append(&name);
                if !entry.is_dir {
                    let size = gtk::Label::new(Some(&format_size(entry.size)));
                    size.add_css_class("tree-preview-archive-size");
                    size.set_xalign(1.0);
                    row.append(&size);
                }
                list.append(&row);
            }
            if listing.more {
                let more = gtk::Label::new(Some("…"));
                more.add_css_class("tree-preview-status");
                more.set_xalign(0.0);
                list.append(&more);
            }
        }
    }
    container.append(&list);
}

impl Tree {
    fn open_root(&mut self, path: &Path, sender: &ComponentSender<Self>) {
        if let Some(id) = self.flush_source.take() {
            id.remove();
        }
        self.pending.clear();

        let mut tree = TreeModel::new(
            path.to_path_buf(),
            self.config.sort_options(),
            self.config.show_hidden,
        );
        if let Err(err) = tree.expand(path, &StdDirSource) {
            self.status(format!("Could not open {}: {err}", path.display()), sender);
            return;
        }
        self.tree = Some(tree);
        self.refresh_rows();
        self.cursor = if self.rows.is_empty() { None } else { Some(0) };
        self.selected = Vec::new();
        self.renaming = None;
        self.renaming_state.set(false);
        self.rename_entry = None;
        self.menu_target = None;
        self.thumbnails.clear();
        self.preview_kinds.clear();
        self.documents.clear();
        self.thumb_cache.clear();
        self.playing.clear();
        self.stop_all_videos();
        self.media.clear();
        self.audio.clear();
        self.gif_anims.borrow_mut().clear();
        self.gif_widgets.borrow_mut().clear();
        self.audio_widgets.borrow_mut().clear();
        if let Some(previous) = self.popover.take() {
            previous.unparent();
        }

        let root = path.to_path_buf();
        let watcher_sender = sender.clone();
        self._watcher = spawn_watcher(path, move |change| {
            watcher_sender.input(TreeMsg::FsChange(change));
        })
        .ok();
        self.watched.clear();
        if self._watcher.is_some() {
            self.watched.insert(root.clone());
        }

        let _ = sender.output(TreeOutput::RootChanged(root));
    }

    fn refresh_rows(&mut self) {
        if let Some(tree) = self.tree.as_ref() {
            tree.visible_rows_into(&mut self.rows);
        } else {
            self.rows.clear();
        }
    }

    /// Align the set of watched directories with the model's expanded+loaded
    /// directories. The initial watch is the (non-recursive) root only, so
    /// opening a tree is instant; expanded directories get individual watches
    /// as they are loaded. `unwatch`/`watch` failures are ignored — a stale
    /// watch only costs an auto-update, never correctness.
    fn reconcile_watches(&mut self) {
        let Some(watcher) = self._watcher.as_mut() else {
            return;
        };
        let Some(model) = self.tree.as_ref() else {
            return;
        };
        let desired: HashSet<PathBuf> = model.expanded_loaded_dirs().into_iter().collect();

        let stale: Vec<PathBuf> = self.watched.difference(&desired).cloned().collect();
        for path in stale {
            let _ = watcher.unwatch(&path);
            self.watched.remove(&path);
        }

        let fresh: Vec<PathBuf> = desired.difference(&self.watched).cloned().collect();
        for path in fresh {
            if watcher.watch(&path, RecursiveMode::NonRecursive).is_ok() {
                self.watched.insert(path);
            }
        }
    }

    fn cursor_path(&self) -> Option<PathBuf> {
        self.cursor.and_then(|i| self.rows.get(i)).map(|r| r.path.clone())
    }

    fn cursor_delta(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let next = (self.cursor.unwrap_or(0) as isize + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        let path = self.rows[next].path.clone();
        self.cursor = Some(next);
        self.set_single_selection(path);
        self.selection_dirty = true;
    }

    /// Shift+Arrow selection extension. Moves the cursor `delta` rows and
    /// selects from the anchor to the new cursor.
    ///
    /// The anchor is fixed on the first extension and kept afterwards, so a run
    /// of Shift+Arrows grows a contiguous block; Ctrl+Shift+Arrows behave the
    /// same once an anchor exists, but keep the message distinct for clarity.
    fn cursor_extend(&mut self, delta: isize, _keep_anchor: bool) {
        if self.rows.is_empty() {
            return;
        }
        let anchor = self
            .anchor
            .clone()
            .or_else(|| self.cursor.map(|i| self.rows[i].path.clone()))
            .unwrap_or_else(|| self.rows[0].path.clone());
        let next = (self.cursor.unwrap_or(0) as isize + delta)
            .clamp(0, self.rows.len() as isize - 1) as usize;
        self.cursor = Some(next);
        self.anchor = Some(anchor.clone());
        let (Some(a), Some(b)) = (self.rows.iter().position(|r| r.path == anchor), Some(next)) else {
            return;
        };
        self.selected = self.rows[index_range(a, b)].iter().map(|r| r.path.clone()).collect();
        self.scroll_to = Some(next);
        self.selection_dirty = true;
    }

    fn cursor_snap(&mut self) {
        let deselected = self.cursor.is_none();
        let prev = self.cursor_path();
        self.refresh_rows();
        if self.rows.is_empty() {
            self.cursor = None;
            self.selected.clear();
            return;
        }
        if deselected {
            // Keep the "nothing selected" state across refreshes.
            self.cursor = None;
            return;
        }
        let index = prev
            .and_then(|p| self.rows.iter().position(|r| r.path == p))
            .unwrap_or(0);
        self.cursor = Some(index);
    }

    fn select(&mut self, path: &Path) {
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.cursor = Some(i);
        }
        self.selected = vec![path.to_path_buf()];
    }

    /// The default effect of a plain left press on a row: collapse the
    /// selection onto it, then toggle a directory or open a double-clicked
    /// file. Shared by the immediate press path and the deferred
    /// [`TreeMsg::RowRelease`] path.
    fn press_default(&mut self, path: &Path, is_dir: bool, sender: &ComponentSender<Self>) {
        // Record the press for double-click detection (files only — see below).
        // It lives here because the click gesture is recreated with the rows.
        let now = std::time::Instant::now();
        let is_double = self.last_press.as_ref().is_some_and(|(last, at)| {
            last == path && now.duration_since(*at) < Duration::from_millis(400)
        });
        self.last_press = Some((path.to_path_buf(), now));

        self.select(path);
        self.anchor = Some(path.to_path_buf());
        if is_dir {
            // Directories toggle on *every* press, so they can be expanded and
            // collapsed as fast as the user clicks. Double-click on a directory
            // is not a distinct action.
            self.toggle_dir(path, sender);
        } else if is_double {
            // Files open on a double-click.
            self.activate(path, sender);
        }
    }

    /// Reveal a path forwarded from a launch (`--select` or a file argument).
    /// Selects it, moves the keyboard cursor onto it, and asks the next widget
    /// pass to scroll it into view. A path that is not a visible row (e.g. it
    /// lives outside the current root) is ignored.
    fn reveal_path(&mut self, path: PathBuf) {
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.cursor = Some(i);
            self.selected = vec![path.clone()];
            self.anchor = Some(path);
            self.selection_dirty = true;
            self.scroll_to = Some(i);
        }
    }

    /// Shift+click: select the contiguous run of rows between the current
    /// anchor and `path` (inclusive of both ends). With no anchor yet, this
    /// degrades to a plain single selection.
    fn range_select(&mut self, path: &Path) {
        let Some(anchor) = self.anchor.clone() else {
            self.select(path);
            self.anchor = Some(path.to_path_buf());
            return;
        };
        let (Some(a), Some(b)) = (
            self.rows.iter().position(|r| r.path == anchor),
            self.rows.iter().position(|r| r.path == path),
        ) else {
            self.select(path);
            return;
        };
        self.selected = self.rows[index_range(a, b)].iter().map(|r| r.path.clone()).collect();
        self.cursor = Some(b);
    }

    fn set_single_selection(&mut self, path: PathBuf) {
        self.selected = vec![path.clone()];
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.cursor = Some(i);
        }
    }

    fn selected_for_clipboard(&self) -> Vec<PathBuf> {
        if self.selected.is_empty() {
            if let Some(path) = self.cursor_path() {
                return vec![path];
            }
            return Vec::new();
        }
        self.selected.clone()
    }

    /// Collapse `path` and drop any thumbnails beneath it (non-persistent:
    /// collapsing a directory hides its previews for good).
    fn collapse_dir(&mut self, path: &Path) {
        if let Some(tree) = self.tree.as_mut() {
            tree.collapse(path);
        }
        self.clear_thumbnails_under(path);
        self.cursor_snap();
    }

    /// Expand or collapse `path` depending on its current state.
    fn toggle_dir(&mut self, path: &Path, sender: &ComponentSender<Self>) {
        let expanded = self.tree.as_ref().is_some_and(|t| t.is_expanded(path));
        if expanded {
            self.collapse_dir(path);
        } else {
            self.expand_dir(path, sender);
        }
    }

    /// Pause every playing preview but keep its widgets, so a hidden tree stops
    /// making noise and its previews are still there (paused) when shown again.
    /// Called via [`TreeMsg::Suspend`] when the pane switches to bookmarks, the
    /// panel is hidden, or its workspace is left.
    fn suspend_media(&mut self) {
        for video in self.video_widgets.values() {
            if let Some(stream) = video.media_stream() {
                stream.set_playing(false);
            }
        }
        for player in self.audio.values() {
            player.set_playing(false);
        }
        // Stop the GIF ticker; the frames it owns are rebuilt on demand.
        self.gif_anims.borrow_mut().clear();
        self.playing.clear();
    }

    /// Tear down all media ahead of app exit. Called via [`TreeMsg::Shutdown`]
    /// while the main loop is still running: the queued message drops every
    /// thumbnail, so the following rebuild destroys the `GtkVideo` widgets and
    /// releases their streams, and the cached `GtkMediaFile`s are dropped here.
    /// Without this the process exits with a live GStreamer GL context, whose
    /// context thread then races the driver's at-exit GL teardown.
    fn shutdown(&mut self) {
        if let Some(id) = self.gif_tick.borrow_mut().take() {
            id.remove();
        }
        if let Some(id) = self.audio_tick.borrow_mut().take() {
            id.remove();
        }
        self.gif_anims.borrow_mut().clear();
        self.gif_widgets.borrow_mut().clear();
        self.audio_widgets.borrow_mut().clear();
        self.playing.clear();
        self.thumbnails.clear();
        self.stop_all_videos();
        self.media.clear();
        // Dropping each player returns its pipeline to NULL.
        self.audio.clear();
    }

    /// Forget all preview state for `prefix` and everything under it. Every
    /// resource a preview owns is torn down explicitly here rather than left to
    /// widget destruction: a removed row can stay referenced long enough to keep
    /// a video or audio stream playing.
    fn clear_thumbnails_under(&mut self, prefix: &Path) {
        self.thumbnails.retain(|path| !path.starts_with(prefix));
        self.preview_kinds.retain(|path, _| !path.starts_with(prefix));
        self.documents.retain(|path, _| !path.starts_with(prefix));
        self.thumb_cache.retain(|path, _| !path.starts_with(prefix));
        self.playing.retain(|path| !path.starts_with(prefix));
        self.stop_videos_under(prefix);
        self.media.retain(|path, _| !path.starts_with(prefix));
        self.gif_anims.borrow_mut().retain(|path, _| !path.starts_with(prefix));
        self.gif_widgets.borrow_mut().retain(|path, _| !path.starts_with(prefix));
        self.audio_widgets.borrow_mut().retain(|path, _| !path.starts_with(prefix));
        // Dropping a player returns its pipeline to NULL.
        self.audio.retain(|path, _| !path.starts_with(prefix));
    }

    /// Detach every video stream under `prefix` from its `GtkVideo` and drop the
    /// widget handle. A `GtkMediaFile` is shared, so dropping the cache is not
    /// enough while the widget still references it.
    fn stop_videos_under(&mut self, prefix: &Path) {
        self.video_widgets.retain(|path, video| {
            if path.starts_with(prefix) {
                crate::ui::stop_video_stream(video);
                false
            } else {
                true
            }
        });
    }

    /// Detach every video stream from its widget and drop all handles.
    fn stop_all_videos(&mut self) {
        for video in self.video_widgets.values() {
            crate::ui::stop_video_stream(video);
        }
        self.video_widgets.clear();
    }

    /// Stop and drop every media resource whose file is not in `visible` (the
    /// model's rows about to be rendered). Previews are ephemeral: a file that
    /// is no longer on screen — collapsed, filtered out, replaced by another
    /// view — must not keep decoding or playing. This single choke point makes
    /// teardown independent of *why* a row disappeared.
    fn retain_visible_media(&mut self, visible: &HashSet<PathBuf>) {
        self.video_widgets.retain(|path, video| {
            if visible.contains(path) {
                true
            } else {
                crate::ui::stop_video_stream(video);
                false
            }
        });
        self.media.retain(|path, _| visible.contains(path));
        self.playing.retain(|path| visible.contains(path));
        self.gif_anims.borrow_mut().retain(|path, _| visible.contains(path));
        // Dropping a player returns its pipeline to NULL.
        self.audio.retain(|path, _| visible.contains(path));
    }

    /// Whether `row` should render an inline preview: a file that was turned on
    /// directly, or that sits under a directory that was. Whether the file is
    /// previewable at all is resolved later, from the detection cache.
    fn wants_preview(&self, row: &VisibleRow) -> bool {
        if row.is_dir {
            return false;
        }
        self.thumbnails.contains(&row.path)
            || row
                .path
                .ancestors()
                .skip(1)
                .any(|ancestor| self.thumbnails.contains(ancestor))
    }

    /// The preview kind for `path`, detected once and cached. `None` (cached)
    /// means the file has no inline preview.
    fn preview_kind(&mut self, path: &Path) -> Option<PreviewKind> {
        if let Some(kind) = self.preview_kinds.get(path) {
            return *kind;
        }
        let kind = preview::detect(path);
        self.preview_kinds.insert(path.to_path_buf(), kind);
        kind
    }

    /// The still texture for `path` — an image, or a GIF's first frame —
    /// decoded and cached on first use.
    fn static_texture(&mut self, path: &Path, kind: PreviewKind) -> Option<gdk::Texture> {
        if let Some(texture) = self.thumb_cache.get(path) {
            return Some(texture.clone());
        }
        let texture = match kind {
            PreviewKind::Image => gdk::Texture::from_filename(path).ok()?,
            PreviewKind::Gif => {
                let animation = gdk::gdk_pixbuf::PixbufAnimation::from_file(path).ok()?;
                let pixbuf = animation.static_image()?;
                gdk::Texture::for_pixbuf(&pixbuf)
            }
            // Everything else renders through its own widget, not a texture.
            _ => return None,
        };
        self.thumb_cache.insert(path.to_path_buf(), texture.clone());
        Some(texture)
    }

    /// The cached, looping media stream for a video thumbnail. It starts paused
    /// (the widget controls playback), so nothing is heard until the user
    /// presses play; the built-in volume control handles mute/volume. Audio uses
    /// its own `playbin`-backed player (see [`Tree::audio_player`]).
    fn media_file(&mut self, path: &Path) -> Option<gtk::MediaFile> {
        if let Some(media) = self.media.get(path) {
            return Some(media.clone());
        }
        let media = gtk::MediaFile::for_filename(path);
        media.set_loop(true);
        // PipeWire/WirePlumber remembers per-application stream volume and
        // mute state; a stale "muted at 0" entry (e.g. from a session that
        // pre-muted thumbnails) would otherwise silence every stream. Start
        // from a known unmuted state so playback is actually audible.
        media.set_muted(false);
        media.set_volume(1.0);
        self.media.insert(path.to_path_buf(), media.clone());
        Some(media)
    }

    /// The cached audio player for `path`, created on first use. `None` when the
    /// `audio` feature is disabled (the stub always declines).
    fn audio_player(&mut self, path: &Path) -> Option<Rc<AudioPlayer>> {
        if let Some(player) = self.audio.get(path) {
            return Some(player.clone());
        }
        let player = AudioPlayer::new(path)?;
        self.audio.insert(path.to_path_buf(), player.clone());
        Some(player)
    }

    /// Play/pause an animated GIF thumbnail. (Videos are handled entirely by
    /// `GtkVideo`'s own transport controls, so they never reach here.)
    fn toggle_thumbnail_play(&mut self, path: &Path) {
        if self.preview_kind(path) != Some(PreviewKind::Gif) {
            return;
        }
        if self.playing.remove(path) {
            self.gif_anims.borrow_mut().remove(path);
        } else {
            let Ok(animation) = gdk::gdk_pixbuf::PixbufAnimation::from_file(path) else {
                return;
            };
            let iter = animation.iter(Some(SystemTime::now()));
            self.gif_anims.borrow_mut().insert(path.to_path_buf(), GifAnim { iter });
            self.playing.insert(path.to_path_buf());
            self.ensure_gif_ticker();
        }
    }

    /// Start the GIF ticker if it isn't already running. It advances the frame
    /// iterator and repaints the current `GtkPicture` for each playing GIF,
    /// without rebuilding the row list.
    fn ensure_gif_ticker(&mut self) {
        if self.gif_tick.borrow().is_some() {
            return;
        }
        let anims = self.gif_anims.clone();
        let widgets = self.gif_widgets.clone();
        let tick = self.gif_tick.clone();
        let id = glib::timeout_add_local(Duration::from_millis(33), move || {
            let mut anims = anims.borrow_mut();
            if anims.is_empty() {
                *tick.borrow_mut() = None;
                return glib::ControlFlow::Break;
            }
            let now = SystemTime::now();
            let widgets = widgets.borrow();
            for (path, anim) in anims.iter_mut() {
                let _ = anim.iter.advance(now);
                if let Some(picture) = widgets.get(path) {
                    let pixbuf = anim.iter.pixbuf();
                    picture.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
                }
            }
            glib::ControlFlow::Continue
        });
        *self.gif_tick.borrow_mut() = Some(id);
    }

    /// Start the audio ticker if it isn't already running. It mirrors each
    /// player's stream into its seek bar, clock and play/pause icon.
    fn ensure_audio_ticker(&mut self) {
        if self.audio_tick.borrow().is_some() {
            return;
        }
        let players = self.audio_widgets.clone();
        let tick = self.audio_tick.clone();
        let id = glib::timeout_add_local(Duration::from_millis(200), move || {
            let players = players.borrow();
            if players.is_empty() {
                *tick.borrow_mut() = None;
                return glib::ControlFlow::Break;
            }
            for widgets in players.values() {
                // The player is owned by `Tree::audio`; a dead weak handle means
                // it was discarded, so stop driving these controls.
                let Some(player) = widgets.player.upgrade() else {
                    continue;
                };
                let position = player.position();
                let duration = player.duration();
                if duration > 0
                    && (widgets.seek.adjustment().upper() - duration as f64).abs() > 0.5
                {
                    widgets.seek.adjustment().set_upper(duration as f64);
                }
                if !widgets.seeking.get() {
                    widgets.seek.set_value(position as f64);
                }
                widgets.time.set_text(&format!(
                    "{} / {}",
                    format_time(position),
                    format_time(duration)
                ));
                widgets.play.set_icon_name(if player.is_playing() {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            }
            glib::ControlFlow::Continue
        });
        *self.audio_tick.borrow_mut() = Some(id);
    }

    /// Append the inline preview for `path` below its row. The kind is detected
    /// (and cached) here; media is decoded/played by [`Self::append_media`],
    /// everything else by [`Self::append_document`].
    fn append_preview(
        &mut self,
        path: &Path,
        row_indent: i32,
        container: &gtk::Box,
        sender: &ComponentSender<Self>,
    ) {
        let Some(kind) = self.preview_kind(path) else {
            return;
        };
        let label_start = row_indent + self.config.icon_size as i32 + 6;
        if kind.is_media() {
            let available = self.content_width - label_start - 6;
            self.append_media(path, kind, label_start, available, container, sender);
        } else {
            self.append_document(path, kind, label_start, container);
        }
    }

    /// Append a still image, animated GIF, video or audio player for `path`.
    fn append_media(
        &mut self,
        path: &Path,
        kind: PreviewKind,
        label_start: i32,
        available: i32,
        container: &gtk::Box,
        sender: &ComponentSender<Self>,
    ) {
        match kind {
            PreviewKind::Image => {
                if let Some(texture) = self.static_texture(path, kind) {
                    let (width, height) = thumbnail_size(&texture, available);
                    container.append(&thumbnail_picture(&texture, label_start, width, height));
                }
            }
            PreviewKind::Gif => {
                // While playing, show the current frame; otherwise the first.
                let playing = self.playing.contains(path);
                let texture = if playing {
                    self.gif_anims
                        .borrow()
                        .get(path)
                        .map(|anim| gdk::Texture::for_pixbuf(&anim.iter.pixbuf()))
                } else {
                    self.static_texture(path, kind)
                };
                if let Some(texture) = texture {
                    let (width, height) = thumbnail_size(&texture, available);
                    let picture = thumbnail_picture(&texture, label_start, width, height);
                    picture.set_tooltip_text(Some("Click to play"));
                    if playing {
                        self.gif_widgets
                            .borrow_mut()
                            .insert(path.to_path_buf(), picture.clone());
                    }
                    let msg = TreeMsg::ToggleThumbnailPlay(path.to_path_buf());
                    let s = sender.clone();
                    let click = gtk::GestureClick::new();
                    click.set_button(1);
                    click.connect_pressed(move |_, _, _, _| s.input(msg.clone()));
                    picture.add_controller(click);
                    container.append(&picture);
                }
            }
            PreviewKind::Video => {
                if let Some(media) = self.media_file(path) {
                    // `GtkVideo` brings its own transport controls (play/pause,
                    // seek, mute). Adding our own click gesture here would fire
                    // on clicks aimed at those controls and rebuild the row,
                    // destroying the control before it acts — so leave the
                    // interaction entirely to `GtkVideo`.
                    let video = gtk::Video::new();
                    video.set_autoplay(false);
                    video.set_media_stream(Some(&media));
                    video.set_loop(true);
                    video.add_css_class("tree-thumbnail");
                    video.set_halign(gtk::Align::Start);
                    let width = available.max(64);
                    video.set_size_request(width, (width * 9 / 16).max(36));
                    video.set_margin_start(label_start);
                    video.set_margin_bottom(6);
                    self.video_widgets.insert(path.to_path_buf(), video.clone());
                    container.append(&video);
                }
            }
            PreviewKind::Audio => {
                if let Some(player) = self.audio_player(path) {
                    // Unlike a video, audio has no picture to show, so we build
                    // a compact transport (play/pause, seek, clock, volume) that
                    // the ticker keeps in sync with the stream.
                    let (controls, widgets) = build_audio_player(&player, label_start);
                    self.audio_widgets.borrow_mut().insert(path.to_path_buf(), widgets);
                    container.append(&controls);
                    self.ensure_audio_ticker();
                }
            }
            // Documents are handled by `append_document`.
            PreviewKind::Text
            | PreviewKind::Csv
            | PreviewKind::Json
            | PreviewKind::Toml
            | PreviewKind::Yaml
            | PreviewKind::Archive => {}
        }
    }

    /// Append a text, table, structured-data or archive preview for `path`. The
    /// parsed data is cached, so a rebuild only rebuilds the widgets.
    fn append_document(
        &mut self,
        path: &Path,
        kind: PreviewKind,
        label_start: i32,
        container: &gtk::Box,
    ) {
        if !self.documents.contains_key(path) {
            let data = preview::load_document(path, kind);
            self.documents.insert(path.to_path_buf(), data);
        }
        let Some(Some(data)) = self.documents.get(path) else {
            return;
        };
        match data {
            DocumentData::Lines(lines) => append_text_lines(container, label_start, lines),
            DocumentData::Structured { lines, status } => {
                append_text_lines(container, label_start, lines);
                append_status(container, label_start, status);
            }
            DocumentData::Table(table) => append_table(container, label_start, table),
            DocumentData::Archive(archive) => append_archive(container, label_start, archive),
        }
    }

    fn expand_dir(&mut self, path: &Path, sender: &ComponentSender<Self>) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        if let Err(err) = tree.expand(path, &StdDirSource) {
            self.status(format!("Could not open {}: {err}", path.display()), sender);
        }
        self.refresh_rows();
    }

    fn activate(&mut self, path: &Path, sender: &ComponentSender<Self>) {
        let Some(index) = self.rows.iter().position(|r| r.path == path) else {
            return;
        };
        if self.rows[index].is_dir {
            // "Open" navigates: the directory becomes this pane's root,
            // replacing the current one. Expanding/collapsing stays on the
            // plain left click (and the arrow keys).
            self.open_root(path, sender);
        } else if let Err(err) = open_in_app(path) {
            self.status(err, sender);
        }
    }

    fn confirm_trash(&mut self, paths: Vec<PathBuf>, sender: ComponentSender<Self>) {
        let message = match paths.as_slice() {
            [single] => {
                let name = single
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| single.display().to_string());
                format!("Move \u{201c}{name}\u{201d} to trash?")
            }
            many => format!("Move {} items to trash?", many.len()),
        };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(message)
            .detail("The panel cannot restore trashed items.")
            .buttons(["Cancel", "Trash"])
            .cancel_button(0)
            .default_button(1)
            .build();
        dialog.choose(Some(&self.parent), None::<&gio::Cancellable>, move |res| {
            if matches!(res, Ok(1)) {
                sender.input(TreeMsg::ConfirmTrash(paths));
            }
        });
    }

    fn confirm_permanent_delete(&mut self, paths: Vec<PathBuf>, sender: ComponentSender<Self>) {
        let message = match paths.as_slice() {
            [single] => {
                let name = single
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| single.display().to_string());
                format!("Permanently delete \u{201c}{name}\u{201d}?")
            }
            many => format!("Permanently delete {} items?", many.len()),
        };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(message)
            .detail("This cannot be undone. Files will not go to the trash.")
            .buttons(["Cancel", "Delete Permanently"])
            .cancel_button(0)
            .default_button(1)
            .build();
        dialog.choose(Some(&self.parent), None::<&gio::Cancellable>, move |res| {
            if matches!(res, Ok(1)) {
                sender.input(TreeMsg::ConfirmPermanentDelete(paths));
            }
        });
    }

    fn open_menu(&mut self, path: &Path) {
        // Preserve an existing multi-selection when the right-clicked row is
        // part of it, so the menu can act on all selected rows. Any other
        // right-click collapses onto the clicked row.
        let in_selection = self.selected.len() > 1 && self.selected.iter().any(|p| p == path);
        if !in_selection {
            self.select(path);
        } else if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.cursor = Some(i);
        }
        if let Some(previous) = self.popover.take() {
            previous.unparent();
        }
        self.menu_target = Some(path.to_path_buf());
        self.menu_point = None;
    }

    /// Open the context menu for `path` anchored at the click point `at` (in
    /// `scrolled` coordinates). Used for the open directory, which has no row
    /// of its own to anchor to.
    fn open_menu_at(&mut self, path: &Path, at: (f64, f64)) {
        if let Some(previous) = self.popover.take() {
            previous.unparent();
        }
        self.menu_target = Some(path.to_path_buf());
        self.menu_point = Some(at);
    }

    fn paste(&mut self, sender: &ComponentSender<Self>) {
        let Some(clip) = self.clipboard.clone() else {
            // Nothing copied/cut inside tree-space: fall back to the system
            // clipboard so files copied in another app can be pasted here.
            self.paste_from_system_clipboard(sender);
            return;
        };
        let Some(dir) = self.target_dir() else {
            return;
        };
        let mut count = 0usize;
        let mut failed: Option<String> = None;
        for src in clip.paths {
            if src == dir {
                continue;
            }
            let result = match clip.op {
                ClipboardOp::Copy => self.ops.copy(&src, &dir),
                ClipboardOp::Cut => self.ops.move_(&src, &dir),
            };
            match result {
                Ok(dest) => {
                    count += 1;
                    self.apply_change(Change::Created { path: dest });
                    if clip.op == ClipboardOp::Cut {
                        self.apply_change(Change::Removed { path: src });
                    }
                }
                Err(err) => {
                    failed = Some(err.to_string());
                    break;
                }
            }
        }
        if let Some(failed) = failed {
            self.status(failed, sender);
            return;
        }
        let verb = match clip.op {
            ClipboardOp::Copy => "Copied",
            ClipboardOp::Cut => "Moved",
        };
        self.status(format!("{verb} {count} item(s) into {}", dir.display()), sender);
        if clip.op == ClipboardOp::Cut {
            self.clipboard = None;
        }
        self.cursor_snap();
    }

    /// Paste the system clipboard's file list (if any) into the target
    /// directory as *copies*. Asynchronous: the clipboard read completes on the
    /// main loop and re-enters through [`TreeMsg::PastePaths`].
    fn paste_from_system_clipboard(&mut self, sender: &ComponentSender<Self>) {
        if self.target_dir().is_none() {
            return;
        }
        let s = sender.clone();
        read_clipboard_files(move |paths| s.input(TreeMsg::PastePaths(paths)));
    }

    /// Perform the actual paste of externally-sourced paths as copies.
    fn paste_paths(&mut self, paths: Vec<PathBuf>, sender: &ComponentSender<Self>) {
        let Some(dir) = self.target_dir() else {
            return;
        };
        if paths.is_empty() {
            self.status("Clipboard has no files to paste".to_string(), sender);
            return;
        }
        let mut count = 0usize;
        let mut failed: Option<String> = None;
        for src in paths {
            if src == dir {
                continue;
            }
            match self.ops.copy(&src, &dir) {
                Ok(dest) => {
                    count += 1;
                    self.apply_change(Change::Created { path: dest });
                }
                Err(err) => {
                    failed = Some(err.to_string());
                    break;
                }
            }
        }
        if let Some(failed) = failed {
            self.status(failed, sender);
            return;
        }
        self.status(format!("Pasted {count} item(s) into {}", dir.display()), sender);
        self.cursor_snap();
    }

    /// Route a drag-and-drop onto `target`: copies happen immediately, and
    /// moves either happen immediately or ask first, depending on the
    /// `tree.confirm_drop_move` setting. No-op drops (onto the item itself or
    /// the directory it already lives in) are silently ignored.
    fn request_drop_into(
        &mut self,
        target: PathBuf,
        sources: Vec<PathBuf>,
        copy: bool,
        sender: &ComponentSender<Self>,
    ) {
        match resolve_drop(&target, sources, copy) {
            DropPlan::Ignore => {}
            DropPlan::Reject(message) => self.status(message, sender),
            DropPlan::Copy(sources) => self.drop_into(&target, &sources, true, sender),
            DropPlan::Move(sources) => {
                if self.config.confirm_drop_move {
                    self.confirm_move(target, sources, sender);
                } else {
                    self.drop_into(&target, &sources, false, sender);
                }
            }
        }
    }

    /// Ask before moving a dragged selection into `target`. Only reached when
    /// `tree.confirm_drop_move` is set; copying (Ctrl+drag) is non-destructive
    /// and never asks.
    fn confirm_move(
        &mut self,
        target: PathBuf,
        sources: Vec<PathBuf>,
        sender: &ComponentSender<Self>,
    ) {
        let target_name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.display().to_string());
        let message = match sources.as_slice() {
            [single] => {
                let name = single
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| single.display().to_string());
                format!("Move \u{201c}{name}\u{201d} into \u{201c}{target_name}\u{201d}?")
            }
            many => format!("Move {} items into \u{201c}{target_name}\u{201d}?", many.len()),
        };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(message)
            .detail("Hold Ctrl while dropping to copy instead of move.")
            .buttons(["Cancel", "Move"])
            .cancel_button(0)
            .default_button(1)
            .build();
        let s = sender.clone();
        dialog.choose(Some(&self.parent), None::<&gio::Cancellable>, move |res| {
            if matches!(res, Ok(1)) {
                s.input(TreeMsg::DropIntoConfirmed { target, sources });
            }
        });
    }

    /// Drag-and-drop landed on directory `target`: move (or copy, when
    /// `copy`) every path in `sources` into it. Used for both in-tree drags
    /// and drops of files from other applications.
    fn drop_into(
        &mut self,
        target: &Path,
        sources: &[PathBuf],
        copy: bool,
        sender: &ComponentSender<Self>,
    ) {
        let mut count = 0usize;
        let mut failed: Option<String> = None;
        for src in sources {
            if src == target || src.parent() == Some(target) {
                continue; // dropping onto its own parent/itself is a no-op
            }
            if !copy && target.starts_with(src) {
                failed = Some("Cannot move a folder into itself".to_string());
                break;
            }
            let result = if copy { self.ops.copy(src, target) } else { self.ops.move_(src, target) };
            match result {
                Ok(dest) => {
                    count += 1;
                    self.apply_change(Change::Created { path: dest });
                    if !copy {
                        self.apply_change(Change::Removed { path: src.clone() });
                    }
                }
                Err(err) => {
                    failed = Some(err.to_string());
                    break;
                }
            }
        }
        if let Some(failed) = failed {
            self.status(failed, sender);
            return;
        }
        if count > 0 {
            let verb = if copy { "Copied" } else { "Moved" };
            self.status(format!("{verb} {count} item(s) into {}", target.display()), sender);
        }
        self.cursor_snap();
    }

    fn create_entry(&mut self, is_dir: bool, sender: &ComponentSender<Self>) {
        let Some(dir) = self.target_dir() else {
            return;
        };
        let name = fresh_name(&dir, if is_dir { "new folder" } else { "new file" });
        let result = if is_dir {
            self.ops.create_dir(&dir, &name)
        } else {
            self.ops.create_file(&dir, &name)
        };
        match result {
            Ok(new_path) => {
                self.apply_change(Change::Created { path: new_path.clone() });
                self.renaming = Some(new_path.clone());
                self.renaming_state.set(true);
                self.select(&new_path);
            }
            Err(err) => self.status(format!("Could not create: {err}"), sender),
        }
    }

    /// The directory new items paste into / are created inside: the cursor's
    /// directory itself, or its parent when the cursor is on a file.
    fn target_dir(&self) -> Option<PathBuf> {
        if let Some(path) = self.cursor_path() {
            let is_dir = self.rows.iter().find(|r| r.path == path).map(|r| r.is_dir).unwrap_or(false);
            if is_dir {
                return Some(path);
            }
            return path.parent().map(Path::to_path_buf);
        }
        self.tree.as_ref().map(|t| t.root().to_path_buf())
    }

    fn apply_change(&mut self, change: Change) {
        if let Some(tree) = self.tree.as_mut() {
            tree.apply(&change, &StdDirSource);
            self.refresh_rows();
        }
    }

    /// Toggle dotfile visibility for the current tree, then re-read every
    /// loaded directory so the change takes effect.
    fn toggle_hidden(&mut self, sender: &ComponentSender<Self>) {
        let show = !self.config.show_hidden;
        self.config.show_hidden = show;
        if let Some(tree) = self.tree.as_mut() {
            tree.set_show_hidden(show);
            tree.reload_all(&StdDirSource);
            self.refresh_rows();
        }
        let label = if show { "Showing hidden files." } else { "Hiding hidden files." };
        self.status(label.to_string(), sender);
    }

    /// Change the primary sort key in the current tree.
    fn set_sort_key(&mut self, key: SortKey, sender: &ComponentSender<Self>) {
        let mut opts = self.config.sort_options();
        opts.key = key;
        self.config.sort_key = key.as_str().to_owned();
        if let Some(tree) = self.tree.as_mut() {
            tree.set_sort(opts);
            tree.reload_all(&StdDirSource);
            self.refresh_rows();
        }
        self.status(format!("Sorting by {}.", key.as_str()), sender);
    }

    /// Flip ascending/descending order in the current tree.
    fn toggle_sort_direction(&mut self, sender: &ComponentSender<Self>) {
        let mut opts = self.config.sort_options();
        opts.ascending = !opts.ascending;
        self.config.sort_ascending = opts.ascending;
        if let Some(tree) = self.tree.as_mut() {
            tree.set_sort(opts);
            tree.reload_all(&StdDirSource);
            self.refresh_rows();
        }
        let dir = if opts.ascending { "ascending" } else { "descending" };
        self.status(format!("Sort order: {dir}."), sender);
    }

    fn status(&mut self, message: String, sender: &ComponentSender<Self>) {
        let _ = sender.output(TreeOutput::Status(message));
    }

    /// Execute a configured shortcut against the row under the keyboard
    /// cursor. The selection is aligned with the cursor first so selection
    /// based actions (cut/copy/paste/delete/...) act on that row, exactly as
    /// they would from the context menu.
    fn run_shortcut(&mut self, target: ShortcutTarget, sender: &ComponentSender<Self>) {
        // Pane-level actions (bound to hamburger-menu shortcuts) are not row
        // operations; hand them to the app, which owns docks and panes.
        if let ShortcutTarget::Builtin(builtin) = &target
            && builtin.is_pane_action()
        {
            let _ = sender.output(TreeOutput::PaneAction(*builtin));
            return;
        }
        // With a row selected the action targets it; with nothing selected
        // (the user left-clicked the blank area) directory-targeted actions
        // target the open directory.
        let has_selection = self.cursor.is_some() || !self.selected.is_empty();
        let path = match self.cursor_path() {
            Some(path) => path,
            None => match self.tree.as_ref().map(|t| t.root().to_path_buf()) {
                Some(root) => root,
                None => return,
            },
        };

        // Custom commands operate on the cursor row.
        if let ShortcutTarget::Command(cmd) = &target {
            if !has_selection {
                return;
            }
            let command = action_command(&cmd.command, &path);
            match spawn_command(&command) {
                Ok(()) => {
                    self.status(format!("Ran {} on {}", cmd.command, path.display()), sender);
                }
                Err(err) => {
                    self.status(format!("Could not run {}: {err}", cmd.command), sender);
                }
            }
            return;
        }
        let ShortcutTarget::Builtin(builtin) = target else {
            return;
        };
        if builtin == BuiltinAction::Separator
            || (builtin.is_directory_only() && !path.is_dir())
        {
            return;
        }
        if matches!(builtin, BuiltinAction::OpenWithDefault) && path.is_dir() {
            return;
        }
        if builtin == BuiltinAction::ViewThumbnail
            && !path.is_dir()
            && preview::detect(&path).is_none()
        {
            return;
        }
        // Row-specific actions are meaningless with nothing selected.
        if !has_selection && needs_selection(builtin) {
            return;
        }
        // Align the selection with the cursor, but don't clobber an existing
        // multi-selection the cursor is already part of (so e.g. Delete still
        // acts on every selected row).
        if has_selection && !self.selected.iter().any(|p| p == &path) {
            self.select(&path);
        }
        // With a multi-selection, fan the action out (trash/delete/properties/
        // copy-path act on every selected row), exactly as the context menu does.
        let selection = self.selected.clone();
        sender.input(action_message(builtin, &path, &selection));
    }
}

// ---------------------------------------------------------------------------
// out-of-component helpers
// ---------------------------------------------------------------------------

fn fresh_name(dir: &Path, stem: &str) -> String {
    for n in 0.. {
        let name = if n == 0 { stem.to_string() } else { format!("{stem} {n}") };
        if !dir.join(&name).exists() {
            return name;
        }
    }
    unreachable!("the loop above always finds a free name")
}

/// Open `path` with its registered default application.
///
/// Resolves the app from the file's *content type* instead of going through
/// `launch_default_for_uri`. The latter falls back to the handler for the
/// generic `file:` URI scheme when no app is registered for the type, and
/// tree-space itself is often that handler (it registers `x-scheme-handler/file`
/// so "Show in folder" requests arrive here) — which turned every open of an
/// unassociated file into a new pane. Returns the app's display name.
fn open_in_app(path: &Path) -> Result<String, String> {
    let content_type = content_type_for(path)
        .ok_or_else(|| format!("Could not determine the type of {}", path.display()))?;
    let app = gio::AppInfo::default_for_type(&content_type, false)
        .ok_or_else(|| format!("No application is registered to open {}", path.display()))?;
    let file = gio::File::for_path(path);
    app.launch(&[file], None::<&gio::AppLaunchContext>)
        .map_err(|err| format!("Could not open {}: {err}", path.display()))?;
    Ok(app.display_name().to_string())
}

/// The content type GIO reports for `path` (e.g. `text/plain`), or `None` if the
/// file cannot be queried.
///
/// Deliberately *not* `content_type_guess_for_tree`: that call exists to find a
/// type common to a whole tree and returns an empty list for a single plain
/// file, which silently disabled both "Open With..." and its default-app label.
fn content_type_for(path: &Path) -> Option<glib::GString> {
    let file = gio::File::for_path(path);
    let info = file
        .query_info(
            gio::FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE,
            gio::FileQueryInfoFlags::NONE,
            None::<&gio::Cancellable>,
        )
        .ok()?;
    info.content_type()
}

/// The display name of the default app for `path` (used to render the
/// "Open With {app}" menu item). `None` when the type cannot be determined or no
/// default is registered.
fn default_app_label(path: &Path) -> Option<String> {
    let content_type = content_type_for(path)?;
    gio::AppInfo::default_for_type(&content_type, false).map(|app| {
        format!("Open With {}", app.display_name())
    })
}

/// The "Open With..." chooser: a modal dialog listing every application that
/// can handle the row, plus the option to choose a custom command.
#[allow(deprecated)] // gtk_file_dialog / gio since 4.10; the chooser is still the standard picker
fn open_with_dialog(parent: &gtk::Window, path: &Path, sender: &ComponentSender<Tree>) {
    let Some(content_type) = content_type_for(path) else {
        let _ = sender.output(TreeOutput::Status(format!(
            "Could not determine the type of {}",
            path.display()
        )));
        return;
    };
    let dialog = gtk::AppChooserDialog::for_content_type(
        Some(parent),
        gtk::DialogFlags::MODAL,
        &content_type,
    );
    dialog.set_title(Some(&format!("Open {} with...", path.display())));

    // Show it as a centered overlay layer surface. As a plain toplevel it has
    // no *toplevel* parent to be transient for (the panel is a layer surface),
    // so a tiling compositor would tile it edge-to-edge instead of floating it
    // like a dialog. Without layer-shell support, leave it a normal dialog
    // (transient for the panel) so it still behaves modally.
    if gtk4_layer_shell::is_supported() && !dialog.is_layer_window() {
        dialog.init_layer_shell();
        dialog.set_namespace(Some(crate::ui::LAYER_NAMESPACE));
        dialog.set_layer(Layer::Overlay);
        dialog.set_keyboard_mode(KeyboardMode::Exclusive);
        dialog.set_default_size(540, 620);
        // No anchors: the compositor centers the surface.
    }

    let dialog = dialog.clone();
    let path = path.to_path_buf();
    let s = sender.clone();
    dialog.connect_response(move |dialog, response| {
        if response == gtk::ResponseType::Ok
            && let Some(app) = dialog.app_info()
        {
            let file = gio::File::for_path(&path);
            match app.launch(&[file], None::<&gio::AppLaunchContext>) {
                Ok(()) => {
                    let _ = s.output(TreeOutput::Status(format!(
                        "Opened {} with {}",
                        path.display(),
                        app.display_name()
                    )));
                }
                Err(err) => {
                    let _ = s.output(TreeOutput::Status(format!(
                        "Could not open {} with {}: {err}",
                        path.display(),
                        app.display_name()
                    )));
                }
            }
        }
        dialog.close();
    });
    dialog.present();
}

/// Put `text` on the system clipboard (used by the "Copy Path" menu items).
fn set_clipboard_text(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

/// The standard interop MIME type for a file-list clipboard payload. Read by
/// browsers, terminals and other file managers.
const URI_LIST_MIME: &str = "text/uri-list";

/// The GNOME/Nautilus file-clipboard format, which additionally encodes the
/// operation ("copy"/"cut") on its first line. Nautilus and other GTK apps
/// advertise this alongside `text/uri-list`.
const GNOME_CLIP_MIME: &str = "x-special/gnome-copied-files";

/// Asynchronously read a file-list payload from the system clipboard and call
/// `done` with the decoded absolute paths (empty when the clipboard holds
/// anything else). Prefers the portable `text/uri-list`; falls back to the
/// GNOME format when only that is offered. The read completes on the main loop.
fn read_clipboard_files(done: impl FnOnce(Vec<PathBuf>) + 'static) {
    let Some(display) = gdk::Display::default() else {
        done(Vec::new());
        return;
    };
    let clipboard = display.clipboard();
    let formats = clipboard.formats();
    let has_uri = formats.contain_mime_type(URI_LIST_MIME);
    let has_gnome = formats.contain_mime_type(GNOME_CLIP_MIME);
    if !has_uri && !has_gnome {
        done(Vec::new());
        return;
    }
    clipboard.read_text_async(gio::Cancellable::NONE, move |result| {
        let text = result.ok().flatten().unwrap_or_default();
        done(parse_clipboard_uris(&text, has_gnome && !has_uri));
    });
}

/// Decode paths from a file-list clipboard payload. Accepts both `text/uri-list`
/// (CRLF- or LF-separated, `#` comments, optional blanks) and
/// `x-special/gnome-copied-files` (a leading `copy`/`cut` verb line). `verb_line`
/// selects whether to expect and skip that GNOME preamble. Non-`file:` URIs
/// (e.g. `http:`) and relative entries are dropped. Pure and unit-testable.
fn parse_clipboard_uris(text: &str, verb_line: bool) -> Vec<PathBuf> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            if verb_line && (line.eq_ignore_ascii_case("copy") || line.eq_ignore_ascii_case("cut")) {
                return None;
            }
            let (path, _host) = glib::filename_from_uri(line).ok()?;
            path.is_absolute().then_some(path)
        })
        .collect()
}

fn spawn_command(cmd: &[String]) -> Result<(), std::io::Error> {
    if cmd.is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"));
    }
    let mut process = std::process::Command::new(&cmd[0]);
    for arg in &cmd[1..] {
        process.arg(arg);
    }
    process.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_menu_label_reflects_the_file_kind() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let label = |path: &Path| {
            menu_label(
                &ContextAction::Builtin(BuiltinAction::ViewThumbnail),
                path,
                PanelSide::Left,
            )
        };
        // A picture keeps the configured label; the rest say what they show.
        let png = write("a.png", &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(label(&png), "View Thumbnail");
        let text = write("a.txt", b"hello\n");
        assert_eq!(label(&text), "Show Preview");
        let json = write("a.json", br#"{"a":1}"#);
        assert_eq!(label(&json), "Show Preview");
        // A zip magic signature is enough to classify it as an archive.
        let zip = write("a.zip", b"PK\x03\x04rest");
        assert_eq!(label(&zip), "Show Contents");
        // Directories keep the default label too.
        assert_eq!(label(dir.path()), "View Thumbnail");
    }

    #[test]
    fn thumbnail_width_tracks_the_panel_and_stays_positive() {
        let panel = PanelConfig { width: 300, margin: 0, ..PanelConfig::default() };
        assert_eq!(thumbnail_content_width(panel), 276);

        let panel = PanelConfig { width: 500, margin: 10, ..PanelConfig::default() };
        assert_eq!(thumbnail_content_width(panel), 456);

        let tiny = PanelConfig { width: 40, margin: 0, ..PanelConfig::default() };
        assert_eq!(thumbnail_content_width(tiny), 64);
    }

    #[test]
    fn accelerator_normalization_supports_both_syntaxes() {
        // The friendly `+` form is rewritten to GTK's native angle-bracket form.
        assert_eq!(normalize_accelerator("Ctrl+x"), "<Control>x");
        assert_eq!(normalize_accelerator("Ctrl+Shift+m"), "<Control><Shift>m");
        assert_eq!(normalize_accelerator("Shift+Delete"), "<Shift>Delete");
        assert_eq!(normalize_accelerator("ctrl+alt+t"), "<Control><Alt>t");
        // Native syntax and bare keys pass through untouched.
        assert_eq!(normalize_accelerator("<Control>x"), "<Control>x");
        assert_eq!(normalize_accelerator("F2"), "F2");
        assert_eq!(normalize_accelerator("Delete"), "Delete");
    }

    fn row(path: &str) -> VisibleRow {
        VisibleRow {
            path: PathBuf::from(path),
            name: path.rsplit('/').next().unwrap().to_string(),
            depth: 0,
            is_dir: false,
            is_symlink: false,
            expanded: false,
            has_children: false,
            matches: false,
        }
    }

    #[test]
    fn parse_clipboard_uris_handles_both_formats() {
        // Portable uri-list: CRLF-separated, with a comment and blank line.
        let uri_list = "#comment\r\nfile:///tmp/a.txt\r\n\r\nfile:///tmp/b%20c.txt\r\n";
        assert_eq!(
            parse_clipboard_uris(uri_list, false),
            vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt")]
        );
        // GNOME format: a leading verb line that must be skipped.
        let gnome = "cut\nfile:///home/u/x\n";
        assert_eq!(parse_clipboard_uris(gnome, true), vec![PathBuf::from("/home/u/x")]);
        // A stray verb line without the flag is not treated specially.
        assert_eq!(parse_clipboard_uris("copy\n", false), Vec::<PathBuf>::new());
        // Non-file and relative entries are dropped.
        assert_eq!(
            parse_clipboard_uris("https://example.com/x\nrelative/path\nfile:///ok\n", false),
            vec![PathBuf::from("/ok")]
        );
        assert!(parse_clipboard_uris("", false).is_empty());
    }

    #[test]
    fn icon_name_matches_extensions_case_insensitively() {
        assert_eq!(icon_name(&row("/a/Photo.JPG")), "image-x-generic-symbolic");
        assert_eq!(icon_name(&row("/a/song.Flac")), "audio-x-generic-symbolic");
        assert_eq!(icon_name(&row("/a/clip.mkv")), "video-x-generic-symbolic");
        assert_eq!(icon_name(&row("/a/archive.tar")), "package-x-generic-symbolic");
        assert_eq!(icon_name(&row("/a/doc.pdf")), "application-pdf-symbolic");
        assert_eq!(icon_name(&row("/a/notes.txt")), "text-x-generic-symbolic");
        // A directory named like an image is still a folder.
        let mut dir = row("/a/pictures");
        dir.is_dir = true;
        assert_eq!(icon_name(&dir), "folder-symbolic");
        dir.expanded = true;
        assert_eq!(icon_name(&dir), "folder-open-symbolic");
        // No extension / a dotfile must not panic or misclassify.
        assert_eq!(icon_name(&row("/a/.bashrc")), "text-x-generic-symbolic");
    }

    #[test]
    fn resolve_drop_filters_noops_and_detects_cycles() {
        let target = Path::new("/a/b");
        // Dropping a path onto itself or its own parent is a no-op.
        assert_eq!(resolve_drop(target, vec![PathBuf::from("/a/b")], false), DropPlan::Ignore);
        assert_eq!(resolve_drop(target, vec![PathBuf::from("/a/b/c")], false), DropPlan::Ignore);
        // Moving a directory into itself is rejected.
        assert!(matches!(
            resolve_drop(target, vec![PathBuf::from("/a")], false),
            DropPlan::Reject(_)
        ));
        // A normal move is confirmed; a copy is immediate.
        assert!(matches!(
            resolve_drop(target, vec![PathBuf::from("/x/y")], false),
            DropPlan::Move(_)
        ));
        assert!(matches!(
            resolve_drop(target, vec![PathBuf::from("/x/y")], true),
            DropPlan::Copy(_)
        ));
    }

    #[test]
    fn drag_paths_uses_the_selection_only_when_the_row_is_in_it() {
        let sel = vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")];
        // A row inside a multi-selection drags the whole selection.
        assert_eq!(drag_paths(&sel, Path::new("/r/b")), sel);
        // A row outside it drags only itself.
        assert_eq!(
            drag_paths(&sel, Path::new("/r/c")),
            vec![PathBuf::from("/r/c")]
        );
        // A single-row selection drags just that row.
        assert_eq!(
            drag_paths(&[PathBuf::from("/r/a")], Path::new("/r/a")),
            vec![PathBuf::from("/r/a")]
        );
        // No selection at all still drags the pressed row.
        assert_eq!(
            drag_paths(&[], Path::new("/r/z")),
            vec![PathBuf::from("/r/z")]
        );
    }

    #[test]
    fn index_range_is_inclusive_and_order_independent() {
        assert_eq!(index_range(2, 2).collect::<Vec<_>>(), vec![2]);
        assert_eq!(index_range(1, 4).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        // Selecting upward (anchor below the cursor) yields the same range.
        assert_eq!(index_range(4, 1).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn format_uri_list_serializes_file_uris() {
        let text = format_uri_list(&[PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt")]);
        assert_eq!(text, "file:///tmp/a.txt\r\nfile:///tmp/b%20c.txt\r\n");
        assert!(format_uri_list(&[]).is_empty());
        // Round-trips through the reader used for pastes.
        assert_eq!(
            parse_clipboard_uris(&text, false),
            vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b c.txt")]
        );
    }

    #[test]
    fn typeahead_finds_the_next_matching_row_and_wraps() {
        let rows = vec![row("/r/apple"), row("/r/banana"), row("/r/apricot"), row("/r/cherry")];
        // From the top, "ap" -> apple (index 0); nowhere to go but wrap
        // is only used to *find* the first match from the cursor onward.
        assert_eq!(typeahead_target(&rows, "ap", None), Some(0));
        assert_eq!(typeahead_target(&rows, "ap", Some(0)), Some(2)); // apricot
        assert_eq!(typeahead_target(&rows, "b", Some(2)), Some(1)); // wraps
        assert_eq!(typeahead_target(&rows, "zzz", None), None);
    }

    #[test]
    fn typeahead_buffer_resets_after_a_pause_and_caps_length() {
        let mut ta = TypeAhead::default();
        let t0 = std::time::Instant::now();
        assert_eq!(ta.push('a', t0).as_deref(), Some("a"));
        assert_eq!(ta.push('b', t0 + Duration::from_millis(100)).as_deref(), Some("ab"));
        // A long pause starts a fresh prefix.
        assert_eq!(ta.push('c', t0 + Duration::from_millis(2000)).as_deref(), Some("c"));
        // Non-searchable characters are ignored.
        assert_eq!(ta.push('\u{1}', t0), None);
    }
}