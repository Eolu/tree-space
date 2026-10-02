//! The bookmarks pane view and its editor dialog.
//!
//! Bookmarks are shown as the body of a pane that has no directory yet (a
//! "new panel"): a scrollable list of suggested directories. Clicking an entry
//! tells the pane to jump to that directory, replacing the view. The row's
//! context menu can open, edit, or delete it. The list itself lives in
//! `bookmarks.toml` (see [`crate::config`]); this module only renders it and
//! reports user intent upward as [`BookmarkEvent`]s.
//!
//! Like the other panels the editor dialog is shown as a centered overlay *layer
//! surface*: a plain toplevel has no toplevel parent to be transient for (the
//! panel is itself a layer surface), so a tiling compositor would tile it.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use relm4::gtk;
use relm4::gtk::{gdk, gio, glib, pango, prelude::*};

use crate::config::{
    Bookmark, BuiltinAction, ContextAction, ContextMenu, PanelSide, ShortcutTarget,
    expand_bookmark_path,
};
use crate::ui::tree::{accel_display, begin_row_drag, is_path_safe, menu_label, past_drag_threshold};

/// Where a dragged bookmark was dropped.
#[derive(Debug, Clone)]
pub enum MoveTarget {
    /// Into the folder at this index path (appended).
    Into(Vec<usize>),
    /// At the level of this entry, immediately before it.
    Before(Vec<usize>),
    /// Top level, appended.
    Root,
}

/// Shared drag state for one bookmarks view. Lives on the `Pane` so it survives
/// the row rebuilds that happen on every list change.
#[derive(Default)]
pub struct BookmarkDrag {
    /// Press point (scrolled-window coordinates), set on a primary press.
    start: RefCell<Option<(f64, f64)>>,
    /// The entry pressed: its index path and path (if it is a leaf).
    pressed: RefCell<Option<(Vec<usize>, Option<PathBuf>)>>,
    /// Set once `gdk_drag_begin` has been issued; suppresses further moves and
    /// is cleared when the drag finishes.
    active: Cell<bool>,
    /// The entry currently being dragged, read by the drop handlers.
    dragging: RefCell<Option<Vec<usize>>>,
}

impl BookmarkDrag {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }
}

/// One visible bookmark row, in on-screen order. Built by [`visible_nodes`] and
/// used both to render the rows and to drive keyboard navigation (the cursor is
/// an index into this list).
#[derive(Debug, Clone)]
pub struct BookmarkNode {
    /// Index path into the `bookmarks` tree (folder levels then position).
    pub index_path: Vec<usize>,
    /// Display name.
    pub name: String,
    /// The expanded directory for a leaf (or a clickable folder); `None` for a
    /// pure folder.
    pub path: Option<PathBuf>,
    /// Whether this entry is a folder (holds children or has no path).
    pub folder: bool,
    /// Whether the folder is shown open (either expanded, or forced open while a
    /// filter is active so matching children stay visible).
    pub open: bool,
    /// Indentation depth.
    pub depth: usize,
}

/// Keyboard cursor and selection state for one bookmarks view. Lives on the
/// `Pane` so it survives the row rebuilds that happen on every list change, and
/// is shared with the list's key controller. The focused/selected row mirrors
/// the tree's cursor, so launching the panel hands the keyboard straight to the
/// first bookmark.
pub struct BookmarkNav {
    list: gtk::Box,
    nodes: RefCell<Vec<BookmarkNode>>,
    /// Position into `nodes`, or `None` before the first interaction.
    cursor: Cell<Option<usize>>,
    /// Set when a rebuild should restore keyboard focus to the cursor row (for
    /// example after expand/collapse re-creates it).
    focus_after_fill: Cell<bool>,
}

impl BookmarkNav {
    pub fn new(list: &gtk::Box) -> Rc<Self> {
        Rc::new(Self {
            list: list.clone(),
            nodes: RefCell::new(Vec::new()),
            cursor: Cell::new(None),
            focus_after_fill: Cell::new(false),
        })
    }

    fn len(&self) -> usize {
        self.nodes.borrow().len()
    }

    /// The row widget at `index` (rows are the `.bookmark-row` buttons, in the
    /// same order as `nodes`).
    fn row(&self, index: usize) -> Option<gtk::Widget> {
        let mut child = self.list.first_child();
        let mut seen = 0usize;
        while let Some(current) = child {
            if current.has_css_class("bookmark-row") {
                if seen == index {
                    return Some(current);
                }
                seen += 1;
            }
            child = current.next_sibling();
        }
        None
    }

    /// Record the freshly-built visible rows, keeping the cursor on the same
    /// entry when it is still visible.
    fn set_nodes(&self, nodes: Vec<BookmarkNode>) {
        let prev = self
            .cursor
            .get()
            .and_then(|c| self.nodes.borrow().get(c).map(|n| n.index_path.clone()));
        *self.nodes.borrow_mut() = nodes;
        let len = self.len();
        self.cursor.set(cursor_after_rebuild(prev.as_deref(), &self.nodes.borrow(), len));
    }

    /// Reflect the cursor in the `.bookmark-row-selected` class.
    fn apply_selection(&self) {
        let cursor = self.cursor.get();
        for i in 0..self.len() {
            if let Some(row) = self.row(i) {
                if cursor == Some(i) {
                    row.add_css_class("bookmark-row-selected");
                } else {
                    row.remove_css_class("bookmark-row-selected");
                }
            }
        }
    }

    /// Move the cursor to `index`, style it, and take keyboard focus.
    fn focus_index(&self, index: usize) {
        self.cursor.set(Some(index));
        self.apply_selection();
        if let Some(row) = self.row(index) {
            row.grab_focus();
        }
    }

    /// Move the cursor by `delta` rows (clamped; the first press selects the
    /// first row).
    fn move_by(&self, delta: i32) {
        let len = self.len();
        if let Some(next) = cursor_after_move(self.cursor.get(), delta, len) {
            self.focus_index(next);
        }
    }

    /// Jump to the first or last row.
    fn move_bound(&self, last: bool) {
        let len = self.len();
        if len == 0 {
            return;
        }
        self.focus_index(if last { len - 1 } else { 0 });
    }

    /// The entry under the cursor, if any.
    fn current(&self) -> Option<BookmarkNode> {
        let nodes = self.nodes.borrow();
        self.cursor.get().and_then(|c| nodes.get(c)).cloned()
    }

    /// Focus the first bookmark, or the cursor if it is already placed. Used
    /// when the panel is launched or shown so the keyboard works immediately.
    pub fn focus_start(&self) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let index = self.cursor.get().unwrap_or(0).min(len - 1);
        self.focus_index(index);
    }

    /// Ask the next rebuild to restore keyboard focus to the cursor row.
    fn focus_on_next_fill(&self) {
        self.focus_after_fill.set(true);
    }
}

/// The cursor after moving `delta` rows over `len` entries (clamped).
fn cursor_after_move(cursor: Option<usize>, delta: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let Some(cursor) = cursor else {
        return Some(0);
    };
    Some((cursor as i64 + delta as i64).clamp(0, (len - 1) as i64) as usize)
}

/// The cursor after a rebuild: stay on the entry with the same index path, or
/// fall back to the first row when it is gone.
fn cursor_after_rebuild(path: Option<&[usize]>, nodes: &[BookmarkNode], len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match path {
        Some(path) => nodes.iter().position(|n| n.index_path == path).or(Some(0)),
        None => None,
    }
}

/// What the user did in the bookmarks view.
#[derive(Debug, Clone)]
pub enum BookmarkEvent {
    /// Jump this pane to the bookmark's directory.
    Open(PathBuf),
    /// Expand or collapse the folder at this index path.
    Toggle(Vec<usize>),
    /// Edit the entry at this index path.
    Edit(Vec<usize>),
    /// Delete the entry at this index path.
    Delete(Vec<usize>),
    /// Create a new leaf bookmark (opens the editor).
    NewBookmark,
    /// Create a new empty bookmark folder (opens the editor).
    NewBookmarkFolder,
    /// Run an inherited (directory) context action against `path` without
    /// opening it.
    Action { path: PathBuf, target: ShortcutTarget },
    /// Move the entry at `from` to `to` (drag and drop).
    Move { from: Vec<usize>, to: MoveTarget },
}

/// The config a bookmark's right-click menu is assembled from: the inherited
/// context menu, the extra `[bookmarks] context` items, and the pane side (for
/// labels like "In right panel").
pub struct BookmarkMenuConfig<'a> {
    pub context: &'a ContextMenu,
    pub extras: &'a [ContextAction],
    pub side: PanelSide,
}

/// Replace the contents of `list` with one row per visible bookmark, recursing
/// into folders. `filter` (case-insensitive substring of name or path; empty
/// shows all) keeps a folder when its name or any descendant matches. Used both
/// for the initial view and to refresh every open pane when the list changes.
pub fn fill_bookmarks(
    list: &gtk::Box,
    bookmarks: &[Bookmark],
    filter: &str,
    menu: &BookmarkMenuConfig,
    drag: &Rc<BookmarkDrag>,
    nav: &Rc<BookmarkNav>,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let query = filter.trim().to_lowercase();
    let filtering = !query.is_empty();
    let nodes = if filtering {
        visible_nodes(bookmarks, Some(&query))
    } else {
        visible_nodes(bookmarks, None)
    };
    for node in &nodes {
        list.append(&bookmark_row(node, menu, drag, on_event.clone()));
    }
    nav.set_nodes(nodes);
    nav.apply_selection();
    if nav.focus_after_fill.get() {
        nav.focus_after_fill.set(false);
        nav.focus_start();
    }
    if list.first_child().is_none() {
        let text = if filtering { "No matching bookmarks" } else { "No bookmarks — use Add Bookmark" };
        let empty = gtk::Label::new(Some(text));
        empty.add_css_class("bookmarks-empty");
        empty.set_xalign(0.0);
        list.append(&empty);
    }
}

/// Wire the bookmarks scroller with its blank-area menu and drag-and-drop:
/// right-clicking empty space opens the `[bookmarks] blank` menu, dropping there
/// moves a dragged entry to the top level, and a legacy event controller starts
/// the row drag (GTK gestures do not activate on a layer-shell surface).
pub fn attach_bookmarks_scroller(
    anchor: &gtk::ScrolledWindow,
    blank_items: &[ContextAction],
    side: PanelSide,
    drag: &Rc<BookmarkDrag>,
    nav: &Rc<BookmarkNav>,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) {
    // ── keyboard navigation ─────────────────────────────────────────────────
    // The rows are focusable buttons, but arrow keys do not traverse them on
    // their own; move the cursor and move focus with it. Enter/Space fall
    // through to the focused row's own click handler.
    {
        let nav = nav.clone();
        let on_event = on_event.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            if state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK) {
                return glib::Propagation::Proceed;
            }
            match key {
                gdk::Key::Up => nav.move_by(-1),
                gdk::Key::Down => nav.move_by(1),
                gdk::Key::Home => nav.move_bound(false),
                gdk::Key::End => nav.move_bound(true),
                gdk::Key::Left | gdk::Key::Right => {
                    let Some(node) = nav.current() else {
                        return glib::Propagation::Proceed;
                    };
                    let want_open = key == gdk::Key::Right;
                    if !node.folder || node.open == want_open {
                        return glib::Propagation::Proceed;
                    }
                    // The rebuild re-creates the rows; ask it to restore focus.
                    nav.focus_on_next_fill();
                    on_event(BookmarkEvent::Toggle(node.index_path));
                }
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        anchor.add_controller(keys);
    }

    // ── blank-area context menu ─────────────────────────────────────────────
    {
        let items = blank_items.to_vec();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let menu_anchor = anchor.clone();
        let on_event = on_event.clone();
        gesture.connect_pressed(move |gesture, _, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let popover = gtk::Popover::new();
            popover.add_css_class("bookmark-menu-popover");
            popover.set_has_arrow(true);
            let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            menu_box.add_css_class("bookmark-menu");
            append_bookmark_items(&menu_box, &items, None, &[], side, on_event.clone());
            popover.set_child(Some(&menu_box));
            popover.set_parent(&menu_anchor);
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            let popover2 = popover.clone();
            popover.connect_closed(move |_| popover2.unparent());
            popover.popup();
        });
        anchor.add_controller(gesture);
    }

    // ── drop on empty space: move the dragged entry to the top level ────────
    {
        let drag = drag.clone();
        let on_event = on_event.clone();
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::MOVE);
        drop.connect_drop(move |_, _value, _x, _y| {
            let Some(from) = drag.dragging.borrow().clone() else {
                return false;
            };
            on_event(BookmarkEvent::Move { from, to: MoveTarget::Root });
            true
        });
        anchor.add_controller(drop);
    }

    // ── drag start (legacy controller; see `begin_row_drag`) ────────────────
    {
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        let d = drag.clone();
        let scrolled = anchor.clone();
        legacy.connect_event(move |_, event| {
            match event.event_type() {
                gdk::EventType::ButtonPress => {
                    let button = event
                        .downcast_ref::<gdk::ButtonEvent>()
                        .map(|b| b.button())
                        .unwrap_or(0);
                    if button == 1 && let Some(pos) = event.position() {
                        *d.start.borrow_mut() = Some(pos);
                    }
                }
                gdk::EventType::ButtonRelease => {
                    let button = event
                        .downcast_ref::<gdk::ButtonEvent>()
                        .map(|b| b.button())
                        .unwrap_or(0);
                    if button == 1 && !d.active.get() {
                        *d.start.borrow_mut() = None;
                        *d.pressed.borrow_mut() = None;
                    }
                }
                gdk::EventType::MotionNotify => {
                    if d.active.get() {
                        return glib::Propagation::Proceed;
                    }
                    let Some(start) = *d.start.borrow() else {
                        return glib::Propagation::Proceed;
                    };
                    let Some(now) = event.position() else {
                        return glib::Propagation::Proceed;
                    };
                    if !past_drag_threshold(scrolled.upcast_ref(), start, now) {
                        return glib::Propagation::Proceed;
                    }
                    let Some((from, path)) = d.pressed.borrow().clone() else {
                        return glib::Propagation::Proceed;
                    };
                    *d.start.borrow_mut() = None;
                    let paths: Vec<PathBuf> = path.into_iter().collect();
                    let finished = {
                        let d = d.clone();
                        move |_drag: gdk::Drag, _delete: bool| {
                            d.active.set(false);
                            *d.dragging.borrow_mut() = None;
                        }
                    };
                    let cancelled = {
                        let d = d.clone();
                        move |_drag: gdk::Drag, _reason: gdk::DragCancelReason| {
                            d.active.set(false);
                            *d.dragging.borrow_mut() = None;
                        }
                    };
                    if begin_row_drag(
                        scrolled.upcast_ref(),
                        &paths,
                        start,
                        gdk::DragAction::MOVE,
                        finished,
                        cancelled,
                    ) {
                        d.active.set(true);
                        *d.dragging.borrow_mut() = Some(from);
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
        anchor.add_controller(legacy);
    }
}

/// Whether `bookmark` matches a lowercase `query` (already trimmed). Matches the
/// name or the path so a directory can be found by either.
fn bookmark_matches(bookmark: &Bookmark, query: &str) -> bool {
    query.is_empty()
        || bookmark.name.to_lowercase().contains(query)
        || bookmark
            .path
            .as_deref()
            .is_some_and(|path| path.to_string_lossy().to_lowercase().contains(query))
}

/// Flatten the bookmark tree into the rows that should be visible, in order.
/// Without a filter, a folder's children show only when it is expanded. With a
/// filter, a matching folder shows all of its children; a folder that does not
/// match is shown only when a descendant matches, and then only the matching
/// subtree.
pub fn visible_nodes(bookmarks: &[Bookmark], query: Option<&str>) -> Vec<BookmarkNode> {
    let mut out = Vec::new();
    walk_nodes(bookmarks, &[], 0, query, &mut out);
    out
}

fn walk_nodes(
    entries: &[Bookmark],
    prefix: &[usize],
    depth: usize,
    query: Option<&str>,
    out: &mut Vec<BookmarkNode>,
) {
    for (i, entry) in entries.iter().enumerate() {
        let mut index_path = prefix.to_vec();
        index_path.push(i);
        let folder = entry.is_folder();
        match query {
            None => {
                out.push(node_of(entry, index_path.clone(), depth, folder, entry.expanded));
                if folder && entry.expanded {
                    walk_nodes(&entry.items, &index_path, depth + 1, None, out);
                }
            }
            Some(query) => {
                if folder {
                    if bookmark_matches(entry, query) {
                        out.push(node_of(entry, index_path.clone(), depth, true, true));
                        walk_all(&entry.items, &index_path, depth + 1, out);
                    } else {
                        // Keep the folder only if one of its descendants survives
                        // the filter; put the folder row ahead of that subtree.
                        let start = out.len();
                        walk_nodes(&entry.items, &index_path, depth + 1, Some(query), out);
                        if out.len() > start {
                            out.insert(start, node_of(entry, index_path, depth, true, true));
                        }
                    }
                } else if bookmark_matches(entry, query) {
                    out.push(node_of(entry, index_path, depth, false, false));
                }
            }
        }
    }
}

/// Every entry of `entries`, recursing through folders (used when a folder
/// matches the filter and all of its contents should be shown).
fn walk_all(entries: &[Bookmark], prefix: &[usize], depth: usize, out: &mut Vec<BookmarkNode>) {
    for (i, entry) in entries.iter().enumerate() {
        let mut index_path = prefix.to_vec();
        index_path.push(i);
        let folder = entry.is_folder();
        out.push(node_of(entry, index_path.clone(), depth, folder, true));
        if folder {
            walk_all(&entry.items, &index_path, depth + 1, out);
        }
    }
}

fn node_of(entry: &Bookmark, index_path: Vec<usize>, depth: usize, folder: bool, open: bool) -> BookmarkNode {
    BookmarkNode {
        index_path,
        name: entry.name.clone(),
        path: entry.path.as_deref().map(expand_bookmark_path),
        folder,
        open,
        depth,
    }
}

/// One bookmark row. A folder toggles expand/collapse on click; a leaf opens.
/// Right-clicking opens the inherited directory menu (for a path) plus the
/// `[bookmarks] context` extras.
fn bookmark_row(
    node: &BookmarkNode,
    menu: &BookmarkMenuConfig,
    drag: &Rc<BookmarkDrag>,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) -> gtk::Widget {
    let folder = node.folder;
    let path = node.path.clone();
    let index_path = &node.index_path;
    let row = gtk::Button::new();
    row.add_css_class("bookmark-row");
    if let Some(path) = &path {
        row.set_tooltip_text(Some(&path.display().to_string()));
    }

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_margin_start((node.depth * 14) as i32);
    if folder {
        let arrow = gtk::Image::from_icon_name(if node.open {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        });
        arrow.add_css_class("bookmark-arrow");
        content.append(&arrow);
    } else {
        // Align leaves with folder labels (the arrow column).
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_size_request(16, -1);
        content.append(&spacer);
    }
    // A folder gets a folder icon; a leaf a bookmark, so the two kinds read
    // apart from ordinary file rows.
    let icon = gtk::Image::from_icon_name(if folder {
        "folder-symbolic"
    } else {
        "user-bookmarks-symbolic"
    });
    icon.add_css_class("bookmark-icon");
    let label = gtk::Label::new(Some(&node.name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.add_css_class("bookmark-label");
    content.append(&icon);
    content.append(&label);
    row.set_child(Some(&content));

    {
        let on_event = on_event.clone();
        if folder {
            let index_path = index_path.to_vec();
            row.connect_clicked(move |_| on_event(BookmarkEvent::Toggle(index_path.clone())));
        } else if let Some(path) = path.clone() {
            row.connect_clicked(move |_| on_event(BookmarkEvent::Open(path.clone())));
        }
    }

    // ── right-click: inherited directory menu + bookmark extras ─────────────
    let actions = build_bookmark_actions(path.as_deref(), menu);
    if !actions.is_empty() {
        let popover = gtk::Popover::new();
        popover.add_css_class("bookmark-menu-popover");
        popover.set_has_arrow(true);
        let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        menu_box.add_css_class("bookmark-menu");
        append_bookmark_items(&menu_box, &actions, path.as_deref(), index_path, menu.side, on_event.clone());
        popover.set_child(Some(&menu_box));

        popover.set_parent(&row);
        // The row is rebuilt whenever the list changes; unparent the popover as
        // the row goes away so GTK does not warn about children left behind.
        {
            let popover = popover.clone();
            row.connect_destroy(move |_| popover.unparent());
        }
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        {
            let popover = popover.clone();
            gesture.connect_pressed(move |gesture, _, x, y| {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            });
        }
        row.add_controller(gesture);
    }

    // ── drag: record the pressed entry, and accept drops ────────────────────
    // The press only records state; the actual `gdk_drag_begin` is issued by the
    // scroller's legacy controller (gestures do not activate on a layer
    // surface). A folder accepts a drop "into"; a leaf accepts one "before" it.
    {
        let drag = drag.clone();
        let index_path = index_path.to_vec();
        let path = path.clone();
        let press = gtk::GestureClick::new();
        press.set_button(gdk::BUTTON_PRIMARY);
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        press.connect_pressed(move |_, _, _, _| {
            *drag.pressed.borrow_mut() = Some((index_path.clone(), path.clone()));
        });
        row.add_controller(press);
    }
    {
        let drag = drag.clone();
        let index_path = index_path.to_vec();
        let target_path = index_path.clone();
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::MOVE);
        drop.connect_drop(move |_, _value, _x, _y| {
            let Some(from) = drag.dragging.borrow().clone() else {
                return false;
            };
            // Claim a drop on the dragged row itself so the scroller's
            // top-level target does not fire; otherwise do the move.
            if from == index_path {
                return true;
            }
            let to = if folder {
                MoveTarget::Into(index_path.clone())
            } else {
                MoveTarget::Before(target_path.clone())
            };
            on_event(BookmarkEvent::Move { from, to });
            true
        });
        row.add_controller(drop);
    }

    row.upcast()
}

/// The full right-click menu for a bookmark: the context menu its directory
/// would get in the tree (when it has a path), reduced to actions that make
/// sense on a bare path, followed by the `[bookmarks] context` extras.
fn build_bookmark_actions(path: Option<&Path>, menu: &BookmarkMenuConfig) -> Vec<ContextAction> {
    let mut actions: Vec<ContextAction> = match path {
        Some(path) => menu.context.actions_for(path).into_iter().filter(action_allowed).collect(),
        None => Vec::new(),
    };
    actions.extend(menu.extras.iter().filter(|a| action_allowed(a)).cloned());
    trim_separators(&mut actions);
    actions
}

/// Drop separators that would lead or trail the menu (e.g. a path-less folder
/// whose only inherited section is empty).
fn trim_separators(actions: &mut Vec<ContextAction>) {
    while actions.first().is_some_and(is_separator) {
        actions.remove(0);
    }
    while actions.last().is_some_and(is_separator) {
        actions.pop();
    }
}

/// Whether an item is a menu divider.
fn is_separator(action: &ContextAction) -> bool {
    builtin_of(action) == Some(BuiltinAction::Separator)
}

/// Whether a configured item can appear in a bookmark's menu. Inherited
/// actions that need a tree row (new file/folder, cut/copy/paste, thumbnails)
/// are dropped; bookmark-only actions and separators are always kept.
fn action_allowed(action: &ContextAction) -> bool {
    match action {
        ContextAction::Command(_) => true,
        ContextAction::Submenu(sub) => sub.items.iter().any(action_allowed),
        ContextAction::Builtin(b) => {
            *b == BuiltinAction::Separator || is_path_safe(*b) || b.is_bookmark_only()
        }
        ContextAction::Entry(e) => is_path_safe(e.action) || e.action.is_bookmark_only(),
    }
}

/// Append `actions` (recursively, including submenus) to a bookmark menu box.
fn append_bookmark_items(
    menu_box: &gtk::Box,
    actions: &[ContextAction],
    path: Option<&Path>,
    index_path: &[usize],
    side: PanelSide,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) {
    for action in actions {
        if action.is_hidden() {
            continue;
        }
        if builtin_of(action) == Some(BuiltinAction::Separator) {
            let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
            separator.add_css_class("tree-menu-separator");
            menu_box.append(&separator);
            continue;
        }

        // A submenu row opens a nested popover.
        if let ContextAction::Submenu(sub) = action {
            let button = submenu_row_button(&sub.label);
            let popover = build_bookmark_submenu(&sub.items, path, index_path, side, on_event.clone());
            let child = popover.clone();
            let anchor = button.clone();
            button.connect_clicked(move |_| {
                child.set_parent(&anchor);
                child.popup();
            });
            let child = popover.clone();
            popover.connect_closed(move |_| child.unparent());
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row_box.add_css_class("tree-menu-item");
            row_box.append(&button);
            row_box.append(&submenu_arrow());
            menu_box.append(&row_box);
            continue;
        }

        let Some(target) = ShortcutTarget::from_action(action) else {
            continue;
        };
        let Some(event) = bookmark_event(target, path, index_path) else {
            continue;
        };

        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row_box.add_css_class("tree-menu-item");
        let label = gtk::Label::new(Some(&menu_label(action, path.unwrap_or(Path::new("")), side)));
        label.set_xalign(0.0);
        label.set_hexpand(true);
        let button = gtk::Button::new();
        button.set_halign(gtk::Align::Fill);
        button.set_hexpand(true);
        button.set_child(Some(&label));
        {
            let on_event = on_event.clone();
            button.connect_clicked(move |_| on_event(event.clone()));
        }
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

/// The event a menu `target` produces: bookmark-only actions edit/delete the
/// entry; everything else runs against the target path (which a path-less
/// folder never offers, hence `None`).
fn bookmark_event(
    target: ShortcutTarget,
    path: Option<&Path>,
    index_path: &[usize],
) -> Option<BookmarkEvent> {
    match &target {
        ShortcutTarget::Builtin(BuiltinAction::EditBookmark) => {
            Some(BookmarkEvent::Edit(index_path.to_vec()))
        }
        ShortcutTarget::Builtin(BuiltinAction::DeleteBookmark) => {
            Some(BookmarkEvent::Delete(index_path.to_vec()))
        }
        ShortcutTarget::Builtin(BuiltinAction::NewBookmark) => {
            Some(BookmarkEvent::NewBookmark)
        }
        ShortcutTarget::Builtin(BuiltinAction::NewBookmarkFolder) => {
            Some(BookmarkEvent::NewBookmarkFolder)
        }
        _ => path.map(|path| BookmarkEvent::Action {
            path: path.to_path_buf(),
            target,
        }),
    }
}

/// Build a nested popover for a bookmark submenu.
fn build_bookmark_submenu(
    items: &[ContextAction],
    path: Option<&Path>,
    index_path: &[usize],
    side: PanelSide,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) -> gtk::Popover {
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu_box.add_css_class("bookmark-menu");
    append_bookmark_items(&menu_box, items, path, index_path, side, on_event);
    let popover = gtk::Popover::new();
    popover.add_css_class("bookmark-menu-popover");
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Right);
    popover.set_child(Some(&menu_box));
    popover
}

/// The builtin behind an item, if any (mirrors the tree's own helper).
fn builtin_of(action: &ContextAction) -> Option<BuiltinAction> {
    match action {
        ContextAction::Builtin(b) => Some(*b),
        ContextAction::Entry(e) => Some(e.action),
        _ => None,
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

/// Show the bookmark editor / creation dialog. `title` is the window title
/// ("Edit Bookmark", "New Folder", ...). A folder (`path` is `None`) edits just
/// the name; a leaf edits the name and path (with a folder picker). `on_save`
/// receives the resolved name and, for a leaf, the path.
pub fn show_bookmark_dialog(
    parent: &gtk::Window,
    title: &str,
    name: &str,
    path: Option<&Path>,
    on_save: impl Fn(String, Option<PathBuf>) + 'static,
) {
    let folder = path.is_none();
    let window = gtk::Window::new();
    window.set_title(Some(title));
    window.set_default_size(420, -1);
    window.set_transient_for(Some(parent));
    if gtk4_layer_shell::is_supported() && !window.is_layer_window() {
        window.init_layer_shell();
        window.set_namespace(Some(crate::ui::LAYER_NAMESPACE));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
    }

    let form = gtk::Box::new(gtk::Orientation::Vertical, 8);
    form.add_css_class("bookmark-editor");

    let name_label = gtk::Label::new(Some("Name"));
    name_label.set_xalign(0.0);
    name_label.add_css_class("bookmark-field-label");
    let name_entry = gtk::Entry::new();
    name_entry.set_text(name);
    name_entry.set_hexpand(true);

    let path_label = gtk::Label::new(Some("Path"));
    path_label.set_xalign(0.0);
    path_label.add_css_class("bookmark-field-label");
    let path_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let path_entry = gtk::Entry::new();
    path_entry.set_text(&path.map(|p| p.display().to_string()).unwrap_or_default());
    path_entry.set_hexpand(true);
    let browse = gtk::Button::with_label("Browse...");
    path_row.append(&path_entry);
    path_row.append(&browse);

    // A folder has no path of its own.
    path_label.set_visible(!folder);
    path_row.set_visible(!folder);

    form.append(&name_label);
    form.append(&name_entry);
    form.append(&path_label);
    form.append(&path_row);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    footer.set_halign(gtk::Align::End);
    footer.add_css_class("bookmark-editor-footer");
    let cancel = gtk::Button::with_label("Cancel");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    footer.append(&cancel);
    footer.append(&save);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("bookmark-editor-root");
    root.append(&form);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&footer);
    window.set_child(Some(&root));

    {
        let path_entry = path_entry.clone();
        let window = window.clone();
        browse.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::new();
            let path_entry = path_entry.clone();
            dialog.select_folder(Some(&window), None::<&gio::Cancellable>, move |res| {
                if let Ok(file) = res
                    && let Some(p) = file.path()
                {
                    path_entry.set_text(&p.display().to_string());
                }
            });
        });
    }

    // Validate and dispatch. A folder needs only a name; a leaf needs a path (a
    // blank name falls back to the directory's name). A blank field is rejected,
    // leaving the dialog open.
    let submit: Rc<dyn Fn()> = {
        let name_entry = name_entry.clone();
        let path_entry = path_entry.clone();
        let window = window.clone();
        let on_save = Rc::new(on_save);
        Rc::new(move || {
            let typed = name_entry.text().trim().to_string();
            if folder {
                if typed.is_empty() {
                    return;
                }
                on_save(typed, None);
                window.close();
                return;
            }
            let raw = path_entry.text().trim().to_string();
            if raw.is_empty() {
                return;
            }
            let path = expand_bookmark_path(Path::new(&raw));
            let name = if typed.is_empty() { Bookmark::default_name(&path) } else { typed };
            on_save(name, Some(path));
            window.close();
        })
    };

    {
        let submit = submit.clone();
        save.connect_clicked(move |_| submit());
    }
    {
        let submit = submit.clone();
        name_entry.connect_activate(move |_| submit());
    }
    {
        let submit = submit.clone();
        path_entry.connect_activate(move |_| submit());
    }
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let keys = gtk::EventControllerKey::new();
        window.add_controller(keys.clone());
        let window = window.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    window.present();
}

#[cfg(test)]
mod tests {
    use super::{bookmark_matches, cursor_after_move, cursor_after_rebuild, visible_nodes, BookmarkNode};
    use crate::config::Bookmark;
    use std::path::PathBuf;

    fn bm(name: &str, path: &str) -> Bookmark {
        Bookmark::leaf(name.to_owned(), PathBuf::from(path))
    }

    fn folder(name: &str, expanded: bool, items: Vec<Bookmark>) -> Bookmark {
        Bookmark {
            name: name.to_owned(),
            path: None,
            items,
            expanded,
        }
    }

    #[test]
    fn filter_matches_name_or_path_case_insensitively() {
        let b = bm("Projects", "/home/me/Projects");
        // The caller lowercases the query; an empty one matches everything.
        assert!(bookmark_matches(&b, ""));
        assert!(bookmark_matches(&b, "proj"));
        assert!(bookmark_matches(&b, "/home/me"));
        assert!(!bookmark_matches(&b, "photos"));
    }

    #[test]
    fn visible_nodes_respects_expansion() {
        let list = vec![
            bm("Home", "/home/me"),
            folder("Work", false, vec![bm("Repo", "/srv/repo")]),
            folder("Open", true, vec![bm("Doc", "/srv/doc")]),
        ];
        let nodes = visible_nodes(&list, None);
        // A collapsed folder contributes only itself; an expanded one lists its
        // child at the next depth.
        let shape: Vec<(Vec<usize>, usize)> =
            nodes.iter().map(|n| (n.index_path.clone(), n.depth)).collect();
        assert_eq!(
            shape,
            vec![
                (vec![0], 0),
                (vec![1], 0),
                (vec![2], 0),
                (vec![2, 0], 1),
            ]
        );
        assert!(nodes[1].folder && !nodes[1].open);
        assert!(!nodes[3].folder && nodes[3].depth == 1);
    }

    #[test]
    fn visible_nodes_filter_keeps_a_folder_only_via_a_descendant() {
        let list = vec![folder(
            "Work",
            false,
            vec![bm("Design", "/srv/design"), bm("Code", "/srv/code")],
        )];
        let nodes = visible_nodes(&list, Some("design"));
        // The folder is kept (forced open) with only the matching child.
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].index_path, vec![0]);
        assert!(nodes[0].folder && nodes[0].open);
        assert_eq!(nodes[1].index_path, vec![0, 0]);
        assert_eq!(nodes[1].name, "Design");
    }

    #[test]
    fn visible_nodes_filter_on_a_folder_shows_all_children() {
        let list = vec![folder(
            "Work",
            false,
            vec![bm("Design", "/srv/design"), bm("Code", "/srv/code")],
        )];
        let nodes = visible_nodes(&list, Some("work"));
        assert_eq!(nodes.len(), 3);
        assert!(nodes[0].open);
        assert_eq!(nodes[1].name, "Design");
        assert_eq!(nodes[2].name, "Code");
    }

    #[test]
    fn cursor_after_move_clamps_and_starts_at_the_top() {
        assert_eq!(cursor_after_move(None, 1, 3), Some(0));
        assert_eq!(cursor_after_move(None, -1, 3), Some(0));
        assert_eq!(cursor_after_move(Some(0), -1, 3), Some(0));
        assert_eq!(cursor_after_move(Some(2), 1, 3), Some(2));
        assert_eq!(cursor_after_move(Some(1), 1, 3), Some(2));
        assert_eq!(cursor_after_move(Some(1), -1, 3), Some(0));
        assert_eq!(cursor_after_move(Some(0), 1, 0), None);
    }

    #[test]
    fn cursor_after_rebuild_sticks_to_the_same_entry() {
        let node = |path: &[usize]| BookmarkNode {
            index_path: path.to_vec(),
            name: String::new(),
            path: None,
            folder: false,
            open: false,
            depth: 0,
        };
        let nodes = vec![node(&[0]), node(&[1]), node(&[1, 0])];
        assert_eq!(cursor_after_rebuild(Some(&[1, 0]), &nodes, nodes.len()), Some(2));
        // A vanished entry falls back to the first row.
        assert_eq!(cursor_after_rebuild(Some(&[9]), &nodes, nodes.len()), Some(0));
        assert_eq!(cursor_after_rebuild(None, &nodes, nodes.len()), None);
        assert_eq!(cursor_after_rebuild(Some(&[0]), &[], 0), None);
    }
}
