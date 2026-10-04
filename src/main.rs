//! tree-space — a dockable, keyboard-first file manager panel for Wayland/Hyprland.
//!
//! Usage:
//!   ts                            — start the panel, or toggle it if a
//!                                           panel is already running
//!   ts --side left|right          — dock to that screen edge
//!                                           (default: the configured side)
//!   ts /path/a [/path/b ...]        — open each directory as a pane
//!   ts --side right /path         — open a pane in the right dock
//!   ts /path/to/file              — reveal a file: open its folder and
//!                                           select it
//!   ts --select /path/to/anything — reveal an explicit file or folder
//!   ts --hidden                   — launch (or keep) the panel hidden
//!   ts --width 420                — set the panel width (absolute px)
//!   ts --width +40                — widen the panel by 40px (`-40` narrows)
//!
//! Directories may also be passed via the TREE_SPACE_DIRS environment variable
//! as a colon-separated list:
//!   TREE_SPACE_DIRS=/a:/b ts
//!
//! Single instance
//! ───────────────
//! Only one panel runs at a time. When an instance is already alive, a new
//! invocation forwards its intent over the instance socket (see `src/ipc.rs`)
//! and exits without starting a second panel:
//!   * no path, no side → toggle every dock together
//!   * `--side X`       → toggle just that side (create and show it if absent)
//!   * `--side X -H`    → hide just that side
//!   * with path(s)     → add a pane for each directory (never a duplicate of
//!     an already-open root), then show the panel
//!   * `--select P`     → open a pane for P's folder and select P
//!   * `--hidden`       → hide every dock; never shows
//!
//! Launch arguments are consumed here (via `Command`) and the process argv
//! handed to GApplication is reduced to the bare program name, so neither the
//! `--side` flag nor directory paths reach GLib's option parser.
//!
//! Desktop / XDG activation
//! ────────────────────────
//! The `GApplication` is created with `HANDLES_OPEN` and an `open` handler, so
//! a `.desktop` entry using `Exec=ts %u` (or `xdg-open`, or a portal
//! request) forwards the file/folder to the running panel instead of being
//! discarded. The handler funnels every opened file back through the same
//! instance socket, so it behaves exactly like `ts --select <path>`.

use std::path::PathBuf;

use relm4::gtk::gio;
use relm4::gtk::prelude::{ApplicationExtManual, FileExt};
use relm4::gtk;
use relm4::prelude::*;

use tree_space::cmd::Command;
use tree_space::freedesktop;
use tree_space::ipc;
use tree_space::ui::app::{App, AppInit};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }

    let mut command = Command::parse(std::env::args_os().skip(1));
    merge_env_dirs(&mut command);

    // Hand off to a running instance, if there is one.
    if ipc::deliver(&command) {
        return Ok(());
    }
    let listener = match ipc::bind() {
        Ok(listener) => Some(listener),
        Err(_) => {
            // Most likely lost a race with another starting instance: try to
            // hand off to it. Without a listener we can still run server-side,
            // just without the ability to receive further requests.
            if ipc::deliver(&command) {
                return Ok(());
            }
            None
        }
    };

    // The primary instance also serves the freedesktop `FileManager1` D-Bus
    // interface. Integrations that bypass the MIME default — the portal's
    // "Show in folder" and Electron's `showItemInFolder`, used by VS Code's
    // "Open Containing Folder" — call it directly, so without this they land in
    // whichever file manager ships that service (usually Nautilus).
    if listener.is_some() {
        freedesktop::serve_file_manager();
    }

    // Build the GApplication by hand so it carries `HANDLES_OPEN` and an `open`
    // handler: that is what lets desktop/X!DG activation (a `.desktop` with
    // `Exec=ts %u`, `xdg-open`, a portal request) hand us a file or
    // folder instead of silently discarding it. Every such request is turned
    // back into a [`Command`] and pushed through the same instance socket the
    // CLI uses, so it lands in the running panel as an ordinary reveal.
    let app = gtk::Application::new(
        Some("org.tree_space.panel"),
        gio::ApplicationFlags::HANDLES_OPEN,
    );
    app.connect_open(|_app, files, _hint| {
        let mut command = Command::default();
        for path in files.iter().filter_map(|f| f.path()) {
            command.reveal.push(path);
        }
        if !command.reveal.is_empty() {
            let _ = ipc::deliver(&command);
        }
    });

    let app = RelmApp::from_app(app)
        .visible_on_activate(!command.hidden)
        .with_args(vec!["ts".to_owned()]);
    app.run::<App>(AppInit { command, listener });

    Ok(())
}

/// Fold TREE_SPACE_DIRS=path1:path2 into the command's roots.
fn merge_env_dirs(command: &mut Command) {
    if let Ok(dirs) = std::env::var("TREE_SPACE_DIRS") {
        for part in dirs.split(':') {
            let trimmed = part.trim();
            let path = std::path::absolute(trimmed).unwrap_or_else(|_| PathBuf::from(trimmed));
            if path.is_dir() {
                command.roots.push(path);
            }
        }
    }
}

fn print_usage() {
    println!(
        "\
tree-space — a dockable, keyboard-first file manager panel.

USAGE:
    ts [OPTIONS] [PATH ...]

ARGS:
    PATH...  Paths to open. A directory opens as a pane (an already-open
           directory is never duplicated — the panel is just shown). A file
           opens its containing folder as a pane and selects the file.

OPTIONS:
    -s, --side <left|right>   Which screen edge to dock to. Defaults to the
                              configured side when omitted.
    --select <PATH>           Reveal PATH: open its containing folder and
                              select it. Works for a file or a folder, and is
                              the mechanism `.desktop`/`xdg-open` activation
                              uses (`Exec=ts %u`).
    -H, --hidden              Launch hidden (or, for a running panel, hide it
                              and keep it hidden). Never shows the panel.
    -w, --width <W>           Resize the panel. `420` sets an absolute width;
                              `+40` grows it by 40px and `-40` shrinks it.
                              Does not change whether the panel is shown.
    -k, --key <ACCEL>         Run a configured shortcut against the active pane
                              as if it were pressed (e.g. `Ctrl+c`, `F2`,
                              `Alt+Left`). Needs no keyboard focus; used by
                              external button decks. Does not change visibility.
    -h, --help                Print this help.

The panel is single-instance: an invocation while one is already running is
forwarded to it and exits. With no directory arguments it toggles the panel
(hide when visible, show when hidden); with `--side` it ensures a dock on that
side and shows it; with `--hidden` it ensures the panel is hidden instead."
    );
}