//! The bookmarks section and its editor dialog.
//!
//! The bookmarks list is a strip that sits above or below a dock's pane stack
//! (never a sidebar): a one-line header with an add button over a short,
//! scrollable list. Clicking an entry opens a pane at that directory; the row's
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

use crate::config::{Bookmark, expand_bookmark_path};

/// What the user did in the bookmarks section.
#[derive(Debug)]
pub enum BookmarkEvent {
    /// Open a new pane at the bookmark's directory.
    Open(PathBuf),
    /// Edit the bookmark at this index in the list.
    Edit(usize),
    /// Delete the bookmark at this index in the list.
    Delete(usize),
    /// Hide the bookmarks strip (the close button; same as the toggle action).
    Hide,
}

/// Build the bookmarks section. Rebuilt whenever the list, its visibility, or a
/// dock changes, so it holds no state of its own. There is no header bar — the
/// list is the whole strip with a small close button floating top-right;
/// bookmarks are added from a directory's context menu.
pub fn bookmarks_section(
    bookmarks: &[Bookmark],
    on_event: Rc<dyn Fn(BookmarkEvent)>,
) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("bookmarks");

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("bookmarks-list");
    if bookmarks.is_empty() {
        let empty = gtk::Label::new(Some("No bookmarks — use Add Bookmark"));
        empty.add_css_class("bookmarks-empty");
        empty.set_xalign(0.0);
        list.append(&empty);
    }
    for (index, bookmark) in bookmarks.iter().enumerate() {
        list.append(&bookmark_row(index, bookmark, on_event.clone()));
    }

    // Keep a long list from squeezing the panes: scroll within a bounded height
    // and ask for only as much as the entries need.
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(220);
    scroller.set_child(Some(&list));

    // The close button floats over the top-right corner; the list reserves a
    // little right padding (in CSS) so the rows never run underneath it.
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&scroller));
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("bookmarks-close");
    close.set_tooltip_text(Some("Hide bookmarks"));
    close.set_halign(gtk::Align::End);
    close.set_valign(gtk::Align::Start);
    {
        let on_event = on_event.clone();
        close.connect_clicked(move |_| on_event(BookmarkEvent::Hide));
    }
    overlay.add_overlay(&close);
    root.append(&overlay);

    root.upcast()
}

/// One bookmark row: an icon and its label, clickable to open, with a
/// right-click menu (Open / Edit / Delete).
fn bookmark_row(index: usize, bookmark: &Bookmark, on_event: Rc<dyn Fn(BookmarkEvent)>) -> gtk::Widget {
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

    let popover = gtk::Popover::new();
    popover.add_css_class("bookmark-menu-popover");
    popover.set_has_arrow(true);
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    menu.add_css_class("bookmark-menu");
    let open_item = menu_button("Open");
    let edit_item = menu_button("Edit...");
    let delete_item = menu_button("Delete");
    menu.append(&open_item);
    menu.append(&edit_item);
    menu.append(&delete_item);
    popover.set_child(Some(&menu));

    {
        let on_event = on_event.clone();
        let popover = popover.clone();
        let path = path.clone();
        open_item.connect_clicked(move |_| {
            popover.popdown();
            on_event(BookmarkEvent::Open(path.clone()));
        });
    }
    {
        let on_event = on_event.clone();
        let popover = popover.clone();
        edit_item.connect_clicked(move |_| {
            popover.popdown();
            on_event(BookmarkEvent::Edit(index));
        });
    }
    {
        let on_event = on_event.clone();
        let popover = popover.clone();
        delete_item.connect_clicked(move |_| {
            popover.popdown();
            on_event(BookmarkEvent::Delete(index));
        });
    }

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

/// A flat, full-width, left-aligned button for a popover menu.
fn menu_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("flat");
    button.set_halign(gtk::Align::Fill);
    if let Some(child) = button.child().and_downcast::<gtk::Label>() {
        child.set_xalign(0.0);
    }
    button
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
