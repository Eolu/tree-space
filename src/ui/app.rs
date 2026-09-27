//! The root component: hosts the panel as one or more dock windows — a left
//! dock, a right dock, or both — each containing its own stack of split panes,
//! and applies single-instance launch requests.
//!
//! Dock model
//! ──────────
//! [`Dock`] is one layer-shell window anchored to one screen edge. It owns a
//! vertical stack of [`Pane`]s. A [`Pane`] is a top bar (hamburger menu +
//! editable path entry), an optional filter row, and its own `Tree` component.
//! Panes have globally stable ids (never reused after close) so toolbar/tree
//! messages survive dock rebuilds and pane removals.
//!
//! The first dock is the *primary* dock: its window is the relm4 root window
//! (rendered by `view!`), its container is `App::pane_container`. Additional
//! docks are created on demand when a launch request names a side not present
//! yet; they are ordinary `gtk::Window`s built imperatively.
//!
//! Split-pane layout within a dock
//! ───────────────────────────────
//! The GTK widget tree is rebuilt from scratch when a dock's pane count
//! changes. Panes are nested with `gtk::Paned`:
//!
//!   panes = [A, B, C]
//!   widget tree = Box { Paned { A, Paned { B, C } } }
//!
//! Each pane is a vertical `Box { toolbar, filter-bar, tree }`. The tree
//! widget's `vexpand` is set to `true` so each half fills its allocation.
//! Every pane has its own filter bar (hidden until requested from that pane's
//! top-bar menu); it applies only to that pane's tree.
//!
//! Launch requests
//! ───────────────
//! A second `tree-space` invocation is forwarded over the instance socket (see
//! [`crate::ipc`]) and arrives as [`AppMsg::LaunchRequest`]:
//!   * no path            → show/hide the whole panel (toggle), or with
//!     `--side X` ensure a dock exists on X and show it
//!   * with path(s)       → add a pane for each path (never a duplicate of an
//!     already-open directory), then show the panel

use std::{cell::{Cell, RefCell}, collections::HashMap, path::{Path, PathBuf}, rc::Rc};

use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk::{gdk, gio, glib, prelude::*};
use relm4::prelude::*;

use crate::cmd::{Command, WidthArg};
use crate::config::{
    Bookmark, BookmarkPosition, BuiltinAction, Config, ContextAction, PANEL_MAX_WIDTH,
    PANEL_MIN_WIDTH, PanelConfig, PanelLayer, PanelSide, PaneMenu, SessionState, ShortcutTarget,
    StartupRoot, bookmark_file_path, load_stylesheet, save_bookmarks_to_path,
};
use crate::fs::SortKey;
use crate::ipc;
use crate::ui::bookmarks::{self, BookmarkEvent};
use crate::ui::toolbar::{PaneShortcuts, Toolbar, ToolbarInit, ToolbarMsg, ToolbarOutput};
use crate::ui::tree::{Tree, TreeInit, TreeOutput, TreeMsg};

/// A pane's back/forward navigation history.
///
/// Directories are recorded in visit order with a cursor into the list. Going
/// back moves the cursor left, forward moves it right; visiting a *new* root
/// (not via back/forward) truncates the forward tail and appends. This is a
/// pure data structure so the rules can be unit-tested without a display.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NavHistory {
    entries: Vec<PathBuf>,
    /// Index of the current entry. Meaningless while `entries` is empty.
    cursor: usize,
    /// Set while a back/forward navigation is in flight, so the `RootChanged`
    /// it produces is not itself recorded as a new visit.
    navigating: bool,
}

impl NavHistory {
    /// Record a newly opened root. A repeat of the current entry is ignored;
    /// any forward history is dropped. While a back/forward is navigating this
    /// is a no-op (the target is already in the list).
    fn record(&mut self, path: &Path) {
        if self.navigating {
            return;
        }
        if self.entries.get(self.cursor).is_some_and(|cur| cur == path) {
            return;
        }
        if self.entries.is_empty() {
            self.entries.push(path.to_path_buf());
            self.cursor = 0;
            return;
        }
        self.entries.truncate(self.cursor + 1);
        self.entries.push(path.to_path_buf());
        self.cursor = self.entries.len() - 1;
    }

    fn can_back(&self) -> bool {
        self.cursor > 0 && self.cursor < self.entries.len()
    }

    fn can_forward(&self) -> bool {
        !self.entries.is_empty() && self.cursor + 1 < self.entries.len()
    }

    /// Step back one entry and return its path, arming `navigating`.
    fn back(&mut self) -> Option<PathBuf> {
        if !self.can_back() {
            return None;
        }
        self.cursor -= 1;
        self.navigating = true;
        Some(self.entries[self.cursor].clone())
    }

    /// Step forward one entry and return its path, arming `navigating`.
    fn forward(&mut self) -> Option<PathBuf> {
        if !self.can_forward() {
            return None;
        }
        self.cursor += 1;
        self.navigating = true;
        Some(self.entries[self.cursor].clone())
    }

    /// Clear the in-flight flag once the resulting `RootChanged` has arrived.
    fn finish_navigation(&mut self) {
        self.navigating = false;
    }
}

/// One split view inside a dock: its own top bar above its own tree.
pub struct Pane {
    id: u64,
    toolbar: Controller<Toolbar>,
    tree: Controller<Tree>,
    /// The directory this pane currently shows, if any. Owned here (rather than
    /// in a parallel `Vec` on the dock) so a pane and its root can never drift.
    root: Option<PathBuf>,
    /// The canonical form of `root`, cached so launch deduplication does not
    /// `canonicalize` every pane on every request. Refreshed on `RootChanged`.
    canonical_root: Option<PathBuf>,
    /// Back/forward history for this pane's root changes.
    history: NavHistory,
    /// This pane's own filter row (hidden until requested from its top bar).
    /// Only this pane's tree receives the filter.
    filter_bar: gtk::Box,
    filter_entry: gtk::SearchEntry,
    /// The pane overlay: its main child is the `{ toolbar, filter_bar?, tree }`
    /// vertical box, and its overlay children hold panes' popovers (e.g. the
    /// path-entry completion dropdown). Built once and reused across split/close
    /// rebuilds, so the tree and toolbar widgets never need to be reparented
    /// (which would trip `gtk_box_append: child has a parent`).
    widget: gtk::Overlay,
}

impl Pane {
    /// Record `root` as this pane's directory and refresh its cached canonical
    /// form.
    fn set_root(&mut self, root: PathBuf) {
        self.canonical_root = std::fs::canonicalize(&root).ok();
        self.root = Some(root);
    }
}

/// One layer-shell dock window anchored to a screen edge, with its panes.
struct Dock {
    side: PanelSide,
    /// The dock's window. For the primary dock this is the relm4 root window;
    /// for additional docks an imperatively-built `gtk::Window`.
    window: gtk::Window,
    /// The vertical box holding the pane stack (for the primary dock this is
    /// `App::pane_container`, referenced by `view!`).
    container: gtk::Box,
    /// The bookmarks section, when it is shown in this dock. Parented in the
    /// dock's outer box (not `container`) so rebuilding the pane stack never
    /// disturbs it.
    bookmarks: Option<gtk::Widget>,
    panes: Vec<Pane>,
    /// Id of the pane most recently interacted with in this dock.
    active_pane: Option<u64>,
}

/// What an invocation (or the hamburger "Collapse") wants to do to dock
/// visibility. Pure data, so the show/hide rules can be unit-tested without a
/// display (see the `visibility` tests below).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityIntent {
    /// Show every dock.
    ShowAll,
    /// Hide every dock.
    HideAll,
    /// Toggle every dock: hide them all when any is visible, else show all.
    ToggleAll,
    /// Show the named side, creating its dock if needed.
    ShowSide(PanelSide),
    /// Hide the named side (a no-op when that dock does not exist).
    HideSide(PanelSide),
    /// Toggle the named side, creating and showing it if it does not exist.
    ToggleSide(PanelSide),
}

/// The resolved effect of a [`VisibilityIntent`]: which sides should be shown
/// afterwards, and whether a missing dock should be created to satisfy it.
///
/// This is the entire visibility state machine as a pure function; the app
/// method [`App::apply_visibility`] only carries out the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibilityPlan {
    /// Sides that should be visible afterwards.
    pub show: Vec<PanelSide>,
    /// Sides that should be hidden afterwards.
    pub hide: Vec<PanelSide>,
    /// Create and seed a dock on this side when it does not already exist.
    pub create: Option<PanelSide>,
    /// Seed an empty dock with a default pane (so showing it is not an empty
    /// shell). Only used with `create`/`show`.
    pub seed: bool,
}

/// Resolve `intent` against the dock sides that currently exist and the sides
/// currently shown. `show`/`hide` name the sides to act on; `create` names a
/// side whose dock must be created first.
pub fn resolve_visibility(
    intent: VisibilityIntent,
    existing: &[PanelSide],
    shown: &[PanelSide],
) -> VisibilityPlan {
    match intent {
        VisibilityIntent::ShowAll => VisibilityPlan {
            show: existing.to_vec(),
            hide: Vec::new(),
            create: None,
            seed: true,
        },
        VisibilityIntent::HideAll => VisibilityPlan {
            show: Vec::new(),
            hide: PanelSide::ALL.to_vec(),
            create: None,
            seed: false,
        },
        VisibilityIntent::ToggleAll => {
            if shown.is_empty() {
                VisibilityPlan {
                    show: existing.to_vec(),
                    hide: Vec::new(),
                    create: None,
                    seed: true,
                }
            } else {
                VisibilityPlan {
                    show: Vec::new(),
                    hide: PanelSide::ALL.to_vec(),
                    create: None,
                    seed: false,
                }
            }
        }
        VisibilityIntent::ShowSide(side) => VisibilityPlan {
            show: vec![side],
            hide: Vec::new(),
            create: (!existing.contains(&side)).then_some(side),
            seed: true,
        },
        VisibilityIntent::HideSide(side) => VisibilityPlan {
            show: Vec::new(),
            hide: vec![side],
            create: None,
            seed: false,
        },
        VisibilityIntent::ToggleSide(side) => {
            if !existing.contains(&side) {
                VisibilityPlan {
                    show: vec![side],
                    hide: Vec::new(),
                    create: Some(side),
                    seed: true,
                }
            } else if shown.contains(&side) {
                VisibilityPlan {
                    show: Vec::new(),
                    hide: vec![side],
                    create: None,
                    seed: false,
                }
            } else {
                VisibilityPlan {
                    show: vec![side],
                    hide: Vec::new(),
                    create: None,
                    seed: true,
                }
            }
        }
    }
}

/// Messages handled by the app itself.
#[derive(Debug)]
pub enum AppMsg {
    /// Output from the top bar of pane `id`.
    PaneToolbar { id: u64, out: ToolbarOutput },
    /// Output from the tree of pane `id`.
    PaneTree { id: u64, out: TreeOutput },
    /// A pane-menu shortcut fired anywhere in pane `id` (the tree, the path
    /// entry, or the filter bar), resolved from the `[pane_menu]` accelerators.
    PaneShortcut { id: u64, action: ContextAction },
    /// The folder-picker launched for pane `id` returned.
    OpenFolderPicked { id: u64, path: Option<PathBuf> },
    /// A new `tree-space` invocation was forwarded by the instance socket.
    LaunchRequest { command: Command },
    /// Split pane `id`'s dock, seeding the new pane from `id`'s root.
    SplitFromPane { id: u64 },
    /// Split pane `id`'s dock with a specific root (context-menu "Open in Split
    /// View").
    OpenSplitFrom { id: u64, root: PathBuf },
    /// Open `root` in a pane on the dock opposite pane `id`'s (context-menu
    /// "In {other} panel").
    OpenOppositeFrom { id: u64, root: PathBuf },
    /// Remove pane `id` (closes the dock, or the whole app, when it is the
    /// last pane).
    ClosePane { id: u64 },
    /// Pane `id`'s filter text changed (empty string clears the filter).
    FilterChanged { id: u64, filter: String },
    /// Close pane `id`'s filter bar and cancel its filter.
    FilterClosed { id: u64 },
    /// Quit requested (window close). Tears media down before the window closes.
    Shutdown,
    /// Change the width of `side`'s dock by `delta` px (drag / keyboard).
    ResizeBy { side: PanelSide, delta: i32 },
    /// Persist the width after an interactive resize finishes.
    ResizeCommit,
    /// Move keyboard focus into pane `id`'s tree (after it is allocated).
    FocusPane { id: u64 },
    /// The bookmarks section in `side`'s dock reported a user action.
    BookmarkEvent { side: PanelSide, event: BookmarkEvent },
    /// The bookmark editor for index `index` was saved with the new values.
    BookmarkEditSaved { index: usize, name: String, path: PathBuf },
    /// Show or hide the bookmarks section in every dock (hamburger item).
    ToggleBookmarks,
}

/// The init payload: the parsed invocation plus the instance socket, if this
/// process became the single server.
pub struct AppInit {
    pub command: Command,
    pub listener: Option<std::os::unix::net::UnixListener>,
}

/// Root state.
pub struct App {
    config: Config,
    status: String,
    window: gtk::Window,
    file_dialog: gtk::FileDialog,
    /// All dock windows. Index 0 is the primary dock (the root window).
    docks: Vec<Dock>,
    /// The primary dock's container child (referenced by `view!`).
    pane_container: gtk::Box,
    /// The panel's side for launches that do not specify one.
    primary_side: PanelSide,
    /// Next pane id; bumped on every pane creation so ids never repeat.
    next_id: u64,
    /// Whether any dock window is currently shown (drives no-arg toggling).
    visible: bool,
    /// Per-side dock width in pixels. Starts from session state (falling back to
    /// `[panel] width`) and changes on interactive resize — one entry per side,
    /// so the two docks are sized independently without parallel scalar fields.
    widths: HashMap<PanelSide, u32>,
    /// Most recently opened root, kept so a width save never drops it.
    last_root: Option<PathBuf>,
    /// Whether the bookmarks section is currently shown in every dock. Starts
    /// from `[bookmarks] show` (or a `startup = "bookmarks"` launch) and is
    /// toggled at runtime from the hamburger menu.
    show_bookmarks: bool,
    /// The saved directory shortcuts, loaded from the bookmarks file and written
    /// back whenever they change.
    bookmarks: Vec<Bookmark>,
    /// Monotonic id for the debounced width save: a scheduled save only writes
    /// if it is still the latest (no `SourceId` juggling — removing a one-shot
    /// source that has already fired panics).
    save_generation: Rc<Cell<u64>>,
}

#[relm4::component(pub)]
impl SimpleComponent for App {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();

    view! {
        gtk::Window {
            set_default_size: (model.config.panel.width as i32, 520),

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 0,
                add_css_class: "panel",

                append: &model.pane_container,

                append: status = &gtk::Label {
                    #[watch]
                    set_label: &model.status,
                    set_halign: gtk::Align::Start,
                    set_ellipsize: gtk::pango::EllipsizeMode::End,
                    add_css_class: "status-bar",
                }
            }
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let loaded = Config::load();
        let mut config = loaded.config;
        let parent = root.clone();

        // Bookmarks are runtime data kept in their own file beside the config.
        // Materialize it (home by default) on first launch.
        let (bookmarks, bookmark_problem) = crate::config::load_bookmarks(&config.bookmarks);
        if let Err(problem) = crate::config::ensure_bookmarks_file(&config.bookmarks, &bookmarks) {
            eprintln!("tree-space: could not create bookmarks file: {problem:?}");
        }

        let session = SessionState::load();
        // An interactive resize is sticky across launches; the configured
        // `[panel] width` is only the initial default for each side.
        let default_width = config.panel.width;
        let widths: HashMap<PanelSide, u32> = [
            (
                PanelSide::Left,
                session.left_width.unwrap_or(default_width).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH),
            ),
            (
                PanelSide::Right,
                session.right_width.unwrap_or(default_width).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH),
            ),
        ]
        .into_iter()
        .collect();

        let primary_side = init.command.side.unwrap_or(config.panel.side);
        // Keep `config.panel.width` meaningful for the primary dock.
        config.panel.width = widths[&primary_side];
        let primary_width = config.panel.width;
        // A `startup = "bookmarks"` launch opens with the section showing and no
        // directory pane; the hamburger or a bookmark click adds panes later.
        let bookmarks_startup = config.startup.is_bookmarks();
        let show_bookmarks = config.bookmarks.show || bookmarks_startup;
        // Build the primary dock's initial panes from the invocation. When the
        // invocation carries no roots, resolve the configured startup directory
        // (last-used, home, or a fixed path), falling back to home.
        let roots = if !init.command.roots.is_empty() {
            init.command.roots.clone()
        } else if bookmarks_startup {
            Vec::new()
        } else {
            let last = session.last_root.clone().filter(|p| p.is_dir());
            let fallback = config
                .startup
                .resolve(last)
                .filter(|p| p.is_dir())
                .or_else(home_dir);
            if let Some(p) = fallback { vec![p] } else { Vec::new() }
        };

        let mut panes: Vec<Pane> = Vec::new();
        let mut next_id = 0u64;
        for root in roots.iter() {
            let pane =
                make_pane(&config, parent.clone(), next_id, primary_side, primary_width, sender.clone());
            pane.tree.emit(TreeMsg::OpenRoot(root.clone()));
            panes.push(pane);
            panes.last_mut().unwrap().set_root(root.clone());
            next_id += 1;
        }
        // Guarantee at least one pane, unless the bookmarks view is the whole
        // initial content.
        if panes.is_empty() && !bookmarks_startup {
            panes.push(make_pane(
                &config,
                parent.clone(),
                next_id,
                primary_side,
                primary_width,
                sender.clone(),
            ));
            next_id += 1;
        }

        let pane_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pane_container.set_vexpand(true);
        fill_pane_container(&pane_container, &panes);

        let mut status = String::new();
        if let Some(problem) = loaded.problem {
            status = format!("config: {problem:?}");
        } else if let Some(problem) = bookmark_problem {
            status = format!("bookmarks: {problem:?}");
        }

        let mut model = App {
            config,
            status,
            window: root.clone(),
            file_dialog: gtk::FileDialog::new(),
            docks: vec![Dock {
                side: primary_side,
                window: root.clone(),
                container: pane_container.clone(),
                bookmarks: None,
                panes,
                active_pane: None,
            }],
            pane_container,
            primary_side,
            next_id,
            visible: !init.command.hidden,
            widths,
            last_root: session.last_root.clone(),
            show_bookmarks,
            bookmarks,
            save_generation: Rc::new(Cell::new(0)),
        };

        init_layer_window(&model.window, &model.config, primary_side, primary_width);
        install_css(&model.config);
        attach_bookmarks_shortcut(&model.window, &model.config.pane_menu, &sender);
        // Interactive resize only makes sense for a docked layer surface; a
        // plain fallback window is resized like any other window.
        if layer_shell_available() {
            attach_resize_controls(&model.window, primary_side, &sender);
        }

        // Serve the instance socket: every new `tree-space` invocation delivers
        // a Command here, hopped onto the UI thread by the main context.
        if let Some(listener) = init.listener {
            let ipc_sender = sender.input_sender().clone();
            ipc::spawn_listener(listener, move |command| {
                let sender = ipc_sender.clone();
                glib::MainContext::default().invoke(move || {
                    let _ = sender.send(AppMsg::LaunchRequest { command });
                });
            });
        }

        let widgets = view_output!();

        // The primary dock's outer box now exists; mount the bookmarks section.
        model.refresh_bookmarks(0, &sender);

        // Quitting while a video thumbnail is playing is a shutdown race:
        // `GtkMediaFile` renders through GStreamer's GL sink, and exiting with
        // that context live lets NVIDIA's at-exit EGL teardown unmap GPU memory
        // while the `gstglcontext` thread is still issuing GL calls (SIGSEGV in
        // the driver). Intercept the first close, tear the media down on the
        // main loop, and only let the window close once that has settled.
        let shutting_down = Rc::new(Cell::new(false));
        let flag = shutting_down.clone();
        let shutdown_sender = sender.clone();
        model.window.connect_close_request(move |_| {
            if flag.get() {
                return glib::Propagation::Proceed;
            }
            flag.set(true);
            shutdown_sender.input(AppMsg::Shutdown);
            glib::Propagation::Stop
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        match msg {
            AppMsg::PaneToolbar { id, out } => match out {
                ToolbarOutput::OpenFolder => {
                    self.set_active(id);
                    self.show_open_folder(id, &sender);
                }
                ToolbarOutput::NavigateTo(path) => {
                    self.set_active(id);
                    if let Some((di, pi)) = self.dock_pane_of(id) {
                        self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(path));
                    }
                }
                ToolbarOutput::FilterRequested => {
                    self.set_active(id);
                    self.open_filter(id);
                }
                ToolbarOutput::SplitView => {
                    sender.input(AppMsg::SplitFromPane { id });
                }
                ToolbarOutput::Collapse => {
                    self.collapse_dock(id);
                }
                ToolbarOutput::ClosePane => {
                    sender.input(AppMsg::ClosePane { id });
                }
                ToolbarOutput::PaneItem(target) => {
                    self.set_active(id);
                    // A pane-level builtin (Up/Back/Forward, ...) is performed by
                    // the app; everything else is a tree action.
                    if let ShortcutTarget::Builtin(action) = &target
                        && action.is_pane_action()
                    {
                        self.dispatch_pane_builtin(id, *action, &sender);
                    } else if let Some((di, pi)) = self.dock_pane_of(id)
                        && let Some(msg) = pane_item_message(&target)
                    {
                        self.docks[di].panes[pi].tree.emit(msg);
                    }
                }
            },

            AppMsg::PaneTree { id, out } => match out {
                TreeOutput::Status(message) => {
                    self.set_active(id);
                    self.status = message;
                }
                TreeOutput::OpenFolderRequested => {
                    self.set_active(id);
                    self.show_open_folder(id, &sender);
                }
                TreeOutput::OpenSplit(root) => {
                    self.set_active(id);
                    sender.input(AppMsg::OpenSplitFrom { id, root });
                }
                TreeOutput::OpenOpposite(root) => {
                    self.set_active(id);
                    sender.input(AppMsg::OpenOppositeFrom { id, root });
                }
                TreeOutput::PaneAction(action) => {
                    self.dispatch_pane_builtin(id, action, &sender);
                }
                TreeOutput::AddBookmark(path) => {
                    self.set_active(id);
                    self.add_bookmark(path);
                    self.refresh_all_bookmarks(&sender);
                }
                TreeOutput::RootChanged(root) => {
                    self.set_active(id);
                    if let Some((di, pi)) = self.dock_pane_of(id) {
                        let pane = &mut self.docks[di].panes[pi];
                        // Record the visit for back/forward, then clear the
                        // in-flight flag a back/forward navigation sets (so its
                        // own root change is not recorded as a fresh visit).
                        pane.history.record(&root);
                        pane.history.finish_navigation();
                        pane.set_root(root.clone());
                        pane.toolbar.emit(ToolbarMsg::SetRoot(root.clone()));
                    }
                    self.status = String::new();
                    // Persist the most recently opened root for the next launch.
                    self.last_root = Some(root);
                    self.persist_session();
                }
            },

            AppMsg::PaneShortcut { id, action } => {
                self.run_pane_shortcut(id, action, &sender);
            }

            AppMsg::OpenFolderPicked { id, path: Some(path) } => {
                if let Some((di, pi)) = self.dock_pane_of(id) {
                    self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(path));
                }
            }
            AppMsg::OpenFolderPicked { path: None, .. } => {}

            AppMsg::LaunchRequest { command } => {
                self.handle_launch(command, &sender);
            }

            AppMsg::SplitFromPane { id } => {
                let (di, pi) = self.dock_pane_of(id).unwrap_or((0, 0));
                let seed = self.docks[di].panes.get(pi).and_then(|p| p.root.clone()).or_else(home_dir);
                self.add_pane(di, seed, &sender);
            }

            AppMsg::OpenSplitFrom { id, root } => {
                if let Some((di, _pi)) = self.dock_pane_of(id) {
                    self.add_pane(di, Some(root), &sender);
                }
            }

            AppMsg::OpenOppositeFrom { id, root } => {
                if let Some((di, _pi)) = self.dock_pane_of(id) {
                    let side = self.docks[di].side.opposite();
                    let target = self.ensure_dock(side, false, &sender);
                    self.add_pane(target, Some(root), &sender);
                    // A freshly created dock starts hidden; reveal it (unless
                    // the panel is currently toggled off).
                    self.set_docks_visible(self.visible);
                }
            }

            AppMsg::ClosePane { id } => {
                let Some((di, pi)) = self.dock_pane_of(id) else {
                    return;
                };
                self.docks[di].panes.remove(pi);
                if self.docks[di].active_pane == Some(id) {
                    self.docks[di].active_pane = self.docks[di].panes.last().map(|p| p.id);
                }
                // The program only exits once the *last* pane anywhere closes,
                // unless the bookmarks section is showing (which keeps the panel
                // alive on its own).
                let panes_left: usize = self.docks.iter().map(|d| d.panes.len()).sum();
                if panes_left == 0 && !self.show_bookmarks {
                    self.window.close();
                    return;
                }
                if self.docks[di].panes.is_empty() {
                    if self.show_bookmarks {
                        // Keep the dock for its bookmarks section; just clear the
                        // (now empty) pane stack.
                        fill_pane_container(&self.docks[di].container.clone(), &[]);
                    } else if di == 0 {
                        // The primary dock is the relm4 root window; closing it
                        // tears the whole app down. Hide it instead, and show it
                        // again on the next launch/toggle.
                        self.visible = false;
                        self.docks[di].window.set_visible(false);
                    } else {
                        let dock = self.docks.remove(di);
                        dock.window.close();
                    }
                } else {
                    fill_pane_container(&self.docks[di].container.clone(), &self.docks[di].panes);
                }
            }

            AppMsg::FilterChanged { id, filter } => {
                if let Some((di, pi)) = self.dock_pane_of(id) {
                    self.docks[di].panes[pi].tree.emit(TreeMsg::SetFilter(filter));
                }
            }
            AppMsg::FilterClosed { id } => {
                if let Some((di, pi)) = self.dock_pane_of(id) {
                    let pane = &mut self.docks[di].panes[pi];
                    pane.filter_entry.set_text("");
                    pane.tree.emit(TreeMsg::SetFilter(String::new()));
                    pane.filter_bar.set_visible(false);
                }
            }

            AppMsg::FocusPane { id } => {
                if let Some((di, pi)) = self.dock_pane_of(id) {
                    self.docks[di].panes[pi].tree.emit(TreeMsg::Focus);
                }
            }

            AppMsg::Shutdown => {
                // Release every video stream synchronously, walking the widget
                // trees rather than the model so that panes already detached
                // from `self.docks` (a closed last pane keeps its widgets alive
                // until the window is destroyed) are covered too.
                for dock in &self.docks {
                    stop_video_widgets(dock.window.upcast_ref());
                }
                // Drop the cached media (and thumbnails) on the main loop.
                for dock in &self.docks {
                    for pane in &dock.panes {
                        pane.tree.emit(TreeMsg::Shutdown);
                    }
                }
                // Let the message above run and GStreamer wind its GL context
                // down before the window closes and the process exits.
                let window = self.window.clone();
                glib::timeout_add_local_once(std::time::Duration::from_millis(SHUTDOWN_GRACE_MS), move || {
                    window.close();
                });
            }

            AppMsg::ResizeBy { side, delta } => self.resize_by(side, delta),
            AppMsg::ResizeCommit => self.persist_session(),

            AppMsg::BookmarkEvent { side, event } => match event {
                BookmarkEvent::Open(path) => self.open_bookmark(side, path, &sender),
                BookmarkEvent::Edit(index) => self.edit_bookmark(side, index, &sender),
                BookmarkEvent::Delete(index) => {
                    if index < self.bookmarks.len() {
                        self.bookmarks.remove(index);
                        self.save_bookmarks();
                        self.refresh_all_bookmarks(&sender);
                    }
                }
                BookmarkEvent::Hide => self.set_bookmarks_visible(false, &sender),
            },
            AppMsg::BookmarkEditSaved { index, name, path } => {
                if let Some(bookmark) = self.bookmarks.get_mut(index) {
                    bookmark.name = name;
                    bookmark.path = path;
                    self.save_bookmarks();
                    self.refresh_all_bookmarks(&sender);
                }
            }
            AppMsg::ToggleBookmarks => self.set_bookmarks_visible(!self.show_bookmarks, &sender),
        }
    }
}

impl App {
    /// Apply a forwarded launch request.
    fn handle_launch(&mut self, command: Command, sender: &ComponentSender<Self>) {
        let hidden = command.hidden;

        // A width change is a side effect that never touches visibility. It
        // targets the named side, or the primary side when none is given.
        if let Some(width) = command.width {
            let side = command.side.unwrap_or(self.primary_side);
            match width {
                WidthArg::To(px) => self.set_width(side, px),
                WidthArg::By(delta) => self.resize_by(side, delta),
            }
            self.persist_session();
            if command.roots.is_empty() && command.reveal.is_empty() {
                return;
            }
        }

        if command.roots.is_empty() && command.reveal.is_empty() {
            // `--hidden` always means "hide", never toggle. Otherwise the
            // command is a show/toggle scoped to a side or to every dock.
            let intent = match (command.side, hidden) {
                (Some(side), true) => VisibilityIntent::HideSide(side),
                (Some(side), false) => VisibilityIntent::ToggleSide(side),
                (None, true) => VisibilityIntent::HideAll,
                (None, false) => VisibilityIntent::ToggleAll,
            };
            self.apply_visibility(intent, sender);
            return;
        }

        for root in &command.roots {
            if self.find_pane_with_dir(root).is_some() {
                // The directory is already open in some pane: never spawn a
                // duplicate — just keep it. The panel is shown below.
                continue;
            }
            let side = command.side.unwrap_or(self.primary_side);
            let di = self.ensure_dock(side, false, sender);
            self.add_pane(di, Some(root.clone()), sender);
        }

        // Reveal each `--select`/file argument: open a pane rooted at the
        // path's parent and select the path inside it. Reveals always open a
        // fresh pane on the default side (they carry a specific target, so
        // reusing an existing pane would lose it).
        for (root, select) in command.reveal_targets() {
            if !root.is_dir() {
                continue;
            }
            let side = command.side.unwrap_or(self.primary_side);
            let di = self.ensure_dock(side, false, sender);
            let id = self.add_pane(di, Some(root), sender);
            if let Some(select) = select
                && let Some((di, pi)) = self.dock_pane_of(id)
            {
                self.docks[di].panes[pi].tree.emit(TreeMsg::SelectPath(select));
            }
        }
        // Show the panel, unless this launch asked to stay hidden.
        self.set_docks_visible(!hidden);
    }

    /// Carry out a [`VisibilityIntent`] using the pure [`resolve_visibility`]
    /// plan. Creates a dock when the intent calls for it and seeds an empty one
    /// so "show" never reveals an empty shell.
    fn apply_visibility(&mut self, intent: VisibilityIntent, sender: &ComponentSender<Self>) {
        let existing: Vec<PanelSide> = self.docks.iter().map(|d| d.side).collect();
        let shown: Vec<PanelSide> = self
            .docks
            .iter()
            .filter(|d| d.window.is_visible())
            .map(|d| d.side)
            .collect();
        let plan = resolve_visibility(intent, &existing, &shown);

        if let Some(side) = plan.create {
            self.ensure_dock(side, plan.seed, sender);
        }
        for side in &plan.show {
            // A newly created dock starts hidden; reveal it. An emptied primary
            // dock is re-seeded so showing it is not an empty shell.
            let di = self.ensure_dock(*side, plan.seed, sender);
            self.docks[di].window.set_visible(true);
        }
        for side in &plan.hide {
            if let Some(di) = self.dock_of_side(*side) {
                self.docks[di].window.set_visible(false);
            }
        }
        // Any side not named by the plan keeps its current visibility.
        self.refresh_visible();
        // When a side was just shown, hand keyboard focus to its top pane so
        // the panel is immediately usable.
        let focus = plan
            .show
            .iter()
            .filter_map(|side| self.dock_of_side(*side))
            .filter_map(|di| self.docks[di].panes.last())
            .map(|pane| pane.tree.emit(TreeMsg::Focus))
            .next();
        let _ = focus;
    }

    /// Set `id` as the active pane in whichever dock holds it.
    fn set_active(&mut self, id: u64) {
        if let Some((di, _pi)) = self.dock_pane_of(id) {
            self.docks[di].active_pane = Some(id);
        }
    }

    /// Show the filter bar for pane `id` and focus its entry. The bar was just
    /// mapped, so an immediate `grab_focus()` no-ops (GTK applies visibility on
    /// the next layout pass) and keystrokes would land in the tree; defer it.
    fn open_filter(&mut self, id: u64) {
        if let Some((di, pi)) = self.dock_pane_of(id) {
            let pane = &mut self.docks[di].panes[pi];
            pane.filter_bar.set_visible(true);
            let entry = pane.filter_entry.clone();
            glib::idle_add_local_once(move || {
                entry.grab_focus();
            });
        }
    }

    /// Append `path` to the bookmarks (skipping a duplicate path) and persist.
    fn add_bookmark(&mut self, path: PathBuf) {
        if self.bookmarks.iter().any(|b| b.path == path) {
            self.status = format!("{} is already bookmarked", path.display());
            return;
        }
        self.bookmarks.push(Bookmark {
            name: Bookmark::default_name(&path),
            path: path.clone(),
        });
        self.save_bookmarks();
        self.status = format!("Bookmarked {}", path.display());
    }

    /// Open a new pane at a bookmark's directory in `side`'s dock.
    fn open_bookmark(&mut self, side: PanelSide, path: PathBuf, sender: &ComponentSender<Self>) {
        let path = crate::config::expand_bookmark_path(&path);
        if !path.is_dir() {
            self.status = format!("{} is not a directory", path.display());
            return;
        }
        // Already open somewhere? Focus that pane instead of duplicating it.
        if let Some((di, pi)) = self.find_pane_with_dir(&path) {
            let id = self.docks[di].panes[pi].id;
            self.set_active(id);
            self.docks[di].panes[pi].tree.emit(TreeMsg::Focus);
            self.set_docks_visible(true);
            return;
        }
        let di = self.dock_of_side(side).unwrap_or(0);
        self.add_pane(di, Some(path), sender);
    }

    /// Open the bookmark editor for the bookmark at `index` in `side`'s dock.
    fn edit_bookmark(&mut self, side: PanelSide, index: usize, sender: &ComponentSender<Self>) {
        let Some(bookmark) = self.bookmarks.get(index).cloned() else {
            return;
        };
        let Some(di) = self.dock_of_side(side) else { return };
        let parent = self.docks[di].window.clone();
        let sender = sender.clone();
        let path = crate::config::expand_bookmark_path(&bookmark.path);
        bookmarks::show_bookmark_editor(&parent, &bookmark.name, &path, move |name, path| {
            sender.input(AppMsg::BookmarkEditSaved { index, name, path });
        });
    }

    /// Save the bookmarks list to its file, reporting any failure.
    fn save_bookmarks(&mut self) {
        let path = bookmark_file_path(&self.config.bookmarks.file);
        if let Err(err) = save_bookmarks_to_path(&path, &self.bookmarks) {
            self.status = format!("Could not save bookmarks: {err:?}");
        }
    }

    /// Rebuild the bookmarks section in every dock (after a list change or a
    /// show/hide toggle).
    fn refresh_all_bookmarks(&mut self, sender: &ComponentSender<Self>) {
        for di in 0..self.docks.len() {
            self.refresh_bookmarks(di, sender);
        }
    }

    /// Show or hide the bookmarks strip everywhere. Hiding it when nothing else
    /// is open closes the panel, so it never leaves a blank surface behind.
    fn set_bookmarks_visible(&mut self, show: bool, sender: &ComponentSender<Self>) {
        self.show_bookmarks = show;
        self.refresh_all_bookmarks(sender);
        if !show {
            self.prune_empty_docks();
        }
    }

    /// Drop secondary docks that no longer show anything, and close the app if
    /// no pane is left anywhere.
    fn prune_empty_docks(&mut self) {
        let mut di = self.docks.len();
        while di > 1 {
            di -= 1;
            if self.docks[di].panes.is_empty() {
                let dock = self.docks.remove(di);
                dock.window.close();
            }
        }
        // The primary dock is the root window and cannot be removed; if nothing
        // is left to show anywhere, quit.
        if self.docks.iter().all(|dock| dock.panes.is_empty()) {
            self.window.close();
        }
    }

    /// Rebuild dock `di`'s bookmarks section. Mounts a freshly-built widget in
    /// the dock's outer box (above or below the pane stack) or removes it when
    /// the section is hidden.
    fn refresh_bookmarks(&mut self, di: usize, sender: &ComponentSender<Self>) {
        let widget = if self.show_bookmarks {
            let list = self.bookmarks.clone();
            let side = self.docks[di].side;
            let sender = sender.clone();
            let on_event: Rc<dyn Fn(BookmarkEvent)> =
                Rc::new(move |event| sender.input(AppMsg::BookmarkEvent { side, event }));
            Some(bookmarks::bookmarks_section(&list, on_event))
        } else {
            None
        };
        self.place_bookmark_widget(di, widget);
    }

    /// Install (or remove) dock `di`'s bookmarks widget at the configured
    /// top/bottom position.
    fn place_bookmark_widget(&mut self, di: usize, widget: Option<gtk::Widget>) {
        if let Some(old) = self.docks[di].bookmarks.take() {
            remove_from_parent(&old);
        }
        let Some(widget) = widget else { return };
        let stack = self.docks[di].container.clone();
        let outer = stack
            .parent()
            .and_downcast::<gtk::Box>()
            .unwrap_or_else(|| stack.clone());
        match self.config.bookmarks.position {
            BookmarkPosition::Top => outer.prepend(&widget),
            BookmarkPosition::Bottom => outer.insert_child_after(&widget, Some(&stack)),
        }
        self.docks[di].bookmarks = Some(widget);
    }

    /// Run a pane-level builtin (`Split View`, `Open Folder...`, `Filter...`,
    /// `Collapse`, `Close Pane`) against pane `id`. Shared by the toolbar and by
    /// keyboard shortcuts forwarded up from the tree.
    fn dispatch_pane_builtin(
        &mut self,
        id: u64,
        action: BuiltinAction,
        sender: &ComponentSender<Self>,
    ) {
        self.set_active(id);
        match action {
            BuiltinAction::OpenFolder => self.show_open_folder(id, sender),
            BuiltinAction::Filter => self.open_filter(id),
            BuiltinAction::SplitView => sender.input(AppMsg::SplitFromPane { id }),
            BuiltinAction::Up => self.go_up(id),
            BuiltinAction::Back => self.go_back(id),
            BuiltinAction::Forward => self.go_forward(id),
            BuiltinAction::Collapse => self.collapse_dock(id),
            BuiltinAction::ClosePane => sender.input(AppMsg::ClosePane { id }),
            BuiltinAction::ToggleBookmarks => sender.input(AppMsg::ToggleBookmarks),
            _ => {}
        }
    }

    /// Open the parent directory of pane `id`'s current root (`Up One Level`).
    /// A no-op at the filesystem root.
    fn go_up(&mut self, id: u64) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        let Some(parent) = self.docks[di].panes[pi]
            .root
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
        else {
            return;
        };
        self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(parent));
    }

    /// Step pane `id` back one entry in its history and reopen that root.
    fn go_back(&mut self, id: u64) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        if let Some(target) = self.docks[di].panes[pi].history.back() {
            self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(target));
        }
    }

    /// Step pane `id` forward one entry in its history and reopen that root.
    fn go_forward(&mut self, id: u64) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        if let Some(target) = self.docks[di].panes[pi].history.forward() {
            self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(target));
        }
    }

    /// Run a pane-menu shortcut resolved anywhere in pane `id`. Pane builtins go
    /// to [`Self::dispatch_pane_builtin`]; view builtins and custom commands are
    /// sent to the pane's tree (which targets the open directory).
    fn run_pane_shortcut(
        &mut self,
        id: u64,
        action: ContextAction,
        sender: &ComponentSender<Self>,
    ) {
        self.set_active(id);
        let Some(target) = ShortcutTarget::from_action(&action) else {
            return;
        };
        if let ShortcutTarget::Builtin(builtin) = &target
            && builtin.is_pane_action()
        {
            self.dispatch_pane_builtin(id, *builtin, sender);
            return;
        }
        if let Some((di, pi)) = self.dock_pane_of(id)
            && let Some(msg) = pane_item_message(&target)
        {
            self.docks[di].panes[pi].tree.emit(msg);
        }
    }

    /// Locate `(dock_index, pane_index)` for a pane id.
    fn dock_pane_of(&self, id: u64) -> Option<(usize, usize)> {
        self.docks.iter().enumerate().find_map(|(di, dock)| {
            dock.panes
                .iter()
                .position(|pane| pane.id == id)
                .map(|pi| (di, pi))
        })
    }

    /// Find a pane (across all docks) that already shows `path`, comparing
    /// canonical forms so that spellings like `./x` and `/a/x` dedupe. The
    /// canonical form of each pane root is cached on the pane, so this does no
    /// filesystem work for the common case.
    fn find_pane_with_dir(&self, path: &PathBuf) -> Option<(usize, usize)> {
        let canonical = std::fs::canonicalize(path).ok().unwrap_or_else(|| path.clone());
        self.docks.iter().enumerate().find_map(|(di, dock)| {
            dock.panes.iter().enumerate().find_map(|(pi, pane)| {
                if pane.canonical_root.as_deref() == Some(canonical.as_path()) {
                    Some((di, pi))
                } else {
                    None
                }
            })
        })
    }

    /// Index of the dock on `side`, if one exists.
    fn dock_of_side(&self, side: PanelSide) -> Option<usize> {
        self.docks.iter().position(|dock| dock.side == side)
    }

    /// Hide the entire dock that holds pane `id` (the toolbar "Collapse"
    /// action). The dock keeps its panes, so it comes back on the next show.
    fn collapse_dock(&mut self, id: u64) {
        if let Some((di, _)) = self.dock_pane_of(id) {
            self.set_dock_visible(di, false);
        }
    }

    /// Show/hide one dock.
    fn set_dock_visible(&mut self, di: usize, visible: bool) {
        self.docks[di].window.set_visible(visible);
        self.refresh_visible();
    }

    /// Show/hide every dock window.
    fn set_docks_visible(&mut self, visible: bool) {
        for dock in &self.docks {
            dock.window.set_visible(visible);
            // When the panel is shown, hand keyboard focus to its active pane.
            if visible
                && let Some(pane) = dock.panes.last()
            {
                pane.tree.emit(TreeMsg::Focus);
            }
        }
        self.visible = visible;
    }

    /// Recomputed flag: true while at least one dock is shown. Drives the
    /// no-argument toggle.
    fn refresh_visible(&mut self) {
        self.visible = self.docks.iter().any(|dock| dock.window.is_visible());
    }

    /// The current width of `side`'s dock.
    fn width_for(&self, side: PanelSide) -> u32 {
        self.widths.get(&side).copied().unwrap_or(self.config.panel.width)
    }

    /// Resize `side`'s dock to `width` px, clamped, telling its trees so inline
    /// thumbnails re-measure. Saves are debounced.
    fn set_width(&mut self, side: PanelSide, width: u32) {
        let width = width.clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH);
        if width == self.width_for(side) {
            return;
        }
        self.widths.insert(side, width);
        let panel = PanelConfig { width, ..self.config.panel };
        for dock in self.docks.iter().filter(|dock| dock.side == side) {
            apply_window_width(&dock.window, width);
            for pane in &dock.panes {
                pane.tree.emit(TreeMsg::SetPanel(panel));
            }
        }
        // Keep the config's copy in step with the primary dock.
        if side == self.primary_side {
            self.config.panel.width = width;
        }
        self.schedule_save();
    }

    /// Change `side`'s width by `delta` px (negative narrows).
    fn resize_by(&mut self, side: PanelSide, delta: i32) {
        let target = (self.width_for(side) as i64 + delta as i64)
            .clamp(PANEL_MIN_WIDTH as i64, PANEL_MAX_WIDTH as i64) as u32;
        self.set_width(side, target);
    }

    /// The session state to persist: last root plus both dock widths.
    fn session_state(&self) -> SessionState {
        SessionState {
            last_root: self.last_root.clone(),
            left_width: Some(self.width_for(PanelSide::Left)),
            right_width: Some(self.width_for(PanelSide::Right)),
        }
    }

    /// Persist session state now. Invalidates any pending debounced save.
    fn persist_session(&mut self) {
        self.save_generation.set(self.save_generation.get().wrapping_add(1));
        if let Err(err) = self.session_state().save() {
            self.status = format!("Could not save session: {err}");
        }
    }

    /// Persist session state after a short quiet period, coalescing the many
    /// width changes a drag produces into a single write. A later change bumps
    /// the generation, so only the newest scheduled save actually writes.
    fn schedule_save(&mut self) {
        let generation = self.save_generation.get().wrapping_add(1);
        self.save_generation.set(generation);
        let current = self.save_generation.clone();
        let state = self.session_state();
        glib::timeout_add_local_once(std::time::Duration::from_millis(SAVE_DEBOUNCE_MS), move || {
            if current.get() == generation {
                let _ = state.save();
            }
        });
    }

    /// Return the index of the dock on `side`, creating it (with a seeded pane
    /// from session state) when missing. The primary dock is always the
    /// config/default side.
    fn ensure_dock(&mut self, side: PanelSide, seed: bool, sender: &ComponentSender<Self>) -> usize {
        if let Some((di, _)) = self.docks.iter().enumerate().find(|(_, d)| d.side == side) {
            if seed && self.docks[di].panes.is_empty() {
                // The dock exists but was emptied; give it a pane again.
                let startup = self.config.startup.clone();
                if let Some(root) = default_root(&startup) {
                    self.add_pane(di, Some(root), sender);
                }
            }
            return di;
        }
        let config = &self.config;
        let width = self.width_for(side);
        let window = gtk::Window::new();
        init_layer_window(&window, config, side, width);
        window.set_default_size(width as i32, 520);
        if layer_shell_available() {
            attach_resize_controls(&window, side, sender);
        }
        attach_bookmarks_shortcut(&window, &config.pane_menu, sender);
        // The window's child is an outer box holding the pane stack; the
        // bookmarks section (when shown) is parented here too, so resetting the
        // stack never disturbs it.
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.set_vexpand(true);
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_vexpand(true);
        outer.append(&container);
        window.set_child(Some(&outer));

        let di = self.docks.len();
        self.docks.push(Dock {
            side,
            window,
            container,
            bookmarks: None,
            panes: Vec::new(),
            active_pane: None,
        });
        if seed {
            let startup = self.config.startup.clone();
            if let Some(root) = default_root(&startup) {
                self.add_pane(di, Some(root), sender);
            }
        }
        self.refresh_bookmarks(di, sender);
        di
    }

    /// Append a pane showing `root` (if any) to dock `di`, returning its id.
    fn add_pane(&mut self, di: usize, root: Option<PathBuf>, sender: &ComponentSender<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let parent = self.docks[di].window.clone();
        let side = self.docks[di].side;
        let width = self.width_for(side);
        let pane = make_pane(&self.config, parent, id, side, width, sender.clone());
        let mut pane = pane;
        if let Some(root) = &root {
            pane.tree.emit(TreeMsg::OpenRoot(root.clone()));
            pane.set_root(root.clone());
        }
        let dock = &mut self.docks[di];
        dock.panes.push(pane);
        dock.active_pane = Some(id);
        let container = dock.container.clone();
        fill_pane_container(&container, &dock.panes);
        // Give the new pane keyboard focus once GTK has allocated it, so the
        // app (and each new split) is usable without a click.
        let focus_sender = sender.clone();
        glib::idle_add_local_once(move || focus_sender.input(AppMsg::FocusPane { id }));
        id
    }

    fn show_open_folder(&self, id: u64, sender: &ComponentSender<Self>) {
        let sender = sender.clone();
        self.file_dialog.select_folder(
            None::<&gtk::Window>,
            None::<&gio::Cancellable>,
            move |result| {
                let path = result.ok().and_then(|file| file.path());
                sender.input(AppMsg::OpenFolderPicked { id, path });
            },
        );
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Map a configurable pane-menu item to the tree message that performs it.
/// Pane-level view actions have dedicated messages; custom commands are run
/// through the tree's shortcut path (which targets the open directory).
fn pane_item_message(target: &ShortcutTarget) -> Option<TreeMsg> {
    match target {
        ShortcutTarget::Builtin(action) => match action {
            BuiltinAction::ToggleHidden => Some(TreeMsg::ToggleHidden),
            BuiltinAction::SortByName => Some(TreeMsg::SetSortKey(SortKey::Name)),
            BuiltinAction::SortBySize => Some(TreeMsg::SetSortKey(SortKey::Size)),
            BuiltinAction::SortByModified => Some(TreeMsg::SetSortKey(SortKey::Modified)),
            BuiltinAction::SortByType => Some(TreeMsg::SetSortKey(SortKey::Type)),
            BuiltinAction::ToggleSortAscending => Some(TreeMsg::ToggleSortDirection),
            other => Some(TreeMsg::RunShortcut {
                target: ShortcutTarget::Builtin(*other),
            }),
        },
        ShortcutTarget::Command(_) => Some(TreeMsg::RunShortcut { target: target.clone() }),
    }
}

fn make_pane(
    config: &Config,
    parent: gtk::Window,
    id: u64,
    side: PanelSide,
    width: u32,
    sender: ComponentSender<App>,
) -> Pane {
    // The pane overlay: the toolbar's completion dropdown is added to it so it
    // can draw over the tree while the entry keeps keyboard focus.
    let overlay = gtk::Overlay::new();
    let toolbar = Toolbar::builder()
        .launch(ToolbarInit {
            overlay: overlay.clone(),
            pane_menu: config.pane_menu.clone(),
        })
        .forward(sender.input_sender(), move |out| AppMsg::PaneToolbar { id, out });
    let tree = Tree::builder()
        .launch(TreeInit {
            config: config.tree.clone(),
            parent,
            menu: config.context_menu.clone(),
            side,
            panel: PanelConfig { width, ..config.panel },
        })
        .forward(sender.input_sender(), move |out| AppMsg::PaneTree { id, out });

    let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
    widget.append(toolbar.widget());

    // ── per-pane filter row (hidden until requested) ────────────────────────
    let filter_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    filter_bar.add_css_class("filter-bar");
    filter_bar.set_visible(false);

    let filter_entry = gtk::SearchEntry::new();
    filter_entry.set_placeholder_text(Some("Filter..."));
    filter_entry.set_hexpand(true);
    filter_entry.set_valign(gtk::Align::Center);
    filter_entry.add_css_class("filter-entry");
    {
        let s = sender.clone();
        filter_entry.connect_search_changed(move |entry| {
            s.input(AppMsg::FilterChanged {
                id,
                filter: entry.text().to_string(),
            });
        });
    }
    filter_bar.append(&filter_entry);

    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.set_tooltip_text(Some("Clear filter"));
    close_btn.set_valign(gtk::Align::Center);
    close_btn.add_css_class("flat");
    {
        let s = sender.clone();
        close_btn.connect_clicked(move |_| {
            s.input(AppMsg::FilterClosed { id });
        });
    }
    filter_bar.append(&close_btn);

    widget.append(&filter_bar);

    let tree_widget = tree.widget();
    tree_widget.set_vexpand(true);
    widget.append(tree_widget);
    widget.set_vexpand(true);

    overlay.set_child(Some(&widget));

    // Pane-menu shortcuts are resolved at the pane level (capture phase), so
    // they work whether focus is on the tree, the path entry, the filter bar, or
    // nothing at all. Row shortcuts stay on the tree.
    {
        let shortcuts = Rc::new(PaneShortcuts::compile(&config.pane_menu));
        let s = sender.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            if let Some(action) = shortcuts.action_for(key, state) {
                s.input(AppMsg::PaneShortcut { id, action });
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        widget.add_controller(keys);
    }

    Pane {
        id,
        toolbar,
        tree,
        root: None,
        canonical_root: None,
        history: NavHistory::default(),
        filter_bar,
        filter_entry,
        widget: overlay,
    }
}

/// Remove `w` from whichever parent it currently has, using the parent's own
/// removal API instead of a raw `unparent()`.
///
/// This matters because a `GtkPaned` keeps its start/end child slots alive
/// across a raw `unparent()`, and its *deferred* destroy (GTK can keep a
/// container alive beyond `container.remove`) then unparents the widget a
/// second time — kicking it out of whatever new parent we re-appended it to.
fn remove_from_parent(w: &gtk::Widget) {
    let Some(parent) = w.parent() else { return };
    if let Ok(paned) = parent.clone().downcast::<gtk::Paned>() {
        let none: Option<&gtk::Widget> = None;
        if paned.start_child().is_some_and(|c| c == *w) {
            paned.set_start_child(none);
        }
        let none: Option<&gtk::Widget> = None;
        if paned.end_child().is_some_and(|c| c == *w) {
            paned.set_end_child(none);
        }
    } else if let Ok(b) = parent.downcast::<gtk::Box>() {
        b.remove(w);
    } else {
        w.unparent();
    }
}

fn pane_widget(pane: &Pane) -> gtk::Widget {
    pane.widget.clone().upcast()
}

/// Rebuild a dock's container Box to show its panes in a nested Paned
/// structure.
///
/// Layout for N panes:
///   N=1 → container has the single pane widget.
///   N=2 → container has Paned { pane[0], pane[1] }
///   N=3 → container has Paned { pane[0], Paned { pane[1], pane[2] } }
///   etc.
///
/// Called on every split/close so widget references are always fresh.
fn fill_pane_container(container: &gtk::Box, panes: &[Pane]) {
    // Detach the pane boxes from their old parents *first*, using each
    // parent's removal API. A raw `unparent()` alone is not enough: a Paned
    // keeps stale child slots that re-unparent the widget when the old chain
    // is finally destroyed (possibly after we have re-appended it).
    for pane in panes {
        remove_from_parent(pane.widget.upcast_ref());
    }
    // Now drop the old structure wholesale.
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    match panes {
        [] => {}
        [single] => {
            let w = pane_widget(single);
            w.set_vexpand(true);
            container.append(&w);
        }
        panes => {
            // Build a right-nested Paned from the last two, then keep
            // wrapping from right to left.
            let n = panes.len();
            let last_w = pane_widget(&panes[n - 1]);
            last_w.set_vexpand(true);
            let mut right: gtk::Widget = last_w.clone().upcast();
            for pane in panes[..n - 1].iter().rev() {
                let left_w = pane_widget(pane);
                left_w.set_vexpand(true);
                let split = gtk::Paned::new(gtk::Orientation::Vertical);
                split.set_vexpand(true);
                split.set_wide_handle(true);
                // Allow both children to shrink and resize freely.
                split.set_shrink_start_child(true);
                split.set_shrink_end_child(true);
                split.set_resize_start_child(true);
                split.set_resize_end_child(true);
                split.set_start_child(Some(&left_w));
                split.set_end_child(Some(&right));
                // Default to 50/50 split: set position after a short delay once
                // GTK has allocated space and computed max_position.
                {
                    let split2 = split.clone();
                    relm4::gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(PANED_CENTER_DELAY_MS),
                        move || {
                            // max_position is INT_MAX until the widget is allocated;
                            // once it has a real allocation, use half the actual height.
                            let alloc = split2.height();
                            if alloc > 0 {
                                split2.set_position(alloc / 2);
                            }
                        },
                    );
                }
                right = split.upcast();
            }
            container.append(&right);
        }
    }
}

/// Clear the media stream of every `GtkVideo` under `widget`, releasing its
/// GStreamer GL sink. Walking the widget tree (rather than the model) means a
/// video also stops when the pane that built it has already been removed from
/// `App::docks` — a closed last pane keeps its widgets parented until the
/// window itself is destroyed.
fn stop_video_widgets(widget: &gtk::Widget) {
    if let Some(video) = widget.downcast_ref::<gtk::Video>() {
        video.set_media_stream(None::<&gtk::MediaStream>);
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        stop_video_widgets(&current);
        child = current.next_sibling();
    }
}

/// Pixels added/removed per keyboard or CLI width increment.
const WIDTH_STEP: i32 = 24;

/// Quiet period before a debounced session save is written after a resize.
const SAVE_DEBOUNCE_MS: u64 = 300;

/// Delay before a freshly split `GtkPaned` is centred at 50/50, once GTK has
/// allocated it (before allocation `max_position` is `INT_MAX`).
const PANED_CENTER_DELAY_MS: u64 = 100;

/// Grace period between tearing media down and closing the window on quit,
/// letting GStreamer release its GL context before the process exits.
const SHUTDOWN_GRACE_MS: u64 = 150;

/// Resize a dock window's layer surface to `width`.
fn apply_window_width(window: &gtk::Window, width: u32) {
    let (_, height) = window.default_size();
    // `set_exclusive_zone` only means anything for a layer surface; on the
    // plain-window fallback it would warn, so resize the window directly.
    if window.is_layer_window() {
        window.set_exclusive_zone(width as i32);
    }
    window.set_size_request(width as i32, -1);
    window.set_default_size(width as i32, if height > 0 { height } else { -1 });
}

/// Wire a dock window's configurable bookmarks shortcut. It lives on the window
/// (capture phase) so it fires from anywhere in the dock — including when the
/// bookmarks strip is the only thing shown and no pane is focused.
fn attach_bookmarks_shortcut(window: &gtk::Window, menu: &PaneMenu, sender: &ComponentSender<App>) {
    let shortcuts = Rc::new(PaneShortcuts::compile(menu));
    let s = sender.clone();
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, state| {
        if matches!(
            shortcuts.action_for(key, state),
            Some(ContextAction::Builtin(BuiltinAction::ToggleBookmarks))
        ) {
            s.input(AppMsg::ToggleBookmarks);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);
}

/// Wire the interactive width controls into a dock window: Super+right-drag to
/// resize and Super+plus/minus to step the width.
fn attach_resize_controls(window: &gtk::Window, side: PanelSide, sender: &ComponentSender<App>) {
    attach_resize_drag(window, side, sender.input_sender().clone());
    attach_resize_keys(window, side, sender.input_sender().clone());
}

fn attach_resize_drag(window: &gtk::Window, side: PanelSide, sender: relm4::Sender<AppMsg>) {
    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_SECONDARY);
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    // (was Super held at drag start, cumulative offset of the last handled update)
    let state = Rc::new(RefCell::new((false, 0.0f64)));

    let begin_state = state.clone();
    drag.connect_drag_begin(move |gesture, _, _| {
        let super_held = gesture
            .current_event_state()
            .contains(gdk::ModifierType::SUPER_MASK);
        *begin_state.borrow_mut() = (super_held, 0.0);
    });

    let update_sender = sender.clone();
    let update_state = state.clone();
    drag.connect_drag_update(move |_, offset_x, _| {
        let mut state = update_state.borrow_mut();
        if !state.0 {
            return;
        }
        let step = offset_x - state.1;
        state.1 = offset_x;
        // A left dock grows as the pointer moves right; a right dock mirrors it.
        let widen = match side {
            PanelSide::Left => step,
            PanelSide::Right => -step,
        };
        let delta = widen.round() as i32;
        if delta != 0 {
            let _ = update_sender.send(AppMsg::ResizeBy { side, delta });
        }
    });

    let end_sender = sender;
    let end_state = state.clone();
    drag.connect_drag_end(move |_, _, _| {
        if end_state.borrow().0 {
            let _ = end_sender.send(AppMsg::ResizeCommit);
        }
    });

    window.add_controller(drag);
}

fn attach_resize_keys(window: &gtk::Window, side: PanelSide, sender: relm4::Sender<AppMsg>) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(move |_, key, _, state| {
        if !state.contains(gdk::ModifierType::SUPER_MASK) {
            return glib::Propagation::Proceed;
        }
        match key {
            gdk::Key::minus | gdk::Key::underscore | gdk::Key::KP_Subtract => {
                let _ = sender.send(AppMsg::ResizeBy { side, delta: -WIDTH_STEP });
                glib::Propagation::Stop
            }
            gdk::Key::equal | gdk::Key::plus | gdk::Key::KP_Add => {
                let _ = sender.send(AppMsg::ResizeBy { side, delta: WIDTH_STEP });
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(keys);
}

/// Whether the running compositor implements the `wlr-layer-shell` protocol.
///
/// Hyprland, sway, river, niri and other wlroots-based compositors do; GNOME's
/// Mutter and KDE's KWin do not. Without it the panel cannot dock, reserve an
/// exclusive zone or use layer stacking, so it degrades to an ordinary window
/// (see [`init_layer_window`]). Cached after the first call, since it cannot
/// change while the app runs.
fn layer_shell_available() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(gtk4_layer_shell::is_supported)
}

/// Configure `window` as a layer-shell surface docked to `side`.
///
/// When the compositor has no `wlr-layer-shell` support the window is left as a
/// plain, decorated, freely-floating toplevel (a usable, undocked fallback
/// rather than a silently broken layer); check [`layer_shell_available`] when
/// docked-only behavior (like interactive resize) must be skipped.
fn init_layer_window(window: &gtk::Window, config: &Config, side: PanelSide, width: u32) {
    if !layer_shell_available() {
        configure_plain_window(window, width);
        return;
    }
    if window.is_layer_window() {
        return;
    }
    window.init_layer_shell();
    window.set_layer(layer_of(config.panel.layer));
    let edge = match side {
        PanelSide::Left => Edge::Left,
        PanelSide::Right => Edge::Right,
    };
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, true);
    window.set_anchor(edge, true);
    window.set_exclusive_zone(width as i32);
    window.set_size_request(width as i32, -1);
    let margin = config.panel.margin as i32;
    window.set_margin(Edge::Top, margin);
    window.set_margin(Edge::Bottom, margin);
    window.set_margin(Edge::Left, margin);
    window.set_margin(Edge::Right, margin);

    // Keyboard focus is `OnDemand`: the compositor grants it when the layer is
    // mapped (so launching or showing the panel focuses it) and while the
    // pointer moves over it, and the claim is released the moment the pointer
    // leaves.
    //
    // Releasing on pointer-leave matters because a Hyprland layer surface that
    // *keeps* keyboard focus does not update the compositor's notion of the
    // focused window (`hyprctl activewindow` still reports the previous one).
    // A click only triggers a refocus when the clicked window differs from that
    // focused window (`CInputManager::processMouseDownNormal`:
    // `focusState()->window() != w`), so a panel that holds the claim makes
    // clicking the previously-focused window a no-op — keyboard focus never
    // returns to it. Dropping the claim kicks the layer from the seat, so the
    // next click refocuses the window normally.
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    let motion = gtk::EventControllerMotion::new();
    {
        let kb_window = window.clone();
        motion.connect_enter(move |_, _, _| kb_window.set_keyboard_mode(KeyboardMode::OnDemand));
    }
    {
        let kb_window = window.clone();
        motion.connect_leave(move |_| {
            // A row/menu popover is a child surface, so opening it fires a
            // pointer-leave for the panel even though the user has not left it.
            // Keep the keyboard claim in that case; the compositor returns
            // focus to the panel when the popover closes.
            if has_visible_popover(kb_window.upcast_ref()) {
                return;
            }
            kb_window.set_keyboard_mode(KeyboardMode::None);
        });
    }
    window.add_controller(motion);
}

/// Fall back to a plain, decorated, freely-floating window when the compositor
/// has no layer-shell support. The panel cannot dock, so it opens at its
/// configured width with a normal title bar; the user places it like any other
/// window. Announced once so the reason for the un-docked panel is not a
/// mystery.
fn configure_plain_window(window: &gtk::Window, width: u32) {
    static ANNOUNCED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ANNOUNCED.get_or_init(|| {
        eprintln!(
            "tree-space: this compositor does not support wlr-layer-shell, so the panel \
             cannot dock; opening it as a normal window instead."
        );
    });
    window.set_title(Some("tree-space"));
    window.set_default_size(width as i32, 720);
    window.set_size_request(width as i32, -1);
}

/// Whether any visible [`gtk::Popover`] is open under `widget` (walking the
/// widget tree). Used to keep keyboard focus on the panel while its menu is up.
fn has_visible_popover(widget: &gtk::Widget) -> bool {
    if widget.is::<gtk::Popover>() && widget.is_visible() {
        return true;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        if has_visible_popover(&current) {
            return true;
        }
        child = current.next_sibling();
    }
    false
}

fn layer_of(layer: PanelLayer) -> Layer {
    use PanelLayer::*;
    match layer {
        Background => Layer::Background,
        Bottom => Layer::Bottom,
        Top => Layer::Top,
        Overlay => Layer::Overlay,
    }
}

/// Load the user stylesheet (falling back to the shipped default) plus the
/// dynamically-sized font rule, which is appended last so `tree.font_size`
/// still wins over anything the stylesheet sets.
fn install_css(config: &Config) {
    let stylesheet = load_stylesheet();
    if let Some(problem) = &stylesheet.problem {
        eprintln!("tree-space: could not read stylesheet: {problem:?}");
    }
    let css = format!(
        "{}\n.tree-row label, .tree-rename-entry, .tree-menu, .hamburger-menu {{ font-size: {}px; }}",
        stylesheet.css, config.tree.font_size
    );
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&css);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The root a freshly-seeded dock should show, resolved from the `[startup]`
/// config (last-used directory, home, or a fixed path), falling back to home.
fn default_root(startup: &StartupRoot) -> Option<PathBuf> {
    // A bookmarks launch opens no directory pane; seeding one would defeat it.
    if startup.is_bookmarks() {
        return None;
    }
    let last = SessionState::load().last_root.filter(|p| p.is_dir());
    startup.resolve(last).filter(|p| p.is_dir()).or_else(home_dir)
}
#[cfg(test)]
mod visibility_tests {
    use super::*;

    const L: PanelSide = PanelSide::Left;
    const R: PanelSide = PanelSide::Right;

    fn plan(intent: VisibilityIntent, existing: &[PanelSide], shown: &[PanelSide]) -> VisibilityPlan {
        resolve_visibility(intent, existing, shown)
    }

    #[test]
    fn show_all_shows_both() {
        let p = plan(VisibilityIntent::ShowAll, &[L], &[]);
        // Only docks that already exist are shown; a second one is not created.
        assert_eq!(p.show, vec![L]);
        assert!(p.seed);
        let both = plan(VisibilityIntent::ShowAll, &[L, R], &[]);
        assert_eq!(both.show, vec![L, R]);
    }

    #[test]
    fn hide_all_hides_both() {
        let p = plan(VisibilityIntent::HideAll, &[L, R], &[L, R]);
        assert_eq!(p.hide, vec![L, R]);
        assert!(p.show.is_empty());
    }

    #[test]
    fn toggle_all_shows_when_nothing_visible_and_hides_otherwise() {
        let shown = plan(VisibilityIntent::ToggleAll, &[L], &[]);
        // Toggling all shows the docks that exist (not a newly created side).
        assert_eq!(shown.show, vec![L]);
        let hidden = plan(VisibilityIntent::ToggleAll, &[L, R], &[L]);
        assert_eq!(hidden.hide, vec![L, R]);
    }

    #[test]
    fn show_side_creates_a_missing_dock() {
        let p = plan(VisibilityIntent::ShowSide(R), &[L], &[L]);
        assert_eq!(p.create, Some(R));
        assert_eq!(p.show, vec![R]);
    }

    #[test]
    fn hide_side_never_creates() {
        let p = plan(VisibilityIntent::HideSide(R), &[L], &[L]);
        assert_eq!(p.create, None);
        assert_eq!(p.hide, vec![R]);
    }

    #[test]
    fn toggle_side_is_granular() {
        // Missing -> create and show.
        let create = plan(VisibilityIntent::ToggleSide(R), &[L], &[L]);
        assert_eq!(create.create, Some(R));
        assert_eq!(create.show, vec![R]);
        // Shown -> hide.
        let hide = plan(VisibilityIntent::ToggleSide(L), &[L, R], &[L, R]);
        assert_eq!(hide.hide, vec![L]);
        // Hidden -> show.
        let show = plan(VisibilityIntent::ToggleSide(L), &[L, R], &[R]);
        assert_eq!(show.show, vec![L]);
    }

    #[test]
    fn toggling_one_side_leaves_the_other_alone() {
        // Left visible, right hidden: toggling right must not name left at all.
        let p = plan(VisibilityIntent::ToggleSide(R), &[L, R], &[L]);
        assert_eq!(p.show, vec![R]);
        assert!(!p.hide.contains(&L));
    }
}

#[cfg(test)]
mod nav_history_tests {
    use super::NavHistory;
    use std::path::{Path, PathBuf};

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn records_visits_in_order_and_navigates_both_ways() {
        let mut h = NavHistory::default();
        h.record(Path::new("/a"));
        h.record(Path::new("/b"));
        h.record(Path::new("/c"));
        assert!(h.can_back());
        assert!(!h.can_forward());

        assert_eq!(h.back(), Some(p("/b")));
        h.finish_navigation();
        assert_eq!(h.back(), Some(p("/a")));
        h.finish_navigation();
        assert!(!h.can_back());
        assert!(h.can_forward());

        assert_eq!(h.forward(), Some(p("/b")));
        h.finish_navigation();
        assert_eq!(h.forward(), Some(p("/c")));
        h.finish_navigation();
        assert!(h.can_back());
        assert!(!h.can_forward());
    }

    #[test]
    fn a_new_visit_after_going_back_truncates_the_forward_tail() {
        let mut h = NavHistory::default();
        h.record(Path::new("/a"));
        h.record(Path::new("/b"));
        h.record(Path::new("/c"));
        assert_eq!(h.back(), Some(p("/b")));
        h.finish_navigation();
        // Visiting /d from /b drops /c from the forward history.
        h.record(Path::new("/d"));
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(p("/b")));
    }

    #[test]
    fn navigating_does_not_record_and_repeats_are_ignored() {
        let mut h = NavHistory::default();
        h.record(Path::new("/a"));
        h.record(Path::new("/b"));
        // Back, then the resulting RootChanged; recording must be suppressed.
        let target = h.back().unwrap();
        assert_eq!(target, p("/a"));
        h.record(&target);
        h.finish_navigation();
        // Still at /a with /b ahead, and no duplicate /a entry was appended.
        assert!(h.can_forward());
        assert_eq!(h.forward(), Some(p("/b")));
        h.finish_navigation();

        // Re-recording the current entry is a no-op.
        h.record(Path::new("/b"));
        assert_eq!(h.cursor, 1);
        assert_eq!(h.entries.len(), 2);
    }

    #[test]
    fn cannot_navigate_an_empty_history() {
        let mut h = NavHistory::default();
        assert!(!h.can_back());
        assert!(!h.can_forward());
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), None);
    }
}
