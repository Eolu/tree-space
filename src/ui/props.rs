//! The Properties dialog.
//!
//! Mirrors what Nautilus shows for a single path: a **Basic** tab (name, type,
//! size/contents, location, free space and timestamps), a **Permissions** tab
//! (owner/group and the classic read/write/execute matrix, applied as you
//! change them) and, for images, a small **Image** tab with the pixel
//! dimensions. Metadata comes from [`crate::fs::meta`]; this module is only the
//! presentation and the wiring of edits back to the filesystem.
//!
//! Like the "Open With..." chooser, the dialog is shown as a centered overlay
//! *layer surface*: a plain toplevel has no toplevel parent to be transient for
//! (the panel is itself a layer surface), so Hyprland would tile it instead of
//! floating it.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use relm4::gtk::{gdk, glib, pango, prelude::*};
use relm4::prelude::*;

use crate::fs::meta::{self, Access, FileMeta, Permissions};
use crate::ui::tree::{Tree, TreeMsg, TreeOutput};

/// Show the Properties dialog for `path`, applying edits live (as Nautilus
/// does) and reporting failures in its own status line.
pub fn show_properties_dialog(parent: &gtk::Window, path: &Path, sender: &ComponentSender<Tree>) {
    let Ok(meta) = meta::read_meta(path) else {
        let _ = sender.output(TreeOutput::Status(format!(
            "Could not read properties of {}",
            path.display()
        )));
        return;
    };

    let window = gtk::Window::new();
    window.set_title(Some(&format!("Properties — {}", meta.name)));
    window.set_default_size(500, 600);
    window.set_transient_for(Some(parent));
    // Keep it centered and modal-like, and let entries/combos take keys. Only
    // possible with layer-shell support; otherwise it stays a normal transient
    // window (the `set_transient_for` above keeps it on top of the panel).
    if gtk4_layer_shell::is_supported() && !window.is_layer_window() {
        window.init_layer_shell();
        window.set_namespace(Some(crate::ui::LAYER_NAMESPACE));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
    }

    let status = gtk::Label::new(None);
    status.add_css_class("props-status");
    status.set_hexpand(true);
    status.set_xalign(0.0);
    status.set_ellipsize(pango::EllipsizeMode::End);
    let set_status: Rc<dyn Fn(String)> = {
        let status = status.clone();
        Rc::new(move |message| status.set_text(&message))
    };

    // ── header ──────────────────────────────────────────────────────────────
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    header.add_css_class("props-header");
    let icon = gtk::Image::from_icon_name(icon_for(&meta));
    icon.set_pixel_size(28);
    let title = gtk::Label::new(Some(&meta.name));
    title.add_css_class("props-title");
    title.set_ellipsize(pango::EllipsizeMode::Middle);
    title.set_xalign(0.0);
    title.set_hexpand(true);
    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    header.append(&icon);
    header.append(&title);
    header.append(&close_button);

    // ── tabs ────────────────────────────────────────────────────────────────
    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    // The pages must not force the window wider than we asked for: long paths
    // and the permission controls would otherwise grow the surface.
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_width(false);
    scroller.set_propagate_natural_height(false);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&notebook));

    let basic = basic_tab(&meta);
    notebook.append_page(&basic.page, Some(&gtk::Label::new(Some("Basic"))));
    let permissions = permissions_tab(&meta, set_status.clone());
    notebook.append_page(&permissions, Some(&gtk::Label::new(Some("Permissions"))));
    if !meta.is_dir
        && let Some((width, height)) = meta::image_dimensions(&meta.path)
    {
        let image = image_tab(&meta, width, height);
        notebook.append_page(&image, Some(&gtk::Label::new(Some("Image"))));
    }

    // ── footer ──────────────────────────────────────────────────────────────
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    footer.add_css_class("props-footer");
    footer.append(&status);
    let done = gtk::Button::with_label("Close");
    done.add_css_class("suggested-action");
    footer.append(&done);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("props");
    root.append(&header);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&scroller);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&footer);
    window.set_child(Some(&root));

    // ── actions ─────────────────────────────────────────────────────────────
    {
        let window = window.clone();
        close_button.connect_clicked(move |_| window.close());
    }
    {
        let window = window.clone();
        done.connect_clicked(move |_| window.close());
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

    // Renaming from the editable Name field: send it to the tree (so the model,
    // watcher and selection all update) and close on success. Invalid names are
    // rejected in place.
    let name_entry = basic.name_entry.clone();
    {
        let sender = sender.clone();
        let set_status = set_status.clone();
        let window = window.clone();
        let path = meta.path.clone();
        name_entry.connect_activate(move |entry| {
            let name = entry.text().trim().to_string();
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                set_status("A name cannot be empty, contain '/', or be '.' or '..'".to_string());
                return;
            }
            if name == meta.name {
                return;
            }
            sender.input(TreeMsg::RenameTo { path: path.clone(), name });
            window.close();
        });
    }

    window.present();
}

/// Show a summary Properties dialog for a multi-row selection: how many items,
/// their combined size, the shared parent folder, and the list of names. Unlike
/// the single-path dialog it is read-only — there is no sensible bulk
/// permissions/name editor here.
pub fn show_multi_properties_dialog(parent: &gtk::Window, paths: &[std::path::PathBuf]) {
    let count = paths.len();

    // Combined size, and whether every item shares one parent folder.
    let mut total: u64 = 0;
    let mut unknown_size = false;
    for path in paths {
        if path.is_dir() {
            unknown_size = true;
        } else {
            match std::fs::metadata(path) {
                Ok(meta) => total += meta.len(),
                Err(_) => unknown_size = true,
            }
        }
    }
    let parents: std::collections::BTreeSet<_> =
        paths.iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    let shared_parent = if parents.len() == 1 {
        parents.into_iter().next().map(|p| p.display().to_string())
    } else {
        None
    };

    let window = gtk::Window::new();
    window.set_title(Some(&format!("Properties — {count} items")));
    window.set_default_size(500, 600);
    window.set_transient_for(Some(parent));
    if gtk4_layer_shell::is_supported() && !window.is_layer_window() {
        window.init_layer_shell();
        window.set_namespace(Some(crate::ui::LAYER_NAMESPACE));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
    }

    // ── header ──────────────────────────────────────────────────────────────
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    header.add_css_class("props-header");
    let icon = gtk::Image::from_icon_name("folder-symbolic");
    icon.set_pixel_size(28);
    let title = gtk::Label::new(Some(&format!("{count} items selected")));
    title.add_css_class("props-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    let close_button = gtk::Button::from_icon_name("window-close-symbolic");
    close_button.add_css_class("flat");
    header.append(&icon);
    header.append(&title);
    header.append(&close_button);

    // ── summary ─────────────────────────────────────────────────────────────
    let grid = grid();
    add_row(&grid, 0, "Items", &value_label(&count.to_string()));
    let size_text = if unknown_size && total == 0 {
        "—".to_string()
    } else if unknown_size {
        format!("{} (some folders not counted)", meta::format_size(total))
    } else {
        meta::format_size(total)
    };
    add_row(&grid, 1, "Total size", &value_label(&size_text));
    if let Some(parent_dir) = &shared_parent {
        add_row(&grid, 2, "Parent folder", &value_label(parent_dir));
    }

    // ── the list of names ───────────────────────────────────────────────────
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("props-multi-list");
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let label = gtk::Label::new(Some(&name));
        label.set_xalign(0.0);
        label.set_ellipsize(pango::EllipsizeMode::Middle);
        label.set_tooltip_text(Some(&path.to_string_lossy()));
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&label));
        row.set_tooltip_text(Some(&path.to_string_lossy()));
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&list));

    // ── footer ──────────────────────────────────────────────────────────────
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    footer.add_css_class("props-footer");
    let spacer = gtk::Label::new(None);
    spacer.set_hexpand(true);
    footer.append(&spacer);
    let done = gtk::Button::with_label("Close");
    done.add_css_class("suggested-action");
    footer.append(&done);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("props");
    root.append(&header);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&grid);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&scroller);
    root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&footer);
    window.set_child(Some(&root));

    {
        let window = window.clone();
        close_button.connect_clicked(move |_| window.close());
    }
    {
        let window = window.clone();
        done.connect_clicked(move |_| window.close());
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

/// The Basic tab plus the widget the caller needs afterwards.
struct BasicTab {
    page: gtk::Widget,
    name_entry: gtk::Entry,
}

/// Name, type, size/contents, link target, parent, free space and timestamps.
fn basic_tab(meta: &FileMeta) -> BasicTab {
    let grid = grid();

    let name_entry = gtk::Entry::new();
    name_entry.set_text(&meta.name);
    name_entry.add_css_class("props-name");
    add_row(&grid, 0, "Name", &name_entry);

    let mut row = 1;
    add_row(&grid, row, "Type", &value_label(&type_text(meta)));
    row += 1;

    if meta.is_dir {
        if let Some(count) = meta.item_count() {
            add_row(&grid, row, "Size", &value_label(&meta::item_count_label(count)));
            row += 1;
        }
    } else {
        add_row(&grid, row, "Size", &value_label(&meta::format_size(meta.size)));
        row += 1;
    }

    if let Some(target) = &meta.link_target {
        add_row(&grid, row, "Link target", &value_label(&target.display().to_string()));
        row += 1;
    }

    if let Some(parent) = meta.path.parent() {
        add_row(&grid, row, "Parent folder", &value_label(&parent.display().to_string()));
        row += 1;
    }

    if let Some(free) = meta::available_space(&meta.path) {
        add_row(&grid, row, "Free space", &value_label(&meta::format_size(free)));
        row += 1;
    }

    for (label, time) in [
        ("Accessed", meta.accessed),
        ("Modified", meta.modified),
        ("Created", meta.created),
    ] {
        if let Some(time) = time {
            add_row(&grid, row, label, &value_label(&format_time(time)));
            row += 1;
        }
    }

    BasicTab { page: grid.upcast(), name_entry }
}

/// Owner/group plus the read/write/execute matrix and the special bits. Every
/// control applies its change immediately and reverts on failure.
fn permissions_tab(meta: &FileMeta, set_status: Rc<dyn Fn(String)>) -> gtk::Widget {
    let grid = grid();

    // Owner and group lists, guaranteeing the current ids are present.
    let mut users = meta::list_users();
    if !users.iter().any(|(id, _)| *id == meta.uid) {
        users.push((meta.uid, meta.owner.clone()));
        users.sort_by_key(|(id, _)| *id);
    }
    let mut groups = meta::list_groups();
    if !groups.iter().any(|(id, _)| *id == meta.gid) {
        groups.push((meta.gid, meta.group.clone()));
        groups.sort_by_key(|(id, _)| *id);
    }
    let users = Rc::new(users);
    let groups = Rc::new(groups);

    let owner = dropdown(&users, meta.uid);
    let group = dropdown(&groups, meta.gid);
    add_row(&grid, 0, "Owner", &owner);
    add_row(&grid, 1, "Group", &group);

    // Access matrix. Checkboxes rather than dropdowns because a `GtkDropDown`
    // refuses to shrink below its widest item, which made the whole dialog
    // wide; the three-bit matrix is also easier to scan.
    let owner_access = AccessRow::new(meta.permissions.owner);
    let group_access = AccessRow::new(meta.permissions.group);
    let other_access = AccessRow::new(meta.permissions.other);
    let access = gtk::Grid::new();
    access.add_css_class("props-access");
    access.set_row_spacing(4);
    access.set_column_spacing(20);
    for (column, text) in ["", "Read", "Write", "Execute"].iter().enumerate() {
        let label = gtk::Label::new(Some(text));
        label.add_css_class("props-label");
        label.set_xalign(0.0);
        access.attach(&label, column as i32, 0, 1, 1);
    }
    for (row, (name, triad)) in [
        ("Owner", &owner_access),
        ("Group", &group_access),
        ("Others", &other_access),
    ]
    .iter()
    .enumerate()
    {
        let label = gtk::Label::new(Some(name));
        label.add_css_class("props-label");
        label.set_xalign(1.0);
        access.attach(&label, 0, row as i32 + 1, 1, 1);
        access.attach(&triad.read, 1, row as i32 + 1, 1, 1);
        access.attach(&triad.write, 2, row as i32 + 1, 1, 1);
        access.attach(&triad.execute, 3, row as i32 + 1, 1, 1);
    }
    add_row(&grid, 2, "Access", &access);

    let chk_uid = gtk::CheckButton::with_label("Set user ID");
    let chk_gid = gtk::CheckButton::with_label("Set group ID");
    let chk_sticky = gtk::CheckButton::with_label("Sticky");
    chk_uid.set_active(meta.permissions.set_uid);
    chk_gid.set_active(meta.permissions.set_gid);
    chk_sticky.set_active(meta.permissions.sticky);
    let special = gtk::Box::new(gtk::Orientation::Vertical, 2);
    special.add_css_class("props-special");
    special.append(&chk_uid);
    special.append(&chk_gid);
    special.append(&chk_sticky);
    add_row(&grid, 3, "Special", &special);

    let path = meta.path.clone();
    let state = Rc::new(RefCell::new(PermState {
        permissions: meta.permissions,
        uid: meta.uid,
        gid: meta.gid,
        suppress: false,
    }));

    // Put every control back to the last known-good state.
    let revert: Rc<dyn Fn()> = {
        let state = state.clone();
        let owner = owner.clone();
        let group = group.clone();
        let owner_access = owner_access.clone();
        let group_access = group_access.clone();
        let other_access = other_access.clone();
        let chk_uid = chk_uid.clone();
        let chk_gid = chk_gid.clone();
        let chk_sticky = chk_sticky.clone();
        let users = users.clone();
        let groups = groups.clone();
        Rc::new(move || {
            let known = *state.borrow();
            state.borrow_mut().suppress = true;
            owner_access.set(known.permissions.owner);
            group_access.set(known.permissions.group);
            other_access.set(known.permissions.other);
            chk_uid.set_active(known.permissions.set_uid);
            chk_gid.set_active(known.permissions.set_gid);
            chk_sticky.set_active(known.permissions.sticky);
            owner.set_selected(index_of(&users, known.uid));
            group.set_selected(index_of(&groups, known.gid));
            state.borrow_mut().suppress = false;
        })
    };

    // Apply the access matrix plus the special bits.
    let apply_permissions: Rc<dyn Fn()> = {
        let state = state.clone();
        let path = path.clone();
        let set_status = set_status.clone();
        let revert = revert.clone();
        let owner_access = owner_access.clone();
        let group_access = group_access.clone();
        let other_access = other_access.clone();
        let chk_uid = chk_uid.clone();
        let chk_gid = chk_gid.clone();
        let chk_sticky = chk_sticky.clone();
        Rc::new(move || {
            if state.borrow().suppress {
                return;
            }
            let permissions = Permissions {
                owner: owner_access.access(),
                group: group_access.access(),
                other: other_access.access(),
                set_uid: chk_uid.is_active(),
                set_gid: chk_gid.is_active(),
                sticky: chk_sticky.is_active(),
            };
            match meta::set_permissions(&path, permissions) {
                Ok(()) => {
                    state.borrow_mut().permissions = permissions;
                    set_status("Permissions changed".to_string());
                }
                Err(err) => {
                    set_status(format!("Could not change permissions: {err}"));
                    revert();
                }
            }
        })
    };
    owner_access.connect(apply_permissions.clone());
    group_access.connect(apply_permissions.clone());
    other_access.connect(apply_permissions.clone());
    for check in [&chk_uid, &chk_gid, &chk_sticky] {
        let apply = apply_permissions.clone();
        check.connect_toggled(move |_| apply());
    }

    // Change owner (uid only) / group (gid only).
    for (combo, entries, is_owner) in [
        (owner.clone(), users.clone(), true),
        (group.clone(), groups.clone(), false),
    ] {
        let state = state.clone();
        let path = path.clone();
        let set_status = set_status.clone();
        let revert = revert.clone();
        combo.connect_selected_notify(move |combo| {
            if state.borrow().suppress {
                return;
            }
            let index = combo.selected() as usize;
            let Some((id, name)) = entries.get(index) else {
                return;
            };
            let result = if is_owner {
                meta::set_owner_uid(&path, *id)
            } else {
                meta::set_group_gid(&path, *id)
            };
            match result {
                Ok(()) => {
                    if is_owner {
                        state.borrow_mut().uid = *id;
                        set_status(format!("Owner changed to {name}"));
                    } else {
                        state.borrow_mut().gid = *id;
                        set_status(format!("Group changed to {name}"));
                    }
                }
                Err(err) => {
                    let what = if is_owner { "owner" } else { "group" };
                    set_status(format!("Could not change {what}: {err}"));
                    revert();
                }
            }
        });
    }

    grid.upcast()
}

/// The three checkboxes that make up one triad's access level.
#[derive(Clone)]
struct AccessRow {
    read: gtk::CheckButton,
    write: gtk::CheckButton,
    execute: gtk::CheckButton,
}

impl AccessRow {
    fn new(access: Access) -> Self {
        let row = AccessRow {
            read: gtk::CheckButton::new(),
            write: gtk::CheckButton::new(),
            execute: gtk::CheckButton::new(),
        };
        row.set(access);
        row
    }

    /// The triad currently ticked.
    fn access(&self) -> Access {
        let bits = u32::from(self.read.is_active()) << 2
            | u32::from(self.write.is_active()) << 1
            | u32::from(self.execute.is_active());
        Access::from_bits(bits)
    }

    fn set(&self, access: Access) {
        let bits = access.bits();
        self.read.set_active(bits & 0b100 != 0);
        self.write.set_active(bits & 0b010 != 0);
        self.execute.set_active(bits & 0b001 != 0);
    }

    /// Run `apply` whenever any of the three boxes is toggled.
    fn connect(&self, apply: Rc<dyn Fn()>) {
        for check in [&self.read, &self.write, &self.execute] {
            let apply = apply.clone();
            check.connect_toggled(move |_| apply());
        }
    }
}

/// The Image tab: type and pixel dimensions.
fn image_tab(meta: &FileMeta, width: u32, height: u32) -> gtk::Widget {
    let grid = grid();
    add_row(&grid, 0, "Type", &value_label(&type_text(meta)));
    add_row(
        &grid,
        1,
        "Dimensions",
        &value_label(&format!("{width} × {height} pixels")),
    );
    grid.upcast()
}

/// Mutable state shared by the permission controls.
#[derive(Debug, Clone, Copy)]
struct PermState {
    permissions: Permissions,
    uid: u32,
    gid: u32,
    /// Set while the controls are being populated/reverted, so programmatic
    /// changes do not fire an apply.
    suppress: bool,
}

fn grid() -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.add_css_class("props-grid");
    grid.set_row_spacing(8);
    grid.set_column_spacing(14);
    grid.set_margin_top(14);
    grid.set_margin_bottom(14);
    grid.set_margin_start(16);
    grid.set_margin_end(16);
    grid
}

/// Attach a right-aligned label in column 0 and `value` in column 1.
fn add_row(grid: &gtk::Grid, row: i32, label: &str, value: &impl IsA<gtk::Widget>) {
    let label = gtk::Label::new(Some(label));
    label.add_css_class("props-label");
    label.set_xalign(1.0);
    label.set_valign(gtk::Align::Start);
    grid.attach(&label, 0, row, 1, 1);
    value.set_hexpand(true);
    grid.attach(value, 1, row, 1, 1);
}

/// A read-only, selectable value label that truncates long paths in the middle.
fn value_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("props-value");
    label.set_xalign(0.0);
    label.set_selectable(true);
    label.set_ellipsize(pango::EllipsizeMode::Middle);
    label.set_max_width_chars(42);
    label
}

/// A searchable dropdown over `(id, name)` entries, selecting `id`.
fn dropdown(entries: &[(u32, String)], id: u32) -> gtk::DropDown {
    let names: Vec<&str> = entries.iter().map(|(_, name)| name.as_str()).collect();
    let combo = gtk::DropDown::from_strings(&names);
    combo.set_enable_search(true);
    combo.set_selected(index_of(entries, id));
    combo
}

fn index_of(entries: &[(u32, String)], id: u32) -> u32 {
    entries
        .iter()
        .position(|(candidate, _)| *candidate == id)
        .map(|index| index as u32)
        .unwrap_or(0)
}

/// "PNG image (image/png)", "Folder (inode/directory)", or the raw MIME type.
fn type_text(meta: &FileMeta) -> String {
    match (&meta.type_description, &meta.mime_type) {
        (Some(description), Some(mime)) => format!("{description} ({mime})"),
        (Some(description), None) => description.clone(),
        (None, Some(mime)) => mime.clone(),
        (None, None) => {
            if meta.is_dir { "Folder".to_string() } else { "Unknown type".to_string() }
        }
    }
}

fn icon_for(meta: &FileMeta) -> &'static str {
    if meta.is_dir {
        return "folder-symbolic";
    }
    match meta.mime_type.as_deref() {
        Some(mime) if mime.starts_with("image/") => "image-x-generic-symbolic",
        Some(mime) if mime.starts_with("audio/") => "audio-x-generic-symbolic",
        Some(mime) if mime.starts_with("video/") => "video-x-generic-symbolic",
        Some("application/pdf") => "application-pdf-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// Local, human-readable timestamp (e.g. "5 September 2026, 14:03").
fn format_time(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    glib::DateTime::from_unix_local(seconds)
        .and_then(|datetime| datetime.format("%e %B %Y, %H:%M"))
        .map(|text| text.to_string())
        .unwrap_or_else(|_| "Unknown".to_string())
}
