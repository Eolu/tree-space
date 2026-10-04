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

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
};

use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use relm4::gtk::{gdk, gio, glib, prelude::*};
use relm4::prelude::*;

use crate::cmd::{Command, WidthArg};
use crate::config::{
    Bookmark, BuiltinAction, Config, ContextAction, PANEL_MAX_WIDTH, PANEL_MIN_WIDTH, PanelConfig,
    PanelLayer, PanelSide, SessionState, ShortcutTarget, StartupRoot, WorkspaceMove,
    bookmark_file_path, load_stylesheet, save_bookmarks_to_path,
};
use crate::workspace::Hyprland;
use crate::fs::SortKey;
use crate::ipc;
use crate::ui::bookmarks::{self, BookmarkEvent, MoveTarget};
use crate::ui::toolbar::{
    PaneShortcuts, Toolbar, ToolbarInit, ToolbarMsg, ToolbarOutput, parse_accelerator,
};
use crate::ui::tree::{Tree, TreeInit, TreeOutput, TreeMsg};

/// One entry in a pane's history: a directory it showed, or the bookmarks view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ViewEntry {
    Dir(PathBuf),
    Bookmarks,
}

/// A pane's back/forward navigation history.
///
/// Views are recorded in visit order with a cursor into the list. Going back
/// moves the cursor left, forward moves it right; visiting a *new* view (not via
/// back/forward) truncates the forward tail and appends. This is a pure data
/// structure so the rules can be unit-tested without a display.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NavHistory {
    entries: Vec<ViewEntry>,
    /// Index of the current entry. Meaningless while `entries` is empty.
    cursor: usize,
    /// Set while a back/forward navigation is in flight, so the `RootChanged`
    /// (or view switch) it produces is not itself recorded as a new visit.
    navigating: bool,
}

impl NavHistory {
    /// Record a newly shown view. A repeat of the current entry is ignored; any
    /// forward history is dropped. While a back/forward is navigating this is a
    /// no-op (the target is already in the list).
    fn record(&mut self, entry: ViewEntry) {
        if self.navigating {
            return;
        }
        if self.entries.get(self.cursor).is_some_and(|cur| cur == &entry) {
            return;
        }
        if self.entries.is_empty() {
            self.entries.push(entry);
            self.cursor = 0;
            return;
        }
        self.entries.truncate(self.cursor + 1);
        self.entries.push(entry);
        self.cursor = self.entries.len() - 1;
    }

    fn can_back(&self) -> bool {
        self.cursor > 0 && self.cursor < self.entries.len()
    }

    fn can_forward(&self) -> bool {
        !self.entries.is_empty() && self.cursor + 1 < self.entries.len()
    }

    /// Step back one entry and return it, arming `navigating`.
    fn back(&mut self) -> Option<ViewEntry> {
        if !self.can_back() {
            return None;
        }
        self.cursor -= 1;
        self.navigating = true;
        Some(self.entries[self.cursor].clone())
    }

    /// Step forward one entry and return it, arming `navigating`.
    fn forward(&mut self) -> Option<ViewEntry> {
        if !self.can_forward() {
            return None;
        }
        self.cursor += 1;
        self.navigating = true;
        Some(self.entries[self.cursor].clone())
    }

    /// Clear the in-flight flag once the resulting view has been shown.
    fn finish_navigation(&mut self) {
        self.navigating = false;
    }
}

/// One split view inside a dock: its own top bar above its own body.
pub struct Pane {
    id: u64,
    toolbar: Controller<Toolbar>,
    tree: Controller<Tree>,
    /// The workspace this pane belongs to (a Hyprland workspace name). A pane
    /// is rendered only while its workspace is the active one.
    workspace: String,
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
    /// Switches the pane body between the tree and the bookmarks view. A pane
    /// created without a directory starts on the bookmarks page (a "new panel"
    /// suggesting places to jump to).
    body: gtk::Stack,
    /// The bookmarks list container (the bookmarks page of `body`), refilled
    /// whenever the list changes.
    bookmarks_list: gtk::Box,
    /// Case-insensitive filter applied to the bookmarks list. Kept per pane so
    /// the filter bar works on the bookmarks view too.
    bookmark_filter: String,
    /// Shared drag state for the bookmarks list (survives row rebuilds).
    bookmark_drag: Rc<bookmarks::BookmarkDrag>,
    /// Keyboard cursor and selection for the bookmarks list.
    bookmark_nav: Rc<bookmarks::BookmarkNav>,
    /// The pane overlay: its main child is the `{ toolbar, filter_bar?, body }`
    /// vertical box, and its overlay children hold panes' popovers (e.g. the
    /// path-entry completion dropdown). Built once and reused across split/close
    /// rebuilds, so the tree and toolbar widgets never need to be reparented
    /// (which would trip `gtk_box_append: child has a parent`).
    widget: gtk::Overlay,
    /// The toolbar's drag grip. Kept so a pane-move drag can compute the grip's
    /// position within whatever window the pane currently lives in (which
    /// changes when the pane is moved to another dock).
    grip: gtk::Image,
    /// The navigation-toolbar buttons when `[panel] nav_toolbar` is enabled;
    /// their enabled state tracks the pane's root and history.
    nav_buttons: Option<NavButtons>,
    /// The pane-menu accelerators, kept so the IPC `--key` command can resolve
    /// a shortcut against this pane without a real key event.
    shortcuts: Rc<PaneShortcuts>,
}

/// The optional navigation toolbar's buttons.
struct NavButtons {
    up: gtk::Button,
    back: gtk::Button,
    forward: gtk::Button,
}

impl Pane {
    /// Record `root` as this pane's directory and refresh its cached canonical
    /// form.
    fn set_root(&mut self, root: PathBuf) {
        self.canonical_root = std::fs::canonicalize(&root).ok();
        self.root = Some(root);
        self.refresh_nav();
    }

    /// Show the bookmarks view in this pane's body (and switch its hamburger).
    fn show_bookmarks(&self) {
        // The tree stays alive but hidden; stop its previews so a playing
        // video or audio file doesn't keep making noise behind the bookmarks.
        self.tree.emit(TreeMsg::Suspend);
        self.body.set_visible_child_name("bookmarks");
        self.toolbar.emit(ToolbarMsg::SetBookmarks(true));
        self.refresh_nav();
    }

    /// Show the tree in this pane's body (and switch its hamburger).
    fn show_tree(&self) {
        self.body.set_visible_child_name("tree");
        self.toolbar.emit(ToolbarMsg::SetBookmarks(false));
        self.refresh_nav();
    }

    /// Whether this pane is currently on the bookmarks view.
    fn on_bookmarks(&self) -> bool {
        self.body.visible_child_name().as_deref() == Some("bookmarks")
    }

    /// Enable the navigation toolbar's buttons to match what the pane can
    /// actually do: Up needs a parent directory and a tree (not the bookmarks
    /// view); Back/Forward need history in that direction. A no-op when the
    /// toolbar is disabled.
    fn refresh_nav(&self) {
        let Some(nav) = &self.nav_buttons else {
            return;
        };
        let has_parent = !self.on_bookmarks()
            && self.root.as_deref().and_then(Path::parent).is_some();
        nav.up.set_sensitive(has_parent);
        nav.back.set_sensitive(self.history.can_back());
        nav.forward.set_sensitive(self.history.can_forward());
    }
}

/// One layer-shell dock window anchored to a screen edge, with its panes.
///
/// A dock is identified by its `(side, monitor)` pair, so a second dock can
/// share a side on another monitor when a pane is dragged there.
struct Dock {
    side: PanelSide,
    /// The monitor the surface landed on, resolved once it is mapped. `None`
    /// until then (and when the compositor exposes no monitor information).
    monitor: Option<gdk::Monitor>,
    /// The dock's window. For the primary dock this is the relm4 root window;
    /// for additional docks an imperatively-built `gtk::Window`.
    window: gtk::Window,
    /// The vertical box holding the pane stack (for the primary dock this is
    /// `App::pane_container`, referenced by `view!`).
    container: gtk::Box,
    panes: Vec<Pane>,
    /// Id of the pane most recently interacted with in this dock.
    active_pane: Option<u64>,
}

/// Live state of a pane-move drag started from a toolbar grip.
struct PaneDrag {
    /// The pane being dragged.
    id: u64,
    /// The pointer's global position when the drag began; every update is this
    /// plus the gesture's offset.
    base: (f64, f64),
    /// Index of the dock currently highlighted as the drop target, if any.
    highlighted: Option<usize>,
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
    /// Hyprland reported a new active workspace (from the IPC event stream).
    ActiveWorkspace(String),
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
    /// A pane-move grip drag began: `start` is the press point relative to the
    /// grip.
    PaneDragBegin { id: u64, start: (f64, f64) },
    /// The pointer moved `offset` (relative to the drag start) during a drag.
    PaneDragUpdate { id: u64, offset: (f64, f64) },
    /// A pane drag ended at `offset`: move the pane to the target under the
    /// pointer.
    PaneDragEnd { id: u64, offset: (f64, f64) },
    /// Resolve a dock's monitor once its layer surface is mapped.
    ResolveDockMonitor { di: usize },
    /// Move keyboard focus into pane `id`'s tree (after it is allocated).
    FocusPane { id: u64 },
    /// Focus the active pane of every visible dock (a window was just mapped,
    /// e.g. the launch surface), so the keyboard works without a click.
    FocusVisible,
    /// The bookmarks view in pane `id` reported a user action.
    BookmarkEvent { id: u64, event: BookmarkEvent },
    /// The bookmark editor for the entry at `index_path` was saved.
    BookmarkEditSaved { index_path: Vec<usize>, name: String, path: Option<PathBuf> },
    /// A new leaf bookmark was created from the bookmarks view.
    BookmarkAdded { name: String, path: PathBuf },
    /// A new (empty) bookmark folder was created from the bookmarks view.
    BookmarkFolderAdded { name: String },
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
    /// Hyprland IPC, when running under Hyprland. `None` means no workspace
    /// scoping: every pane shares the empty workspace name and is always shown.
    hypr: Option<Hyprland>,
    /// The active workspace (a Hyprland name), as last reported. Panes are
    /// shown only while their own workspace matches this.
    active_workspace: String,
    /// Next pane id; bumped on every pane creation so ids never repeat.
    next_id: u64,
    /// Workspaces where the user explicitly hid the panel (via `ts --hidden`
    /// or collapsing). A workspace absent from the set is shown. Hiding on one
    /// workspace never affects the others.
    hidden_workspaces: HashSet<String>,
    /// Per-side dock width in pixels. Starts from session state (falling back to
    /// `[panel] width`) and changes on interactive resize — one entry per side,
    /// so the two docks are sized independently without parallel scalar fields.
    widths: HashMap<PanelSide, u32>,
    /// Most recently opened root, kept so a width save never drops it.
    last_root: Option<PathBuf>,
    /// The bookmarks list, loaded from the bookmarks file and written back
    /// whenever it changes.
    bookmarks: Vec<Bookmark>,
    /// The pane-move drag in flight, if any.
    pane_drag: Option<PaneDrag>,
    /// Monotonic id for the debounced width save: a scheduled save only writes
    /// if it is still the latest (no `SourceId` juggling — removing a one-shot
    /// source that has already fired panics).
    save_generation: Rc<Cell<u64>>,
    /// A handle to this component's own input, kept so helpers can schedule a
    /// deferred [`AppMsg::FocusPane`] (focus must land after the newly-shown
    /// body page is laid out).
    sender: ComponentSender<App>,
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
        // Panes are scoped to the workspace they are opened on. Under Hyprland
        // we follow the active workspace over IPC; without it every pane shares
        // the empty workspace (i.e. all panes are visible, as before).
        let hypr = Hyprland::connect();
        let active_workspace =
            hypr.as_ref().and_then(Hyprland::active_workspace).unwrap_or_default();
        // Build the primary dock's initial panes from the invocation. With no
        // roots, resolve the configured startup directory; a `bookmarks` startup
        // (the default) resolves to none, so the pane opens the bookmarks view.
        let roots = if !init.command.roots.is_empty() {
            init.command.roots.clone()
        } else if config.startup.is_bookmarks() {
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
            let pane = make_pane(
                &config,
                parent.clone(),
                next_id,
                primary_side,
                primary_width,
                active_workspace.clone(),
                sender.clone(),
            );
            pane.tree.emit(TreeMsg::OpenRoot(root.clone()));
            panes.push(pane);
            panes.last_mut().unwrap().set_root(root.clone());
            next_id += 1;
        }
        // Guarantee at least one pane. With no roots it opens the bookmarks view
        // (a "new panel" suggesting places to jump to).
        if panes.is_empty() {
            let mut pane = make_pane(
                &config,
                parent.clone(),
                next_id,
                primary_side,
                primary_width,
                active_workspace.clone(),
                sender.clone(),
            );
            pane.show_bookmarks();
            pane.history.record(ViewEntry::Bookmarks);
            pane.refresh_nav();
            panes.push(pane);
            next_id += 1;
        }

        let pane_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        pane_container.set_vexpand(true);
        fill_pane_container(&pane_container, &panes, &active_workspace);

        let mut status = String::new();
        if let Some(problem) = loaded.problem {
            status = format!("config: {problem:?}");
        } else if let Some(problem) = bookmark_problem {
            status = format!("bookmarks: {problem:?}");
        }

        // A hidden launch hides the panel only on the workspace it starts on.
        let hidden_workspaces = if init.command.hidden {
            HashSet::from([active_workspace.clone()])
        } else {
            HashSet::new()
        };

        let mut model = App {
            config,
            status,
            window: root.clone(),
            file_dialog: gtk::FileDialog::new(),
            docks: vec![Dock {
                side: primary_side,
                monitor: None,
                window: root.clone(),
                container: pane_container.clone(),
                panes,
                active_pane: None,
            }],
            pane_container,
            primary_side,
            hypr: hypr.clone(),
            active_workspace,
            next_id,
            hidden_workspaces,
            widths,
            last_root: session.last_root.clone(),
            bookmarks,
            pane_drag: None,
            save_generation: Rc::new(Cell::new(0)),
            sender: sender.clone(),
        };

        // Follow workspace changes so the panel hides when its workspace is not
        // the active one and returns when it is.
        if let Some(hypr) = &hypr {
            let workspace_sender = sender.clone();
            hypr.spawn_watcher(move |name| {
                let sender = workspace_sender.clone();
                glib::MainContext::default().invoke(move || {
                    sender.input(AppMsg::ActiveWorkspace(name));
                });
            });
        }

        init_layer_window(&model.window, &model.config, primary_side, primary_width, None);
        install_css(&model.config);
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

        // A mapped surface takes keyboard focus, so launching the panel leaves
        // the keyboard on the first row without a click (the compositor grants
        // the layer's keyboard on map; this focuses the row within it).
        {
            let sender = sender.clone();
            model.window.connect_map(move |_| {
                let sender = sender.clone();
                // The surface now exists, so its monitor can be resolved.
                sender.input(AppMsg::ResolveDockMonitor { di: 0 });
                glib::idle_add_local_once(move || sender.input(AppMsg::FocusVisible));
            });
        }

        // Populate every pane's bookmarks list now that the order is settled.
        model.refresh_all_bookmarks(&sender);

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
                    self.open_root_in_pane(id, path);
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
                ToolbarOutput::PaneItem(action) => self.run_pane_shortcut(id, action, &sender),
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
                        pane.history.record(ViewEntry::Dir(root.clone()));
                        pane.history.finish_navigation();
                        pane.show_tree();
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
                self.open_root_in_pane(id, path);
            }
            AppMsg::OpenFolderPicked { path: None, .. } => {}

            AppMsg::LaunchRequest { command } => {
                self.handle_launch(command, &sender);
            }

            AppMsg::ActiveWorkspace(name) => self.on_active_workspace(name),

            AppMsg::SplitFromPane { id } => {
                // A new panel with no path opens the bookmarks view, so the
                // split suggests places to jump to.
                let di = self.dock_pane_of(id).map(|(di, _)| di).unwrap_or(0);
                self.add_pane(di, None, &sender);
            }

            AppMsg::OpenSplitFrom { id, root } => {
                if let Some((di, _pi)) = self.dock_pane_of(id) {
                    self.add_pane(di, Some(root), &sender);
                }
            }

            AppMsg::OpenOppositeFrom { id, root } => {
                if let Some((di, _pi)) = self.dock_pane_of(id) {
                    let side = self.docks[di].side.opposite();
                    let monitor = self.docks[di].monitor.clone();
                    let target = self.ensure_dock(side, monitor, false, &sender);
                    self.add_pane(target, Some(root), &sender);
                    // A freshly created dock starts hidden; reveal it (unless
                    // the panel is currently toggled off on this workspace).
                    let visible = self.visible();
                    self.set_docks_visible(visible);
                }
            }

            AppMsg::ClosePane { id } => {
                let Some((di, pi)) = self.dock_pane_of(id) else {
                    return;
                };
                // The pane's tree is about to be dropped; pause its previews
                // first so a playing video can't be kept alive by a widget the
                // removal leaves parented.
                self.docks[di].panes[pi].tree.emit(TreeMsg::Suspend);
                self.docks[di].panes.remove(pi);
                if self.docks[di].active_pane == Some(id) {
                    self.docks[di].active_pane = self.last_active_pane_id(di);
                }
                // The program only exits once the *last* pane anywhere closes.
                let panes_left: usize = self.docks.iter().map(|d| d.panes.len()).sum();
                if panes_left == 0 {
                    self.window.close();
                    return;
                }
                if self.docks[di].panes.is_empty() && di != 0 {
                    // Nothing left in this non-primary dock on any workspace.
                    let dock = self.docks.remove(di);
                    dock.window.close();
                } else {
                    fill_pane_container(
                        &self.docks[di].container.clone(),
                        &self.docks[di].panes,
                        &self.active_workspace,
                    );
                    // The dock may still hold panes, but none on the active
                    // workspace; it must not linger as a blank strip. Closing a
                    // pane is not a request to hide the panel, so the user's
                    // show/hide intent is left alone.
                    let shown = self.visible() && self.dock_has_active_pane(di);
                    self.docks[di].window.set_visible(shown);
                }
            }

            AppMsg::FilterChanged { id, filter } => {
                if let Some((di, pi)) = self.dock_pane_of(id) {
                    if self.docks[di].panes[pi].on_bookmarks() {
                        self.docks[di].panes[pi].bookmark_filter = filter;
                        self.refresh_all_bookmarks(&sender);
                    } else {
                        self.docks[di].panes[pi].tree.emit(TreeMsg::SetFilter(filter));
                    }
                }
            }
            AppMsg::FilterClosed { id } => {
                if let Some((di, pi)) = self.dock_pane_of(id) {
                    let pane = &mut self.docks[di].panes[pi];
                    pane.filter_entry.set_text("");
                    pane.tree.emit(TreeMsg::SetFilter(String::new()));
                    pane.bookmark_filter.clear();
                    pane.filter_bar.set_visible(false);
                }
                self.refresh_all_bookmarks(&sender);
            }

            AppMsg::FocusPane { id } => {
                self.focus_pane(id);
            }

            AppMsg::FocusVisible => {
                let ids: Vec<u64> = (0..self.docks.len())
                    .filter(|&di| self.docks[di].window.is_visible())
                    .filter_map(|di| self.last_active_pane_id(di))
                    .collect();
                for id in ids {
                    self.focus_pane(id);
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

            AppMsg::ResolveDockMonitor { di } => {
                let Some(dock) = self.docks.get(di) else { return };
                if dock.monitor.is_some() {
                    return;
                }
                let (Some(surface), Some(display)) =
                    (dock.window.surface(), gdk::Display::default())
                else {
                    return;
                };
                if let Some(monitor) = display.monitor_at_surface(&surface) {
                    self.docks[di].monitor = Some(monitor);
                }
            }

            AppMsg::PaneDragBegin { id, start } => self.begin_pane_drag(id, start),
            AppMsg::PaneDragUpdate { id, offset } => self.update_pane_drag(id, offset),
            AppMsg::PaneDragEnd { id, offset } => self.end_pane_drag(id, offset, &sender),

            AppMsg::BookmarkEvent { id, event } => match event {
                BookmarkEvent::Open(path) => self.open_bookmark(id, path, &sender),
                BookmarkEvent::Toggle(index_path) => {
                    if let Some(entry) = Bookmark::get_mut(&mut self.bookmarks, &index_path) {
                        entry.expanded = !entry.expanded;
                    }
                    self.refresh_all_bookmarks(&sender);
                }
                BookmarkEvent::Edit(index_path) => self.edit_bookmark(id, index_path, &sender),
                BookmarkEvent::NewBookmark => self.new_bookmark(id, &sender),
                BookmarkEvent::NewBookmarkFolder => self.new_bookmark_folder(id, &sender),
                BookmarkEvent::Action { path, target } => {
                    self.run_bookmark_action(id, path, target);
                }
                BookmarkEvent::Move { from, to } => self.move_bookmark(from, to, &sender),
                BookmarkEvent::Delete(index_path) => {
                    if Bookmark::remove(&mut self.bookmarks, &index_path) {
                        self.save_bookmarks();
                        self.refresh_all_bookmarks(&sender);
                    }
                }
            },
            AppMsg::BookmarkEditSaved { index_path, name, path } => {
                if let Some(entry) = Bookmark::get_mut(&mut self.bookmarks, &index_path) {
                    entry.name = name;
                    // A folder has no path; a leaf always sets one.
                    if path.is_some() {
                        entry.path = path;
                    }
                    self.save_bookmarks();
                    self.refresh_all_bookmarks(&sender);
                }
            }
            AppMsg::BookmarkAdded { name, path } => {
                if Bookmark::contains_path(&self.bookmarks, &path) {
                    self.status = format!("{} is already bookmarked", path.display());
                    return;
                }
                self.bookmarks.push(Bookmark::leaf(name, path));
                self.save_bookmarks();
                self.refresh_all_bookmarks(&sender);
            }
            AppMsg::BookmarkFolderAdded { name } => {
                self.bookmarks.push(Bookmark {
                    name,
                    path: None,
                    items: Vec::new(),
                    expanded: false,
                });
                self.save_bookmarks();
                self.refresh_all_bookmarks(&sender);
            }
        }
    }
}

impl App {
    /// Apply a forwarded launch request.
    fn handle_launch(&mut self, command: Command, sender: &ComponentSender<Self>) {
        // `--key`: run a configured shortcut against the active pane as if it
        // were pressed. It never changes visibility; a cold start (no panes at
        // all) falls through to the normal show.
        if let Some(accel) = command.key.clone() {
            match self.active_pane_id(command.side) {
                Some(id) => {
                    self.run_accelerator(id, &accel, sender);
                    return;
                }
                None if !self.docks.is_empty() => return,
                None => {}
            }
        }

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
            let monitor = self.default_monitor();
            let di = self.ensure_dock(side, monitor, false, sender);
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
            let monitor = self.default_monitor();
            let di = self.ensure_dock(side, monitor, false, sender);
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
        // Sides may host more than one dock (one per monitor); the visibility
        // rules only care which sides exist and which are shown.
        let mut existing: Vec<PanelSide> = Vec::new();
        let mut shown: Vec<PanelSide> = Vec::new();
        for dock in &self.docks {
            if !existing.contains(&dock.side) {
                existing.push(dock.side);
            }
            if dock.window.is_visible() && !shown.contains(&dock.side) {
                shown.push(dock.side);
            }
        }
        let plan = resolve_visibility(intent, &existing, &shown);

        // A whole-panel show just reveals what is already open: an emptied dock
        // (e.g. the default side after its pane was moved to the other side)
        // must not be re-seeded with a fresh pane when a pane is still open
        // elsewhere. Only a side-specific show/creation seeds an empty dock.
        let whole_panel =
            matches!(intent, VisibilityIntent::ShowAll | VisibilityIntent::ToggleAll);
        // Only panes on the active workspace count as "open" for seeding: a
        // whole-panel show should reveal them rather than spawn a new pane.
        let panes_exist = self
            .docks
            .iter()
            .flat_map(|dock| &dock.panes)
            .any(|pane| pane.workspace == self.active_workspace);
        let seed_empty = should_seed_empty(panes_exist, plan.seed, whole_panel);

        if let Some(side) = plan.create {
            let monitor = self.default_monitor();
            self.ensure_dock(side, monitor, plan.seed, sender);
        }
        for side in &plan.show {
            // Show every dock on this side (there may be more than one, one per
            // monitor), creating one on the default monitor if none exists.
            if self.dock_of_side(*side).is_none() {
                let monitor = self.default_monitor();
                self.ensure_dock(*side, monitor, seed_empty || !whole_panel, sender);
            }
            let indices: Vec<usize> = self
                .docks
                .iter()
                .enumerate()
                .filter(|(_, dock)| dock.side == *side)
                .map(|(di, _)| di)
                .collect();
            for di in indices {
                if seed_empty && !self.dock_has_active_pane(di) {
                    let startup = self.config.startup.clone();
                    match default_root(&startup) {
                        Some(root) => self.add_pane(di, Some(root), sender),
                        None => self.add_pane(di, None, sender),
                    };
                }
                // Never reveal a dock with no active-workspace pane.
                let show = self.dock_has_active_pane(di);
                self.docks[di].window.set_visible(show);
            }
        }
        for side in &plan.hide {
            let indices: Vec<usize> = self
                .docks
                .iter()
                .enumerate()
                .filter(|(_, dock)| dock.side == *side)
                .map(|(di, _)| di)
                .collect();
            for di in indices {
                self.docks[di].window.set_visible(false);
                self.suspend_dock_panes(di);
            }
        }
        // Any side not named by the plan keeps its current visibility.
        self.refresh_visible();
        // When a side was just shown, hand keyboard focus to its top pane so
        // the panel is immediately usable.
        let focus: Vec<u64> = plan
            .show
            .iter()
            .flat_map(|side| {
                self.docks
                    .iter()
                    .filter(move |dock| dock.side == *side)
                    .filter_map(|dock| dock.panes.last())
                    .map(|pane| pane.id)
            })
            .collect();
        for id in focus {
            self.focus_pane(id);
        }
    }

    /// Set `id` as the active pane in whichever dock holds it.
    fn set_active(&mut self, id: u64) {
        if let Some((di, _pi)) = self.dock_pane_of(id) {
            self.docks[di].active_pane = Some(id);
        }
    }

    /// Hand keyboard focus to pane `id`: the bookmark cursor when the bookmarks
    /// view is showing, otherwise the tree's current row. Used on launch, on
    /// show, and when a pane becomes active so the keyboard works without a
    /// click.
    fn focus_pane(&mut self, id: u64) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        let pane = &self.docks[di].panes[pi];
        if pane.on_bookmarks() {
            pane.bookmark_nav.focus_start();
        } else {
            pane.tree.emit(TreeMsg::Focus);
        }
    }

    /// Focus pane `id` on the next idle. Used after a body switch: the new page
    /// (and its freshly-loaded tree rows) is not laid out until then, so an
    /// immediate `grab_focus` would land on the now-hidden page.
    fn focus_pane_later(&self, id: u64) {
        let sender = self.sender.clone();
        glib::idle_add_local_once(move || sender.input(AppMsg::FocusPane { id }));
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

    /// Append `path` to the bookmarks (skipping a duplicate path, at any depth)
    /// and persist.
    fn add_bookmark(&mut self, path: PathBuf) {
        if Bookmark::contains_path(&self.bookmarks, &path) {
            self.status = format!("{} is already bookmarked", path.display());
            return;
        }
        self.bookmarks.push(Bookmark::leaf(Bookmark::default_name(&path), path.clone()));
        self.save_bookmarks();
        self.status = format!("Bookmarked {}", path.display());
    }

    /// Show `path` in pane `id`: switch its body to the tree and load the
    /// directory (leaving the bookmarks view, if it was showing).
    fn open_root_in_pane(&mut self, id: u64, path: PathBuf) {
        if let Some((di, pi)) = self.dock_pane_of(id) {
            self.docks[di].panes[pi].show_tree();
            self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(path));
            self.set_active(id);
        }
        self.focus_pane_later(id);
    }

    /// Jump pane `id` to a bookmark's directory, replacing its bookmarks view
    /// (or its current directory) with that folder.
    fn open_bookmark(&mut self, id: u64, path: PathBuf, _sender: &ComponentSender<Self>) {
        let path = crate::config::expand_bookmark_path(&path);
        if !path.is_dir() {
            self.status = format!("{} is not a directory", path.display());
            return;
        }
        self.open_root_in_pane(id, path);
    }

    /// Run a bookmark's inherited context action against its directory without
    /// opening it. `Open` navigates (a directory) or launches (a file); other
    /// path-safe builtins are sent to the pane's tree, which acts on the
    /// explicit path; custom commands run against the path.
    fn run_bookmark_action(&mut self, id: u64, path: PathBuf, target: ShortcutTarget) {
        let path = crate::config::expand_bookmark_path(&path);
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        match target {
            ShortcutTarget::Builtin(BuiltinAction::Open) => {
                if path.is_dir() {
                    self.open_root_in_pane(id, path);
                } else {
                    self.docks[di].panes[pi].tree.emit(TreeMsg::OpenWithDefault(path));
                }
            }
            ShortcutTarget::Builtin(action) => {
                if let Some(msg) = crate::ui::tree::path_action_message(action, &path) {
                    self.docks[di].panes[pi].tree.emit(msg);
                }
            }
            ShortcutTarget::Command(cmd) => {
                self.docks[di].panes[pi]
                    .tree
                    .emit(TreeMsg::RunCommand { command: cmd.command, path });
            }
        }
    }

    /// Switch pane `id` to the bookmarks view (the hamburger "Bookmarks" item).
    fn show_bookmarks_view(&mut self, id: u64) {
        if let Some((di, pi)) = self.dock_pane_of(id) {
            self.docks[di].panes[pi].show_bookmarks();
            self.docks[di].panes[pi].history.record(ViewEntry::Bookmarks);
            self.docks[di].panes[pi].refresh_nav();
            self.set_active(id);
        }
        self.focus_pane_later(id);
    }

    /// Open the bookmark editor for the entry at `index_path`, parented to the
    /// window of pane `id`.
    fn edit_bookmark(&mut self, id: u64, index_path: Vec<usize>, sender: &ComponentSender<Self>) {
        let Some(bookmark) = Bookmark::get(&self.bookmarks, &index_path).cloned() else {
            return;
        };
        let Some((di, _pi)) = self.dock_pane_of(id) else { return };
        let parent = self.docks[di].window.clone();
        let sender = sender.clone();
        let path = bookmark.path.as_deref().map(crate::config::expand_bookmark_path);
        let title = if bookmark.is_folder() { "Edit Folder" } else { "Edit Bookmark" };
        bookmarks::show_bookmark_dialog(
            &parent,
            title,
            &bookmark.name,
            path.as_deref(),
            move |name, path| {
                sender.input(AppMsg::BookmarkEditSaved { index_path: index_path.clone(), name, path });
            },
        );
    }

    /// Open the "new bookmark" dialog for pane `id`: a name and a path (with a
    /// folder picker), matching the edit dialog.
    fn new_bookmark(&mut self, id: u64, sender: &ComponentSender<Self>) {
        let Some((di, _pi)) = self.dock_pane_of(id) else { return };
        let parent = self.docks[di].window.clone();
        let sender = sender.clone();
        bookmarks::show_bookmark_dialog(
            &parent,
            "New Bookmark",
            "",
            Some(Path::new("")),
            move |name, path| {
                if let Some(path) = path {
                    sender.input(AppMsg::BookmarkAdded { name, path });
                }
            },
        );
    }

    /// Open the "new folder" dialog for pane `id`: a name only.
    fn new_bookmark_folder(&mut self, id: u64, sender: &ComponentSender<Self>) {
        let Some((di, _pi)) = self.dock_pane_of(id) else { return };
        let parent = self.docks[di].window.clone();
        let sender = sender.clone();
        bookmarks::show_bookmark_dialog(&parent, "New Folder", "", None, move |name, _path| {
            sender.input(AppMsg::BookmarkFolderAdded { name });
        });
    }

    /// Move the bookmark entry at `from` to `to` (drag and drop), then persist.
    /// Moving an entry into itself or one of its descendants is refused.
    fn move_bookmark(&mut self, from: Vec<usize>, to: MoveTarget, sender: &ComponentSender<Self>) {
        if from.is_empty() {
            return;
        }
        // Resolve the destination against the tree *after* the entry is removed,
        // so index shifts from the removal are accounted for.
        let (parent, index) = match &to {
            MoveTarget::Root => (Vec::new(), usize::MAX),
            MoveTarget::Into(folder) => {
                if folder.starts_with(&from) {
                    return;
                }
                (adjust_path_after_removal(&from, folder), usize::MAX)
            }
            MoveTarget::Before(leaf) => {
                if leaf.is_empty() || leaf.starts_with(&from) {
                    return;
                }
                let parent_orig = leaf[..leaf.len() - 1].to_vec();
                let parent = adjust_path_after_removal(&from, &parent_orig);
                let mut index = leaf[leaf.len() - 1];
                // A removal earlier in the same sibling list shifts the target.
                if from.len() == leaf.len()
                    && from[..from.len() - 1] == parent_orig[..]
                    && from[from.len() - 1] < index
                {
                    index -= 1;
                }
                (parent, index)
            }
        };
        let Some(entry) = Bookmark::take(&mut self.bookmarks, &from) else {
            return;
        };
        if !Bookmark::insert(&mut self.bookmarks, &parent, index, entry) {
            return;
        }
        self.save_bookmarks();
        self.refresh_all_bookmarks(sender);
    }

    /// Save the bookmarks list to its file, reporting any failure.
    fn save_bookmarks(&mut self) {
        let path = bookmark_file_path(&self.config.bookmarks.file);
        if let Err(err) = save_bookmarks_to_path(&path, &self.bookmarks) {
            self.status = format!("Could not save bookmarks: {err:?}");
        }
    }

    /// Refill every pane's bookmarks list after the list changes. Panes showing
    /// the view update in place; hidden ones are ready when next shown. Each
    /// pane's own filter and menu config are applied.
    fn refresh_all_bookmarks(&mut self, sender: &ComponentSender<Self>) {
        let context = self.config.context_menu.clone();
        let extras = self.config.bookmarks.context.clone();
        let bookmarks = self.bookmarks.clone();
        for di in 0..self.docks.len() {
            for pi in 0..self.docks[di].panes.len() {
                let pane = &self.docks[di].panes[pi];
                let menu = bookmarks::BookmarkMenuConfig {
                    context: &context,
                    extras: &extras,
                    side: self.docks[di].side,
                };
                let id = pane.id;
                let filter = pane.bookmark_filter.clone();
                let sender = sender.clone();
                let on_event: Rc<dyn Fn(BookmarkEvent)> =
                    Rc::new(move |event| sender.input(AppMsg::BookmarkEvent { id, event }));
                bookmarks::fill_bookmarks(
                    &pane.bookmarks_list,
                    &bookmarks,
                    &filter,
                    &menu,
                    &pane.bookmark_drag,
                    &pane.bookmark_nav,
                    on_event,
                );
            }
        }
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
            BuiltinAction::ToggleBookmarks => self.show_bookmarks_view(id),
            BuiltinAction::NewBookmark => self.new_bookmark(id, sender),
            BuiltinAction::NewBookmarkFolder => self.new_bookmark_folder(id, sender),
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
        self.focus_pane_later(id);
    }

    /// Step pane `id` back one entry in its history and show it.
    fn go_back(&mut self, id: u64) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        if let Some(target) = self.docks[di].panes[pi].history.back() {
            self.show_view_entry(id, target);
        }
    }

    /// Step pane `id` forward one entry in its history and show it.
    fn go_forward(&mut self, id: u64) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        if let Some(target) = self.docks[di].panes[pi].history.forward() {
            self.show_view_entry(id, target);
        }
    }

    /// Show a history entry in pane `id`: a directory (load it in the tree) or
    /// the bookmarks view. A back/forward to the bookmarks view produces no
    /// `RootChanged`, so its navigation flag is cleared here.
    fn show_view_entry(&mut self, id: u64, entry: ViewEntry) {
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        match entry {
            ViewEntry::Dir(path) => {
                self.docks[di].panes[pi].show_tree();
                self.docks[di].panes[pi].tree.emit(TreeMsg::OpenRoot(path));
            }
            ViewEntry::Bookmarks => {
                self.docks[di].panes[pi].show_bookmarks();
                self.docks[di].panes[pi].history.finish_navigation();
                self.docks[di].panes[pi].refresh_nav();
            }
        }
        self.set_active(id);
        self.focus_pane_later(id);
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
        // "Move to Workspace" carries a target that `ShortcutTarget` cannot
        // represent, so it is handled here from the full menu item.
        if let ContextAction::Entry(entry) = &action
            && entry.action == BuiltinAction::MoveToWorkspace
            && entry.workspace.is_none()
        {
            self.status = "Move to Workspace needs a workspace = \"...\" target".to_owned();
            return;
        }
        if let Some(target) = action.workspace_move() {
            self.move_active_pane_to_workspace(target);
            return;
        }
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

    /// The pane an action should target: the active pane of the asked-for side
    /// (or the primary side / first visible dock), falling back to its last
    /// pane.
    fn active_pane_id(&self, side: Option<PanelSide>) -> Option<u64> {
        let di = match side {
            Some(side) => self.dock_of_side(side)?,
            None => self
                .dock_of_side(self.primary_side)
                .or_else(|| self.docks.iter().position(|dock| dock.window.is_visible()))
                .or_else(|| (!self.docks.is_empty()).then_some(0))?,
        };
        let dock = &self.docks[di];
        let active = &self.active_workspace;
        dock.active_pane
            .filter(|id| {
                dock.panes.iter().any(|pane| pane.id == *id && &pane.workspace == active)
            })
            .or_else(|| {
                dock.panes
                    .iter()
                    .rev()
                    .find(|pane| &pane.workspace == active)
                    .map(|pane| pane.id)
            })
    }

    /// Run an accelerator string (e.g. `Ctrl+c`, `Down`) against pane `id`, as
    /// if the key were pressed: pane-menu shortcuts first (they are resolved at
    /// the pane level in a real press), then the bookmarks or tree view's
    /// structural/context-menu shortcuts. Used by the IPC `--key` command.
    fn run_accelerator(&mut self, id: u64, accel: &str, sender: &ComponentSender<Self>) {
        let Some((key, mods)) = parse_accelerator(accel) else {
            return;
        };
        let Some((di, pi)) = self.dock_pane_of(id) else {
            return;
        };
        if let Some(action) = self.docks[di].panes[pi].shortcuts.action_for(key, mods) {
            self.run_pane_shortcut(id, action, sender);
            return;
        }
        // Navigation and activation belong to the bookmarks view, not the
        // (hidden) tree, which would have no rows to move over or open.
        if self.docks[di].panes[pi].on_bookmarks() {
            if bookmarks::BookmarkNav::is_nav_key(key) {
                if let Some(node) = self.docks[di].panes[pi].bookmark_nav.navigate(key) {
                    sender.input(AppMsg::BookmarkEvent {
                        id,
                        event: BookmarkEvent::Toggle(node.index_path),
                    });
                }
                return;
            }
            if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space) {
                if let Some(event) = self.docks[di].panes[pi].bookmark_nav.activate() {
                    sender.input(AppMsg::BookmarkEvent { id, event });
                }
                return;
            }
        }
        self.docks[di].panes[pi]
            .tree
            .emit(TreeMsg::RunAccelerator(accel.to_owned()));
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

    /// Index of the first dock on `side`, if one exists.
    fn dock_of_side(&self, side: PanelSide) -> Option<usize> {
        self.docks.iter().position(|dock| dock.side == side)
    }

    /// Index of the dock on `(side, monitor)`. A dock whose monitor has not
    /// been resolved yet matches only an unresolved (`None`) query.
    fn dock_index_of(&self, side: PanelSide, monitor: Option<&gdk::Monitor>) -> Option<usize> {
        self.docks.iter().position(|dock| {
            dock.side == side
                && match (&dock.monitor, monitor) {
                    (Some(a), Some(b)) => monitor_key(a) == monitor_key(b),
                    (None, None) => true,
                    _ => false,
                }
        })
    }

    /// The monitor a newly-created (CLI/reveal) dock should use: the primary
    /// dock's, falling back to the first monitor the display reports.
    fn default_monitor(&self) -> Option<gdk::Monitor> {
        self.docks
            .first()
            .and_then(|dock| dock.monitor.clone())
            .or_else(|| {
                gdk::Display::default()?
                    .monitors()
                    .item(0)
                    .and_downcast::<gdk::Monitor>()
            })
    }

    /// Hide the entire dock that holds pane `id` (the toolbar "Collapse"
    /// action). The dock keeps its panes, so it comes back on the next show.
    fn collapse_dock(&mut self, id: u64) {
        if let Some((di, _)) = self.dock_pane_of(id) {
            self.set_dock_visible(di, false);
        }
    }

    /// Show/hide one dock. A dock only maps when it holds a pane on the active
    /// workspace.
    fn set_dock_visible(&mut self, di: usize, visible: bool) {
        let shown = visible && self.dock_has_active_pane(di);
        self.docks[di].window.set_visible(shown);
        if !shown {
            self.suspend_dock_panes(di);
        }
        self.refresh_visible();
    }

    /// Pause media in every pane of dock `di`. Hiding a dock does not destroy
    /// its trees, so without this a preview would keep playing while invisible.
    fn suspend_dock_panes(&self, di: usize) {
        for pane in &self.docks[di].panes {
            pane.tree.emit(TreeMsg::Suspend);
        }
    }

    /// Whether dock `di` holds at least one pane on the active workspace.
    fn dock_has_active_pane(&self, di: usize) -> bool {
        let active = &self.active_workspace;
        self.docks[di].panes.iter().any(|pane| &pane.workspace == active)
    }

    /// The most recently added pane on the active workspace in dock `di`, if any.
    fn last_active_pane_id(&self, di: usize) -> Option<u64> {
        let active = &self.active_workspace;
        self.docks[di]
            .panes
            .iter()
            .rev()
            .find(|pane| &pane.workspace == active)
            .map(|pane| pane.id)
    }

    /// Handle a workspace change: re-render every dock for the new workspace and
    /// update which docks are mapped.
    fn on_active_workspace(&mut self, name: String) {
        if self.active_workspace == name {
            return;
        }
        self.active_workspace = name;
        self.refill_docks();
        self.sync_dock_visibility();
        let active = (0..self.docks.len())
            .find(|&di| self.docks[di].window.is_visible())
            .and_then(|di| self.last_active_pane_id(di));
        if let Some(id) = active {
            self.focus_pane(id);
        }
    }

    /// Rebuild every dock's pane stack for the active workspace.
    fn refill_docks(&mut self) {
        for dock in &self.docks {
            fill_pane_container(&dock.container, &dock.panes, &self.active_workspace);
        }
    }

    /// Whether the panel is intended to be shown on the active workspace:
    /// shown unless the user explicitly hid it there. Each workspace keeps its
    /// own intent, so hiding on one never affects the others.
    fn visible(&self) -> bool {
        !self.hidden_workspaces.contains(&self.active_workspace)
    }

    /// Record the show/hide intent for the active workspace.
    fn set_visible(&mut self, visible: bool) {
        if visible {
            self.hidden_workspaces.remove(&self.active_workspace);
        } else {
            self.hidden_workspaces.insert(self.active_workspace.clone());
        }
    }

    /// Apply the active workspace's show/hide intent to every dock, showing only
    /// docks that hold a pane on the active workspace.
    fn sync_dock_visibility(&mut self) {
        let show = self.visible();
        for di in 0..self.docks.len() {
            let shown = show && self.dock_has_active_pane(di);
            self.docks[di].window.set_visible(shown);
            if !shown {
                self.suspend_dock_panes(di);
            }
        }
    }

    /// Show/hide every dock window. Docks with no pane on the active workspace
    /// stay hidden.
    fn set_docks_visible(&mut self, visible: bool) {
        self.set_visible(visible);
        self.sync_dock_visibility();
        if visible {
            // Hand keyboard focus to each shown dock's active pane. The
            // window-map handler also does this once the surface is realized, so
            // a just-shown panel gets focus even though the grab here may
            // precede allocation.
            let focus: Vec<u64> = (0..self.docks.len())
                .filter(|&di| self.docks[di].window.is_visible())
                .filter_map(|di| self.last_active_pane_id(di))
                .collect();
            for id in focus {
                self.focus_pane(id);
            }
        }
    }

    /// Move the active pane to another workspace (a configured action). It
    /// disappears from the current workspace and appears on the target. The
    /// panel itself never moves.
    fn move_active_pane_to_workspace(&mut self, target: WorkspaceMove) {
        if self.hypr.is_none() {
            self.status = "Workspace moves need a Hyprland session".to_owned();
            return;
        }
        let Some(id) = self.active_pane_id(None) else { return };
        let Some((di, pi)) = self.dock_pane_of(id) else { return };
        let from = self.docks[di].panes[pi].workspace.clone();
        let name = match target {
            WorkspaceMove::Named(name) => name,
            WorkspaceMove::Previous => self.neighbor_workspace(&from, -1),
            WorkspaceMove::Next => self.neighbor_workspace(&from, 1),
        };
        if name.is_empty() || name == from {
            return;
        }
        self.docks[di].panes[pi].workspace = name.clone();
        let container = self.docks[di].container.clone();
        fill_pane_container(&container, &self.docks[di].panes, &self.active_workspace);
        self.sync_dock_visibility();
        self.status = format!("Pane moved to workspace {name}");
    }

    /// The workspace `delta` steps from `from`. A numeric workspace steps by
    /// number (`1`→`2`), creating it implicitly when the pane lands there; a
    /// named/special workspace steps through the workspaces Hyprland currently
    /// reports, wrapping around.
    fn neighbor_workspace(&self, from: &str, delta: i32) -> String {
        if let Ok(id) = from.parse::<i64>() {
            return (id + delta as i64).max(1).to_string();
        }
        let Some(hypr) = &self.hypr else { return String::new() };
        let names = hypr.workspace_names();
        if names.is_empty() {
            return from.to_owned();
        }
        match names.iter().position(|name| name == from) {
            Some(i) => {
                let j = (i as i32 + delta).rem_euclid(names.len() as i32) as usize;
                names[j].clone()
            }
            None => names[0].clone(),
        }
    }

    /// Reconcile the active workspace's show/hide intent with what is actually
    /// mapped: shown while at least one dock is visible. Drives the no-argument
    /// toggle. Only the active workspace's intent is touched.
    fn refresh_visible(&mut self) {
        let any = self.docks.iter().any(|dock| dock.window.is_visible());
        self.set_visible(any);
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

    /// Return the index of the dock on `(side, monitor)`, creating it (with a
    /// seeded pane from `[startup]`) when missing. `monitor` is the monitor the
    /// new dock should dock to; `None` lets the compositor pick.
    fn ensure_dock(
        &mut self,
        side: PanelSide,
        monitor: Option<gdk::Monitor>,
        seed: bool,
        sender: &ComponentSender<Self>,
    ) -> usize {
        if let Some(di) = self.dock_index_of(side, monitor.as_ref()) {
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
        init_layer_window(&window, config, side, width, monitor.as_ref());
        window.set_default_size(width as i32, 520);
        if layer_shell_available() {
            attach_resize_controls(&window, side, sender);
        }
        // The window's child is the box holding the pane stack.
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        outer.set_vexpand(true);
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_vexpand(true);
        outer.append(&container);
        window.set_child(Some(&outer));

        let di = self.docks.len();
        self.docks.push(Dock {
            side,
            monitor,
            window,
            container,
            panes: Vec::new(),
            active_pane: None,
        });
        // As with the primary dock, resolve the monitor and hand focus to this
        // dock's pane once its surface is mapped (grab_focus before that no-ops).
        {
            let sender = sender.clone();
            let window = self.docks[di].window.clone();
            window.connect_map(move |_| {
                let sender = sender.clone();
                sender.input(AppMsg::ResolveDockMonitor { di });
                glib::idle_add_local_once(move || sender.input(AppMsg::FocusVisible));
            });
        }
        if seed {
            let startup = self.config.startup.clone();
            if let Some(root) = default_root(&startup) {
                self.add_pane(di, Some(root), sender);
            } else {
                // No directory: seed the dock with a bookmarks pane.
                self.add_pane(di, None, sender);
            }
        }
        di
    }

    /// Append a pane showing `root` to dock `di`, returning its id. A `None`
    /// root opens the bookmarks view instead of a directory.
    fn add_pane(&mut self, di: usize, root: Option<PathBuf>, sender: &ComponentSender<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let parent = self.docks[di].window.clone();
        let side = self.docks[di].side;
        let width = self.width_for(side);
        let workspace = self.active_workspace.clone();
        let pane = make_pane(&self.config, parent, id, side, width, workspace, sender.clone());
        let mut pane = pane;
        match &root {
            Some(root) => pane.tree.emit(TreeMsg::OpenRoot(root.clone())),
            None => {
                pane.show_bookmarks();
                pane.history.record(ViewEntry::Bookmarks);
                pane.refresh_nav();
            }
        }
        if let Some(root) = &root {
            pane.set_root(root.clone());
        }
        let dock = &mut self.docks[di];
        dock.panes.push(pane);
        dock.active_pane = Some(id);
        let container = dock.container.clone();
        fill_pane_container(&container, &dock.panes, &self.active_workspace);
        // Give the new pane keyboard focus once GTK has allocated it, so the
        // app (and each new split) is usable without a click.
        let focus_sender = sender.clone();
        glib::idle_add_local_once(move || focus_sender.input(AppMsg::FocusPane { id }));
        // Populate the new pane's bookmarks list.
        self.refresh_all_bookmarks(sender);
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

    /// The absolute top-left of dock `di`'s surface, derived from its monitor
    /// geometry, side, width and the configured margin. Turns window-relative
    /// drag coordinates into screen coordinates for target detection.
    fn dock_origin(&self, di: usize) -> (f64, f64) {
        let dock = &self.docks[di];
        let margin = self.config.panel.margin as f64;
        let width = self.width_for(dock.side) as f64;
        let (mx, my, mw) = match dock.monitor.as_ref().map(gdk::Monitor::geometry) {
            Some(g) => (g.x() as f64, g.y() as f64, g.width() as f64),
            None => (0.0, 0.0, 0.0),
        };
        let x = match dock.side {
            PanelSide::Left => mx + margin,
            PanelSide::Right => mx + mw - width - margin,
        };
        (x, my + margin)
    }

    /// The `(side, monitor)` under an absolute screen point, or `None` when the
    /// point is outside every monitor. The side is the nearer edge of the
    /// monitor under the pointer.
    fn point_target(&self, point: (f64, f64)) -> Option<(PanelSide, gdk::Monitor)> {
        let (x, y) = point;
        let monitors = gdk::Display::default()?.monitors();
        for i in 0..monitors.n_items() {
            let Some(monitor) = monitors.item(i).and_downcast::<gdk::Monitor>() else {
                continue;
            };
            let g = monitor.geometry();
            let (x0, y0) = (g.x() as f64, g.y() as f64);
            let (w, h) = (g.width() as f64, g.height() as f64);
            if x >= x0 && x < x0 + w && y >= y0 && y < y0 + h {
                let side = if x < x0 + w / 2.0 { PanelSide::Left } else { PanelSide::Right };
                return Some((side, monitor));
            }
        }
        None
    }

    /// Begin a pane-move drag: remember the pane and the pointer's absolute
    /// start position, and cue the source pane.
    fn begin_pane_drag(&mut self, id: u64, start: (f64, f64)) {
        let Some((di, pi)) = self.dock_pane_of(id) else { return };
        // Compute the grip's top-left within the pane's current window. Doing
        // this here (rather than in the gesture closure) keeps it correct after
        // the pane has been moved to a different dock/window.
        let grip_origin = {
            let grip = &self.docks[di].panes[pi].grip;
            let window = &self.docks[di].window;
            grip.compute_point(window, &relm4::gtk::graphene::Point::new(0.0, 0.0))
                .map(|p| (p.x() as f64, p.y() as f64))
                .unwrap_or((0.0, 0.0))
        };
        let origin = self.dock_origin(di);
        let base = (origin.0 + grip_origin.0 + start.0, origin.1 + grip_origin.1 + start.1);
        self.docks[di].panes[pi].widget.add_css_class("pane-moving");
        self.pane_drag = Some(PaneDrag { id, base, highlighted: None });
        self.status = "Drag the pane to the edge of a screen to move it".to_owned();
    }

    /// Update the drop target for a drag in progress: highlight the target dock
    /// and describe it in the status line.
    fn update_pane_drag(&mut self, id: u64, offset: (f64, f64)) {
        let Some(drag) = self.pane_drag.as_ref() else { return };
        if drag.id != id {
            return;
        }
        let point = (drag.base.0 + offset.0, drag.base.1 + offset.1);
        let target = self.point_target(point);
        let highlight = target
            .as_ref()
            .and_then(|(side, monitor)| self.dock_index_of(*side, Some(monitor)));
        if let Some(drag) = self.pane_drag.as_mut()
            && drag.highlighted != highlight
        {
            if let Some(di) = drag.highlighted
                && let Some(dock) = self.docks.get(di)
            {
                dock.window.remove_css_class("pane-drop-target");
            }
            if let Some(di) = highlight
                && let Some(dock) = self.docks.get(di)
            {
                dock.window.add_css_class("pane-drop-target");
            }
            drag.highlighted = highlight;
        }
        self.status = match target {
            Some((side, monitor)) => format!(
                "Drop to move the pane to the {} panel on {}",
                side.name(),
                monitor_label(&monitor)
            ),
            None => "Drag the pane to the edge of a screen to move it".to_owned(),
        };
    }

    /// Finish a pane drag: move the pane to the target under the pointer, or
    /// cancel when it was dropped outside every monitor.
    fn end_pane_drag(&mut self, id: u64, offset: (f64, f64), sender: &ComponentSender<Self>) {
        let Some(drag) = self.pane_drag.take() else { return };
        if drag.id != id {
            self.pane_drag = Some(drag);
            return;
        }
        if let Some((di, pi)) = self.dock_pane_of(id) {
            self.docks[di].panes[pi].widget.remove_css_class("pane-moving");
        }
        if let Some(di) = drag.highlighted
            && let Some(dock) = self.docks.get(di)
        {
            dock.window.remove_css_class("pane-drop-target");
        }
        let point = (drag.base.0 + offset.0, drag.base.1 + offset.1);
        match self.point_target(point) {
            Some((side, monitor)) => self.move_pane_to(id, side, monitor, sender),
            None => self.status.clear(),
        }
    }

    /// Move pane `id` into the dock on `(side, monitor)`, creating that dock if
    /// it does not exist yet. A no-op when the pane is already there.
    fn move_pane_to(
        &mut self,
        id: u64,
        side: PanelSide,
        monitor: gdk::Monitor,
        sender: &ComponentSender<Self>,
    ) {
        let Some((sdi, spi)) = self.dock_pane_of(id) else { return };
        if self.dock_index_of(side, Some(&monitor)) == Some(sdi) {
            self.status.clear();
            return;
        }
        let pane = self.docks[sdi].panes.remove(spi);
        remove_from_parent(pane.widget.upcast_ref());
        if self.docks[sdi].active_pane == Some(id) {
            self.docks[sdi].active_pane = self.docks[sdi].panes.last().map(|p| p.id);
        }
        if self.docks[sdi].panes.is_empty() {
            if sdi == 0 {
                // The primary dock is the relm4 root window; hide it rather
                // than destroy the app (see `ClosePane`).
                self.docks[sdi].window.set_visible(false);
            } else {
                let dock = self.docks.remove(sdi);
                dock.window.close();
            }
        } else {
            let container = self.docks[sdi].container.clone();
            fill_pane_container(&container, &self.docks[sdi].panes, &self.active_workspace);
        }
        let tdi = self.ensure_dock(side, Some(monitor), false, sender);
        pane.tree.emit(TreeMsg::SetSide(side));
        self.docks[tdi].panes.push(pane);
        self.docks[tdi].active_pane = Some(id);
        let container = self.docks[tdi].container.clone();
        fill_pane_container(&container, &self.docks[tdi].panes, &self.active_workspace);
        self.sync_dock_visibility();
        self.refresh_all_bookmarks(sender);
        self.focus_pane_later(id);
        self.status = format!("Moved pane to the {} panel", side.name());
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

/// Build the optional navigation toolbar: Up One Level, Back, Forward. Each
/// button dispatches its pane builtin through the same path as the hamburger,
/// so no new message type is needed.
fn build_nav_bar(id: u64, sender: &ComponentSender<App>) -> (gtk::Box, NavButtons) {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    bar.add_css_class("nav-toolbar");

    let up = nav_button("pan-up-symbolic", "Up One Level", BuiltinAction::Up, id, sender);
    let back = nav_button("pan-start-symbolic", "Back", BuiltinAction::Back, id, sender);
    let forward = nav_button("pan-end-symbolic", "Forward", BuiltinAction::Forward, id, sender);

    bar.append(&up);
    bar.append(&back);
    bar.append(&forward);
    (bar, NavButtons { up, back, forward })
}

/// One flat icon button in the navigation toolbar. Not focusable, so clicking it
/// never pulls the keyboard out of the tree or bookmarks list.
fn nav_button(
    icon: &str,
    tooltip: &str,
    action: BuiltinAction,
    id: u64,
    sender: &ComponentSender<App>,
) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("nav-button");
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.set_focusable(false);
    button.set_can_focus(false);
    button.set_valign(gtk::Align::Center);
    let sender = sender.clone();
    button.connect_clicked(move |_| {
        sender.input(AppMsg::PaneToolbar {
            id,
            out: ToolbarOutput::PaneItem(ContextAction::Builtin(action)),
        });
    });
    button
}

fn make_pane(
    config: &Config,
    parent: gtk::Window,
    id: u64,
    side: PanelSide,
    width: u32,
    workspace: String,
    sender: ComponentSender<App>,
) -> Pane {
    // The pane overlay: the toolbar's completion dropdown is added to it so it
    // can draw over the tree while the entry keeps keyboard focus.
    let overlay = gtk::Overlay::new();
    let toolbar = Toolbar::builder()
        .launch(ToolbarInit {
            overlay: overlay.clone(),
            pane_menu: config.pane_menu.clone(),
            bookmarks_menu: config.bookmarks.menu.clone(),
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

    // The toolbar grip starts a pane-move drag. The gesture reports the press
    // point and offsets relative to it; the app adds them to the pane's
    // absolute origin (computed against the pane's *current* window, which
    // changes when the pane is moved) to find the target edge/monitor.
    let grip = toolbar.model().grip().clone();
    {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let s = sender.clone();
        drag.connect_drag_begin(move |_, start_x, start_y| {
            s.input(AppMsg::PaneDragBegin { id, start: (start_x, start_y) });
        });
        let s = sender.clone();
        drag.connect_drag_update(move |_, offset_x, offset_y| {
            s.input(AppMsg::PaneDragUpdate { id, offset: (offset_x, offset_y) });
        });
        let s = sender.clone();
        drag.connect_drag_end(move |_, offset_x, offset_y| {
            s.input(AppMsg::PaneDragEnd { id, offset: (offset_x, offset_y) });
        });
        grip.add_controller(drag);
    }

    let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
    widget.append(toolbar.widget());

    // ── optional navigation toolbar (up one level / back / forward) ─────────
    let nav_buttons = if config.panel.nav_toolbar {
        let (bar, buttons) = build_nav_bar(id, &sender);
        widget.append(&bar);
        Some(buttons)
    } else {
        None
    };

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

    // ── pane body: tree or bookmarks, switched by `body` ────────────────────
    let tree_widget = tree.widget();
    tree_widget.set_vexpand(true);

    let bookmarks_list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bookmarks_list.add_css_class("bookmarks-list");
    let bookmarks_scroll = gtk::ScrolledWindow::new();
    bookmarks_scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    bookmarks_scroll.set_vexpand(true);
    bookmarks_scroll.add_css_class("bookmarks-view");
    bookmarks_scroll.set_child(Some(&bookmarks_list));
    let bookmark_drag = bookmarks::BookmarkDrag::new();
    let bookmark_nav = bookmarks::BookmarkNav::new(&bookmarks_list);
    // The blank-area menu and the drag-and-drop wiring (reorder/move entries,
    // including in and out of folders).
    {
        let items = config.bookmarks.blank.clone();
        let sender = sender.clone();
        let on_event: Rc<dyn Fn(BookmarkEvent)> =
            Rc::new(move |event| sender.input(AppMsg::BookmarkEvent { id, event }));
        bookmarks::attach_bookmarks_scroller(&bookmarks_scroll, &items, side, &bookmark_drag, &bookmark_nav, on_event);
    }

    let body = gtk::Stack::new();
    body.set_vexpand(true);
    body.add_named(tree_widget, Some("tree"));
    body.add_named(&bookmarks_scroll, Some("bookmarks"));
    body.set_visible_child_name("tree");
    widget.append(&body);
    widget.set_vexpand(true);

    overlay.set_child(Some(&widget));

    // Pane-menu shortcuts are resolved at the pane level (capture phase), so
    // they work whether focus is on the tree, the path entry, the filter bar, or
    // nothing at all. Row shortcuts stay on the tree. The compiled table is kept
    // on the pane so the IPC `--key` command can reuse it.
    let shortcuts = {
        // Both menus' shortcuts are live regardless of which body is showing,
        // so a binding in either works from the tree or the bookmarks view.
        let mut items = config.pane_menu.items.clone();
        items.extend(config.bookmarks.menu.iter().cloned());
        let shortcuts = Rc::new(PaneShortcuts::compile(&items));
        let s = sender.clone();
        let bound = shortcuts.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            if let Some(action) = bound.action_for(key, state) {
                s.input(AppMsg::PaneShortcut { id, action });
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        widget.add_controller(keys);
        shortcuts
    };

    Pane {
        id,
        toolbar,
        tree,
        workspace,
        root: None,
        canonical_root: None,
        history: NavHistory::default(),
        filter_bar,
        filter_entry,
        body,
        bookmarks_list,
        bookmark_filter: String::new(),
        bookmark_drag,
        bookmark_nav,
        nav_buttons,
        widget: overlay,
        grip,
        shortcuts,
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

/// Rebuild a dock's container Box to show the panes of `active` (the active
/// workspace) in a nested Paned structure. Panes on other workspaces are
/// detached but kept alive.
///
/// Layout for N visible panes:
///   N=1 → container has the single pane widget.
///   N=2 → container has Paned { pane[0], pane[1] }
///   N=3 → container has Paned { pane[0], Paned { pane[1], pane[2] } }
///   etc.
///
/// Called on every split/close/workspace change so widget references are always
/// fresh.
fn fill_pane_container(container: &gtk::Box, panes: &[Pane], active: &str) {
    // Detach every pane box from its old parent *first*, using each parent's
    // removal API. A raw `unparent()` alone is not enough: a Paned keeps stale
    // child slots that re-unparent the widget when the old chain is finally
    // destroyed (possibly after we have re-appended it). Hidden panes are
    // detached too, so they are not left behind in the discarded structure.
    for pane in panes {
        remove_from_parent(pane.widget.upcast_ref());
    }
    // Now drop the old structure wholesale.
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }

    let visible: Vec<&Pane> = panes.iter().filter(|pane| pane.workspace == active).collect();
    match visible.as_slice() {
        [] => {}
        [single] => {
            let w = pane_widget(single);
            w.set_vexpand(true);
            container.append(&w);
        }
        visible => {
            // Build a right-nested Paned from the last two, then keep
            // wrapping from right to left.
            let n = visible.len();
            let last_w = pane_widget(visible[n - 1]);
            last_w.set_vexpand(true);
            let mut right: gtk::Widget = last_w.clone().upcast();
            for pane in visible[..n - 1].iter().rev() {
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
        crate::ui::stop_video_stream(video);
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
fn init_layer_window(
    window: &gtk::Window,
    config: &Config,
    side: PanelSide,
    width: u32,
    monitor: Option<&gdk::Monitor>,
) {
    if !layer_shell_available() {
        configure_plain_window(window, width);
        return;
    }
    if window.is_layer_window() {
        return;
    }
    window.init_layer_shell();
    window.set_namespace(Some(crate::ui::LAYER_NAMESPACE));
    // Pin to the monitor before the surface is mapped; `None` leaves the
    // compositor to pick (the primary dock resolves its monitor after mapping).
    window.set_monitor(monitor);
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

/// A stable key for a monitor, used to compare docks. Prefers the connector
/// name; falls back to model + geometry for backends (often Wayland) that do
/// not expose a connector.
fn monitor_key(monitor: &gdk::Monitor) -> String {
    if let Some(connector) = monitor.connector().filter(|c| !c.is_empty()) {
        return connector.to_string();
    }
    let g = monitor.geometry();
    let model = monitor.model().map(|m| m.to_string()).unwrap_or_default();
    format!("{model}:{}x{}+{}+{}", g.width(), g.height(), g.x(), g.y())
}

/// Whether an existing, emptied dock should be given a fresh pane when showing.
/// A side-specific show does; a whole-panel show does not, as long as a pane is
/// still open somewhere — showing should reveal the existing pane, not spawn a
/// new one.
fn should_seed_empty(panes_exist: bool, seed: bool, whole_panel: bool) -> bool {
    seed && (!whole_panel || !panes_exist)
}

/// A human label for a monitor, for the drag status line.
fn monitor_label(monitor: &gdk::Monitor) -> String {
    monitor
        .connector()
        .filter(|c| !c.is_empty())
        .or_else(|| monitor.model())
        .map(|name| name.to_string())
        .unwrap_or_else(|| "this monitor".to_owned())
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
    let stylesheet = load_stylesheet(&config.theme);
    if let Some(problem) = &stylesheet.problem {
        eprintln!("tree-space: could not load stylesheet: {problem:?}");
    }
    crate::highlight::set_palette(crate::theme::syntax_palette(&config.theme));
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

/// Translate an index `path` into the tree that remains after the entry at
/// `from` is removed. Index paths that diverge from `from` deeper in a different
/// subtree are unaffected; a sibling before the removed entry shifts down one.
fn adjust_path_after_removal(from: &[usize], path: &[usize]) -> Vec<usize> {
    let mut out = path.to_vec();
    for level in 0..path.len().min(from.len()) {
        if from[level] < path[level] {
            out[level] -= 1;
            return out;
        } else if from[level] > path[level] {
            return out;
        }
    }
    out
}

#[cfg(test)]
mod adjust_path_tests {
    use super::adjust_path_after_removal;

    #[test]
    fn adjusts_sibling_indices_after_a_removal() {
        // Removing index 0: later siblings shift down.
        assert_eq!(adjust_path_after_removal(&[0], &[2]), vec![1]);
        // Removing index 2: earlier siblings are unaffected.
        assert_eq!(adjust_path_after_removal(&[2], &[0]), vec![0]);
        // A deeper path in a sibling subtree shifts at the first level only.
        assert_eq!(adjust_path_after_removal(&[0], &[1, 3]), vec![0, 3]);
        // An ancestor of the removed node is unaffected.
        assert_eq!(adjust_path_after_removal(&[1, 2], &[1]), vec![1]);
        // Diverge at the second level: only that index shifts.
        assert_eq!(adjust_path_after_removal(&[1, 0], &[1, 2]), vec![1, 1]);
    }
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

    #[test]
    fn whole_panel_show_does_not_seed_while_a_pane_is_open() {
        // A whole-panel show with a pane still open elsewhere must reveal it,
        // not spawn a fresh pane in the emptied dock.
        assert!(!should_seed_empty(true, true, true));
        // With nothing open anywhere it does seed, or "show" would be empty.
        assert!(should_seed_empty(false, true, true));
        // A side-specific show always seeds an empty dock it was asked to show.
        assert!(should_seed_empty(true, true, false));
        // No seed requested is never overridden.
        assert!(!should_seed_empty(false, false, false));
    }
}

#[cfg(test)]
mod nav_history_tests {
    use super::{NavHistory, ViewEntry};
    use std::path::PathBuf;

    fn dir(s: &str) -> ViewEntry {
        ViewEntry::Dir(PathBuf::from(s))
    }

    #[test]
    fn records_visits_in_order_and_navigates_both_ways() {
        let mut h = NavHistory::default();
        h.record(dir("/a"));
        h.record(dir("/b"));
        h.record(dir("/c"));
        assert!(h.can_back());
        assert!(!h.can_forward());

        assert_eq!(h.back(), Some(dir("/b")));
        h.finish_navigation();
        assert_eq!(h.back(), Some(dir("/a")));
        h.finish_navigation();
        assert!(!h.can_back());
        assert!(h.can_forward());

        assert_eq!(h.forward(), Some(dir("/b")));
        h.finish_navigation();
        assert_eq!(h.forward(), Some(dir("/c")));
        h.finish_navigation();
        assert!(h.can_back());
        assert!(!h.can_forward());
    }

    #[test]
    fn a_new_visit_after_going_back_truncates_the_forward_tail() {
        let mut h = NavHistory::default();
        h.record(dir("/a"));
        h.record(dir("/b"));
        h.record(dir("/c"));
        assert_eq!(h.back(), Some(dir("/b")));
        h.finish_navigation();
        // Visiting /d from /b drops /c from the forward history.
        h.record(dir("/d"));
        assert!(!h.can_forward());
        assert_eq!(h.back(), Some(dir("/b")));
    }

    #[test]
    fn navigating_does_not_record_and_repeats_are_ignored() {
        let mut h = NavHistory::default();
        h.record(dir("/a"));
        h.record(dir("/b"));
        // Back, then the resulting RootChanged; recording must be suppressed.
        let target = h.back().unwrap();
        assert_eq!(target, dir("/a"));
        h.record(target.clone());
        h.finish_navigation();
        // Still at /a with /b ahead, and no duplicate /a entry was appended.
        assert!(h.can_forward());
        assert_eq!(h.forward(), Some(dir("/b")));
        h.finish_navigation();

        // Re-recording the current entry is a no-op.
        h.record(dir("/b"));
        assert_eq!(h.cursor, 1);
        assert_eq!(h.entries.len(), 2);
    }

    #[test]
    fn bookmarks_view_is_a_history_entry() {
        let mut h = NavHistory::default();
        h.record(ViewEntry::Bookmarks);
        h.record(dir("/a"));
        // From /a, back goes to the bookmarks view.
        assert_eq!(h.back(), Some(ViewEntry::Bookmarks));
        h.finish_navigation();
        // Re-recording the view we are already on is a no-op.
        h.record(ViewEntry::Bookmarks);
        assert_eq!(h.entries.len(), 2);
        // And forward returns to the directory.
        assert_eq!(h.forward(), Some(dir("/a")));
        h.finish_navigation();
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
