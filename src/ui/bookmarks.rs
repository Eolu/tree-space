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

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use relm4::gtk;
use relm4::gtk::{gdk, gio, glib, pango, prelude::*};

use crate::config::{
    Bookmark, BuiltinAction, ContextAction, ContextMenu, PanelSide, ShortcutTarget,
    expand_bookmark_path,
};
use crate::ui::tree::{accel_display, is_path_safe, menu_label};

/// What the user did in the bookmarks view.
#[derive(Debug, Clone)]
pub enum BookmarkEvent {
    /// Jump this pane to the bookmark's directory.
    Open(PathBuf),
    /// Edit the bookmark at this index in the list.
    Edit(usize),
    /// Delete the bookmark at this index in the list.
    Delete(usize),
    /// Run an inherited (directory) context action against `path` without
    /// opening it.
    Action { path: PathBuf, target: ShortcutTarget },
}

/// The config a bookmark's right-click menu is assembled from: the inherited
/// context menu, the extra `[bookmarks] context` items, and the pane side (for
/// labels like "In right panel").
pub struct BookmarkMenuConfig<'a> {
    pub context: &'a ContextMenu,
    pub extras: &'a [ContextAction],
    pub side: PanelSide,
}

/// Replace the contents of `list` with a row per bookmark that matches `filter`
/// (case-insensitive substring of the name or path; empty shows all). Used both
/// for the initial view and to refresh every open pane when the list changes.
pub fn fill_bookmarks(
    list: &gtk::Box,
    bookmarks: &[Bookmark],
    filter: &str,
    menu: &BookmarkMenuConfig,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let query = filter.trim().to_lowercase();
    let visible: Vec<(usize, &Bookmark)> = bookmarks
        .iter()
        .enumerate()
        .filter(|(_, bookmark)| bookmark_matches(bookmark, &query))
        .collect();
    if visible.is_empty() {
        let text = if query.is_empty() {
            "No bookmarks — use Add Bookmark"
        } else {
            "No matching bookmarks"
        };
        let empty = gtk::Label::new(Some(text));
        empty.add_css_class("bookmarks-empty");
        empty.set_xalign(0.0);
        list.append(&empty);
        return;
    }
    for (index, bookmark) in visible {
        list.append(&bookmark_row(index, bookmark, menu, on_event.clone()));
    }
}

/// Whether `bookmark` matches a lowercase `query` (already trimmed). Matches the
/// name or the path so a directory can be found by either.
fn bookmark_matches(bookmark: &Bookmark, query: &str) -> bool {
    query.is_empty()
        || bookmark.name.to_lowercase().contains(query)
        || bookmark.path.to_string_lossy().to_lowercase().contains(query)
}

/// One bookmark row: an icon and its label, clickable to open, with a
/// right-click menu (the inherited directory menu plus `[bookmarks] context`).
fn bookmark_row(
    index: usize,
    bookmark: &Bookmark,
    menu: &BookmarkMenuConfig,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) -> gtk::Widget {
    let path = expand_bookmark_path(&bookmark.path);
    let row = gtk::Button::new();
    row.add_css_class("bookmark-row");
    row.set_tooltip_text(Some(&path.display().to_string()));

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    // Deliberately not a folder icon, so bookmarks read as a distinct kind.
    let icon = gtk::Image::from_icon_name("user-bookmarks-symbolic");
    icon.add_css_class("bookmark-icon");
    let label = gtk::Label::new(Some(&bookmark.name));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.add_css_class("bookmark-label");
    content.append(&icon);
    content.append(&label);
    row.set_child(Some(&content));

    {
        let on_event = on_event.clone();
        let path = path.clone();
        row.connect_clicked(move |_| on_event(BookmarkEvent::Open(path.clone())));
    }

    // ── right-click: inherited directory menu + bookmark extras ─────────────
    let popover = gtk::Popover::new();
    popover.add_css_class("bookmark-menu-popover");
    popover.set_has_arrow(true);
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu_box.add_css_class("bookmark-menu");
    let actions = build_bookmark_actions(&path, menu);
    append_bookmark_items(&menu_box, &actions, &path, index, menu.side, on_event);
    popover.set_child(Some(&menu_box));

    popover.set_parent(&row);
    // The row is rebuilt whenever the list changes; unparent the popover as the
    // row goes away so GTK does not warn about a widget with children left.
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

    row.upcast()
}

/// The full right-click menu for the bookmark at `path`: the context menu the
/// directory would get in the tree, reduced to actions that make sense on a
/// bare path, followed by the `[bookmarks] context` extras.
fn build_bookmark_actions(path: &Path, menu: &BookmarkMenuConfig) -> Vec<ContextAction> {
    let mut actions: Vec<ContextAction> = menu
        .context
        .actions_for(path)
        .into_iter()
        .filter(action_allowed)
        .collect();
    actions.extend(menu.extras.iter().filter(|a| action_allowed(a)).cloned());
    actions
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
    path: &Path,
    index: usize,
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
            let popover = build_bookmark_submenu(&sub.items, path, index, side, on_event.clone());
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
        let event = bookmark_event(target, path, index);

        let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row_box.add_css_class("tree-menu-item");
        let label = gtk::Label::new(Some(&menu_label(action, path, side)));
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
/// entry; everything else runs against the target path.
fn bookmark_event(target: ShortcutTarget, path: &Path, index: usize) -> BookmarkEvent {
    match &target {
        ShortcutTarget::Builtin(BuiltinAction::EditBookmark) => BookmarkEvent::Edit(index),
        ShortcutTarget::Builtin(BuiltinAction::DeleteBookmark) => BookmarkEvent::Delete(index),
        _ => BookmarkEvent::Action { path: path.to_path_buf(), target },
    }
}

/// Build a nested popover for a bookmark submenu.
fn build_bookmark_submenu(
    items: &[ContextAction],
    path: &Path,
    index: usize,
    side: PanelSide,
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) -> gtk::Popover {
    let menu_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu_box.add_css_class("bookmark-menu");
    append_bookmark_items(&menu_box, items, path, index, side, on_event);
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

/// Show the editor for a bookmark: an editable name and path (with a folder
/// picker). `on_save` receives the resolved name and path when the user saves.
pub fn show_bookmark_editor(
    parent: &gtk::Window,
    name: &str,
    path: &Path,
    on_save: impl Fn(String, PathBuf) + 'static,
) {
    let window = gtk::Window::new();
    window.set_title(Some("Edit Bookmark"));
    window.set_default_size(420, -1);
    window.set_transient_for(Some(parent));
    if gtk4_layer_shell::is_supported() && !window.is_layer_window() {
        window.init_layer_shell();
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
    path_entry.set_text(&path.display().to_string());
    path_entry.set_hexpand(true);
    let browse = gtk::Button::with_label("Browse...");
    path_row.append(&path_entry);
    path_row.append(&browse);

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

    // Validate and dispatch. A blank name falls back to the directory's name; a
    // blank path is rejected (leaving the dialog open).
    let submit: Rc<dyn Fn()> = {
        let name_entry = name_entry.clone();
        let path_entry = path_entry.clone();
        let window = window.clone();
        let on_save = Rc::new(on_save);
        Rc::new(move || {
            let raw = path_entry.text().trim().to_string();
            if raw.is_empty() {
                return;
            }
            let path = expand_bookmark_path(Path::new(&raw));
            let typed = name_entry.text().trim().to_string();
            let name = if typed.is_empty() { Bookmark::default_name(&path) } else { typed };
            on_save(name, path);
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
    use super::bookmark_matches;
    use crate::config::Bookmark;
    use std::path::PathBuf;

    fn bm(name: &str, path: &str) -> Bookmark {
        Bookmark { name: name.to_owned(), path: PathBuf::from(path) }
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
}
