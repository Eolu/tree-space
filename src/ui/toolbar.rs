//! The panel toolbar: a hamburger menu button and an editable current-path
//! entry that fills the remaining width.
//!
//! Layout (left to right):
//!   [☰]  [/home/user/projects/...                               ]
//!
//! The hamburger opens a popover built from the configurable `[pane_menu]`
//! list (`Open Folder...`, `Filter...`, `Split View`, view/sort actions, `Collapse`,
//! `Close Pane`, custom commands). Each row shows its configured shortcut as a
//! right-aligned hint, mirroring the tree context menu. See
//! [`build_pane_popover`].
//!
//! The same menu's shortcuts are compiled by [`PaneShortcuts`] and matched by a
//! pane-wide key controller (attached in `app::make_pane`), so a pane action
//! works no matter which widget in the pane has focus.
//!
//! While the path entry is edited it shows a dropdown of directory completions
//! for the typed text ([`suggestions`] lists the matching subdirectories of the
//! typed directory, non-recursively). ↑/↓ move the highlight, Enter opens the
//! highlighted one (the first by default, or the typed path when the dropdown is
//! closed), and clicking a row opens it too.
//!
//! The dropdown is a plain child of the pane overlay, *not* a `GtkPopover`:
//! a popover gets its own Wayland surface and would steal the keyboard from the
//! entry, so typing would stop after the first suggestion appeared.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use crate::config::{BuiltinAction, ContextAction, PaneMenu, ShortcutTarget};
use relm4::gtk;
use relm4::gtk::gdk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

/// Most completions to offer for a single typed path.
const MAX_SUGGESTIONS: usize = 15;

/// Intent produced by the toolbar, forwarded to the app.
#[derive(Debug)]
pub enum ToolbarOutput {
    /// The user asked to pick a folder to browse (from the hamburger menu).
    OpenFolder,
    /// The user pressed Enter in the path entry with a (possibly new) path.
    NavigateTo(PathBuf),
    /// The user asked (from the top-bar menu) to open the panel filter bar.
    FilterRequested,
    /// The user requested a new split pane.
    SplitView,
    /// The user requested to hide this pane's entire side (the whole dock).
    Collapse,
    /// The user requested to close this pane (exits the app on the last one).
    ClosePane,
    /// A configurable pane-menu item resolved to a tree builtin or custom
    /// command. The app routes builtins to the pane's tree and reports errors
    /// for commands.
    PaneItem(ShortcutTarget),
}

/// Messages the app can send into the toolbar.
#[derive(Debug)]
pub enum ToolbarMsg {
    /// Update the path entry to reflect the current root.
    SetRoot(PathBuf),
    /// Switch the hamburger to the bookmarks-view menu (or back to the pane
    /// menu) as the pane's body changes.
    SetBookmarks(bool),
}

/// The toolbar's init payload: the pane overlay its completion dropdown is
/// parented to (so the dropdown can draw over the tree without stealing focus),
/// plus the two configurable hamburger menus.
pub struct ToolbarInit {
    pub overlay: gtk::Overlay,
    pub pane_menu: PaneMenu,
    /// The hamburger shown while the bookmarks view is active.
    pub bookmarks_menu: Vec<ContextAction>,
}

/// The toolbar component. Holds the current root path for the editable entry.
pub struct Toolbar {
    root: PathBuf,
    path_entry: gtk::Entry,
    menu_button: gtk::MenuButton,
    /// Items of the normal pane menu and of the bookmarks-view menu; the
    /// popover is rebuilt from one or the other as the body switches.
    pane_items: Vec<ContextAction>,
    bookmarks_items: Vec<ContextAction>,
    completion: Completion,
}

/// The path-entry completion dropdown and the shared state its handlers read.
struct Completion {
    overlay: gtk::Overlay,
    dropdown: gtk::ScrolledWindow,
    list: gtk::Box,
    /// Current suggestions, top-first.
    items: Rc<RefCell<Vec<PathBuf>>>,
    /// Index of the highlighted suggestion.
    selected: Rc<Cell<usize>>,
    /// Set while the entry text is changed programmatically, so `changed` does
    /// not treat it as typing and pop the dropdown open.
    suppress: Rc<Cell<bool>>,
}

#[relm4::component(pub)]
impl SimpleComponent for Toolbar {
    type Init = ToolbarInit;
    type Input = ToolbarMsg;
    type Output = ToolbarOutput;

    view! {
        gtk::Box {
            set_orientation: gtk::Orientation::Horizontal,
            set_spacing: 4,
            add_css_class: "toolbar",

            // ── hamburger menu button ────────────────────────────────────────
            // The popover is built from the configurable pane menu in `init`.
            append: menu_button = &gtk::MenuButton {
                set_icon_name: "open-menu-symbolic",
                set_tooltip_text: Some("Menu"),
                set_valign: gtk::Align::Center,
                set_hexpand: false,
            },

            // ── current path entry ────────────────────────────────────────────
            // The text is driven by `ToolbarMsg::SetRoot`; typing is read by the
            // completion handlers installed in `init`.
            append: path_entry = &gtk::Entry {
                set_placeholder_text: Some("Path..."),
                set_hexpand: true,
                set_valign: gtk::Align::Center,
                add_css_class: "path-entry",
            },
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let widgets = view_output!();
        let entry = widgets.path_entry.clone();

        // The dropdown lives in the pane overlay so it can be drawn on top of
        // the tree while keyboard focus stays in the entry.
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("path-completion-list");

        let dropdown = gtk::ScrolledWindow::new();
        dropdown.add_css_class("path-completion");
        dropdown.set_visible(false);
        dropdown.set_halign(gtk::Align::Start);
        dropdown.set_valign(gtk::Align::Start);
        dropdown.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        dropdown.set_max_content_height(240);
        dropdown.set_propagate_natural_height(true);
        dropdown.set_child(Some(&list));
        init.overlay.add_overlay(&dropdown);

        let completion = Completion {
            overlay: init.overlay,
            dropdown,
            list,
            items: Rc::new(RefCell::new(Vec::new())),
            selected: Rc::new(Cell::new(0)),
            suppress: Rc::new(Cell::new(false)),
        };
        attach_completion(&entry, &completion, &sender);

        let model = Toolbar {
            root: PathBuf::new(),
            path_entry: entry,
            menu_button: widgets.menu_button.clone(),
            pane_items: init.pane_menu.items.clone(),
            bookmarks_items: init.bookmarks_menu.clone(),
            completion,
        };

        // Build the hamburger popover from the configured pane menu.
        let popover = build_pane_popover(&model.pane_items, &sender);
        model.menu_button.set_popover(Some(&popover));

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            ToolbarMsg::SetRoot(path) => {
                self.root = path;
                self.completion.dropdown.set_visible(false);
                self.completion.suppress.set(true);
                self.path_entry.set_text(&self.root.display().to_string());
                self.path_entry.set_position(-1);
                self.completion.suppress.set(false);
            }
            ToolbarMsg::SetBookmarks(on) => {
                let items =
                    if on { &self.bookmarks_items } else { &self.pane_items };
                let popover = build_pane_popover(items, &sender);
                self.menu_button.set_popover(Some(&popover));
            }
        }
    }
}

/// Wire the entry's `changed`, key and focus handlers to the completion state.
fn attach_completion(
    entry: &gtk::Entry,
    completion: &Completion,
    sender: &ComponentSender<Toolbar>,
) {
    {
        let entry_for_rows = entry.clone();
        let overlay = completion.overlay.clone();
        let dropdown = completion.dropdown.clone();
        let list = completion.list.clone();
        let items = completion.items.clone();
        let selected = completion.selected.clone();
        let suppress = completion.suppress.clone();
        let sender = sender.clone();
        entry.connect_changed(move |entry| {
            if suppress.get() {
                return;
            }
            let found = suggestions(&entry.text());
            *items.borrow_mut() = found;
            selected.set(0);
            rebuild_list(&list, &items.borrow(), 0, &entry_for_rows, &dropdown, &suppress, &sender);
            if items.borrow().is_empty() {
                dropdown.set_visible(false);
            } else {
                place_dropdown(&overlay, &dropdown, entry);
                dropdown.set_visible(true);
            }
        });
    }

    {
        let key_entry = entry.clone();
        let dropdown = completion.dropdown.clone();
        let list = completion.list.clone();
        let items = completion.items.clone();
        let selected = completion.selected.clone();
        let suppress = completion.suppress.clone();
        let sender = sender.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, _| {
            let count = items.borrow().len();
            match key {
                gdk::Key::Down if count > 0 => {
                    let i = (selected.get() + 1).min(count - 1);
                    selected.set(i);
                    highlight(&list, i);
                    gtk::glib::Propagation::Stop
                }
                gdk::Key::Up if count > 0 => {
                    let i = selected.get().saturating_sub(1);
                    selected.set(i);
                    highlight(&list, i);
                    gtk::glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    if dropdown.is_visible() && count > 0 {
                        let path = items.borrow()[selected.get()].clone();
                        activate(&key_entry, &dropdown, &suppress, &sender, path);
                    } else {
                        let path = PathBuf::from(key_entry.text().to_string());
                        if path.is_dir() {
                            activate(&key_entry, &dropdown, &suppress, &sender, path);
                        }
                    }
                    gtk::glib::Propagation::Stop
                }
                gdk::Key::Escape if dropdown.is_visible() => {
                    dropdown.set_visible(false);
                    gtk::glib::Propagation::Stop
                }
                _ => gtk::glib::Propagation::Proceed,
            }
        });
        entry.add_controller(keys);
    }

    {
        let dropdown = completion.dropdown.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| dropdown.set_visible(false));
        entry.add_controller(focus);
    }
}

/// Position the dropdown just below `entry`, matching its width, using the
/// pane overlay's coordinate space.
fn place_dropdown(overlay: &gtk::Overlay, dropdown: &gtk::ScrolledWindow, entry: &gtk::Entry) {
    if let Some(rect) = entry.compute_bounds(overlay) {
        dropdown.set_margin_start(rect.x() as i32);
        dropdown.set_margin_top((rect.y() + rect.height()) as i32);
        dropdown.set_width_request(rect.width() as i32);
    }
}

/// Rebuild `list` from `items`, marking row `selected` as highlighted.
fn rebuild_list(
    list: &gtk::Box,
    items: &[PathBuf],
    selected: usize,
    entry: &gtk::Entry,
    dropdown: &gtk::ScrolledWindow,
    suppress: &Rc<Cell<bool>>,
    sender: &ComponentSender<Toolbar>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for (i, path) in items.iter().enumerate() {
        let button = gtk::Button::new();
        button.set_halign(gtk::Align::Fill);
        // Keep focus in the entry: clicking a row must not steal the keyboard.
        button.set_focusable(false);
        button.set_can_focus(false);
        button.add_css_class("path-completion-item");
        if i == selected {
            button.add_css_class("active");
        }
        let label = gtk::Label::new(Some(&path.display().to_string()));
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::Start);
        button.set_child(Some(&label));

        let path = path.clone();
        let entry = entry.clone();
        let dropdown = dropdown.clone();
        let suppress = suppress.clone();
        let sender = sender.clone();
        button.connect_clicked(move |_| {
            activate(&entry, &dropdown, &suppress, &sender, path.clone());
        });
        list.append(&button);
    }
}

/// Move the highlight to `index` without rebuilding the rows.
fn highlight(list: &gtk::Box, index: usize) {
    let mut i = 0;
    let mut child = list.first_child();
    while let Some(current) = child {
        if let Some(button) = current.downcast_ref::<gtk::Button>() {
            if i == index {
                button.add_css_class("active");
            } else {
                button.remove_css_class("active");
            }
            i += 1;
        }
        child = current.next_sibling();
    }
}

/// Open `path`: fill the entry, close the dropdown, and tell the app to navigate.
fn activate(
    entry: &gtk::Entry,
    dropdown: &gtk::ScrolledWindow,
    suppress: &Rc<Cell<bool>>,
    sender: &ComponentSender<Toolbar>,
    path: PathBuf,
) {
    suppress.set(true);
    entry.set_text(&path.display().to_string());
    entry.set_position(-1);
    suppress.set(false);
    dropdown.set_visible(false);
    let _ = sender.output(ToolbarOutput::NavigateTo(path));
}

/// Directory completions for the typed `text`, non-recursively: the directories
/// inside the typed path's parent whose name starts with the typed final
/// component. A leading `~` expands to `$HOME`; a trailing `/` lists everything
/// in that directory. Results are name-sorted and capped at [`MAX_SUGGESTIONS`].
fn suggestions(text: &str) -> Vec<PathBuf> {
    if text.is_empty() {
        return Vec::new();
    }
    let expanded = match text.strip_prefix('~') {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => format!("{home}{rest}"),
            Err(_) => text.to_string(),
        },
        None => text.to_string(),
    };

    // `parent` keeps the typed directory (with its trailing separator) so
    // suggestions read back as an extension of what was typed.
    let (parent, prefix) = match expanded.rsplit_once('/') {
        Some((head, name)) => (format!("{head}/"), name.to_string()),
        None => (String::new(), expanded),
    };
    let dir = if parent.is_empty() { PathBuf::from(".") } else { PathBuf::from(&parent) };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let show_hidden = prefix.starts_with('.');
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| show_hidden || !name.starts_with('.'))
        .filter(|name| name.starts_with(&prefix))
        .collect();
    names.sort();
    names.truncate(MAX_SUGGESTIONS);
    names.into_iter().map(|name| PathBuf::from(format!("{parent}{name}"))).collect()
}

/// Build the hamburger popover from a configured menu (the pane menu or the
/// bookmarks-view menu). Separators become dividers; every other item becomes a
/// button that pops the menu down and dispatches its action. Plain buttons never
/// auto-close a `GtkPopover`, hence the explicit `popdown`.
fn build_pane_popover(items: &[ContextAction], sender: &ComponentSender<Toolbar>) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class("hamburger-popover");

    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 0);
    box_.add_css_class("hamburger-menu");
    append_pane_items(&box_, items, &popover, sender);

    popover.set_child(Some(&box_));
    popover
}

/// Append `items` (recursing into submenus) to the hamburger menu box.
/// `close` is the top-level popover a leaf item pops down before dispatching;
/// submenu rows open a nested popover instead.
fn append_pane_items(
    box_: &gtk::Box,
    items: &[ContextAction],
    close: &gtk::Popover,
    sender: &ComponentSender<Toolbar>,
) {
    for item in items {
        // A hidden item keeps its shortcut live but is not rendered.
        if item.is_hidden() {
            continue;
        }
        if matches!(item, ContextAction::Builtin(BuiltinAction::Separator)) {
            let divider = gtk::Separator::new(gtk::Orientation::Horizontal);
            divider.add_css_class("tree-menu-separator");
            box_.append(&divider);
            continue;
        }

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("tree-menu-item");

        // A submenu row opens a nested popover.
        if let ContextAction::Submenu(sub) = item {
            let button = menu_row_button(&sub.label);
            let child_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            child_box.add_css_class("hamburger-menu");
            let child = gtk::Popover::new();
            child.add_css_class("hamburger-popover");
            append_pane_items(&child_box, &sub.items, close, sender);
            child.set_child(Some(&child_box));
            child.set_has_arrow(false);
            child.set_position(gtk::PositionType::Right);
            let child_anchor = child.clone();
            let anchor = button.clone();
            button.connect_clicked(move |_| {
                child_anchor.set_parent(&anchor);
                child_anchor.popup();
            });
            let child_anchor = child.clone();
            child.connect_closed(move |_| child_anchor.unparent());
            row.append(&button);
            let arrow = gtk::Label::new(Some("\u{25b8}"));
            arrow.add_css_class("tree-menu-submenu-arrow");
            arrow.set_halign(gtk::Align::End);
            arrow.set_valign(gtk::Align::Center);
            row.append(&arrow);
            box_.append(&row);
            continue;
        }

        let button = menu_row_button(&item.label());
        let action = item.clone();
        let p = close.clone();
        let s = sender.clone();
        button.connect_clicked(move |_| {
            p.popdown();
            dispatch_pane_action(&action, &s);
        });
        row.append(&button);

        if let Some(shortcut) = item.shortcut()
            && let Some(display) = crate::ui::tree::accel_display(&shortcut)
        {
            let hint = gtk::Label::new(Some(&display));
            hint.add_css_class("tree-menu-shortcut");
            hint.set_halign(gtk::Align::End);
            hint.set_valign(gtk::Align::Center);
            row.append(&hint);
        }

        box_.append(&row);
    }
}

/// A full-width, left-aligned label button for a hamburger-menu row.
fn menu_row_button(label: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    button.set_child(Some(&text));
    button
}

/// Resolve one pane-menu item to a toolbar output. Pane-level builtins map to
/// their dedicated variants; tree-level builtins and custom commands go through
/// [`ToolbarOutput::PaneItem`] for the app to route.
fn dispatch_pane_action(action: &ContextAction, sender: &ComponentSender<Toolbar>) {
    let target = match action {
        ContextAction::Builtin(a) => *a,
        ContextAction::Entry(e) => e.action,
        ContextAction::Command(cmd) => {
            let _ = sender.output(ToolbarOutput::PaneItem(ShortcutTarget::Command(cmd.clone())));
            return;
        }
        // A submenu row never dispatches; its children carry the actions.
        ContextAction::Submenu(_) => return,
    };
    let out = match target {
        BuiltinAction::OpenFolder => ToolbarOutput::OpenFolder,
        BuiltinAction::Filter => ToolbarOutput::FilterRequested,
        BuiltinAction::SplitView => ToolbarOutput::SplitView,
        BuiltinAction::Collapse => ToolbarOutput::Collapse,
        BuiltinAction::ClosePane => ToolbarOutput::ClosePane,
        // Navigation actions are pane-level too; route them through the shared
        // pane-item path so the app's dispatcher performs them.
        BuiltinAction::Up | BuiltinAction::Back | BuiltinAction::Forward => {
            ToolbarOutput::PaneItem(ShortcutTarget::Builtin(target))
        }
        BuiltinAction::Separator => return,
        other => ToolbarOutput::PaneItem(ShortcutTarget::Builtin(other)),
    };
    let _ = sender.output(out);
}

/// The pane-menu accelerators compiled to key + modifier form, so a key press
/// can be resolved anywhere in the pane (not only while the tree has focus).
pub struct PaneShortcuts {
    bindings: Vec<(gdk::Key, gdk::ModifierType, ContextAction)>,
}

impl PaneShortcuts {
    /// Compile every shortcut-carrying item in `items`. Unparseable
    /// accelerators are skipped (the menu item still works).
    pub fn compile(items: &[ContextAction]) -> Self {
        let mut bindings = Vec::new();
        collect_pane_bindings(items, &mut bindings);
        Self { bindings }
    }

    /// The menu item whose shortcut matches this key event, if any.
    pub fn action_for(
        &self,
        key: gdk::Key,
        state: gdk::ModifierType,
    ) -> Option<ContextAction> {
        let state = accel_mods(state);
        self.bindings
            .iter()
            .find(|(k, mods, _)| key.to_lower() == k.to_lower() && accel_mods(*mods) == state)
            .map(|(_, _, action)| action.clone())
    }
}

/// Recursively collect shortcut bindings from `items`, descending into
/// submenus. Hidden items are included: hiding only affects rendering, never
/// whether a shortcut fires.
fn collect_pane_bindings(
    items: &[ContextAction],
    out: &mut Vec<(gdk::Key, gdk::ModifierType, ContextAction)>,
) {
    for item in items {
        if let Some(children) = item.submenu_items() {
            collect_pane_bindings(children, out);
            continue;
        }
        let Some(accel) = item.shortcut() else {
            continue;
        };
        if let Some((key, mods)) = parse_accelerator(&accel) {
            out.push((key, mods, item.clone()));
        }
    }
}

/// Strip a key state down to the modifier bits accelerators care about.
fn accel_mods(m: gdk::ModifierType) -> gdk::ModifierType {
    m & (gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::HYPER_MASK
        | gdk::ModifierType::META_MASK)
}

/// Parse a `Ctrl+x` style accelerator into a key and modifier set. GTK's parser
/// only understands its native `<Control>x` spelling, so normalize `+`-joined
/// strings first.
pub fn parse_accelerator(accel: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
    gtk::accelerator_parse(normalize_accelerator(accel))
}

fn normalize_accelerator(accel: &str) -> String {
    if !accel.contains('+') {
        return accel.to_owned();
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
            _ => key = token.to_owned(),
        }
    }
    format!("{mods}{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_slash_lists_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("file.txt"), b"x").unwrap();

        let query = format!("{}/", dir.path().display());
        let got = suggestions(&query);
        assert_eq!(
            got,
            vec![dir.path().join("alpha"), dir.path().join("beta")],
            "only directories, name-sorted"
        );
    }

    #[test]
    fn partial_name_completes_case_sensitively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("projects")).unwrap();
        std::fs::create_dir(dir.path().join("pictures")).unwrap();

        let query = format!("{}/pro", dir.path().display());
        assert_eq!(suggestions(&query), vec![dir.path().join("projects")]);
    }

    #[test]
    fn non_matching_prefix_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        let query = format!("{}/zzz", dir.path().display());
        assert!(suggestions(&query).is_empty());
    }

    #[test]
    fn hidden_directories_only_shown_for_a_dot_prefix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".config")).unwrap();
        std::fs::create_dir(dir.path().join("visible")).unwrap();

        let all = suggestions(&format!("{}/", dir.path().display()));
        assert_eq!(all, vec![dir.path().join("visible")]);

        let hidden = suggestions(&format!("{}/.", dir.path().display()));
        assert_eq!(hidden, vec![dir.path().join(".config")]);
    }

    #[test]
    fn suggestions_are_capped() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_SUGGESTIONS + 5) {
            std::fs::create_dir(dir.path().join(format!("dir{i:03}"))).unwrap();
        }
        let got = suggestions(&format!("{}/", dir.path().display()));
        assert_eq!(got.len(), MAX_SUGGESTIONS);
    }

    #[test]
    fn empty_text_yields_nothing() {
        assert!(suggestions("").is_empty());
    }
}
