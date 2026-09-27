# tree-space

A wayland file manager that lives on the edge of your screen, inspired by the vscode explorer.

tree-space docks a keyboard-driven file tree to a screen edge, out of the way of your normal windows. It is built for
people who want a real, full-featured file manager that is always one keystroke
away instead of a window they have to open, arrange, and lose behind other
things. It runs best on Wayland compositors that implement `wlr-layer-shell`
(Hyprland, sway, river, niri, and other wlroots-based compositors).

The interface will feel familiar if you have used the file explorer in a text
editor: a hierarchical, keyboard-first tree with inline rename, previews, and a
status line, rather than a grid of tiles.

## What it does

The aim is feature parity with the file managers you already use, plus
configuration that most of them do not offer.

**Full file management.** Create, rename, duplicate, and delete files and
folders. Cut/copy/paste internally and to and from the system clipboard. Move
files to the trash (through GIO, so it is recoverable) or delete them
permanently. Create symlinks. Inspect and edit ownership and permissions. Copy
absolute or root-relative paths.

**Drag and drop, both directions.** Drag files out to any other application
(editor, terminal, browser, chat client) and drop files from other applications
in. Dropping onto a folder moves the files there; hold Ctrl to copy. Drag the
whole selection, not just one row.

**Navigation.** Per-pane back/forward history, open a folder as a new split pane,
open it in the opposite dock, filter the current listing, and choose what the
panel opens to on startup.

**Previews.** Toggle an inline preview below any row: image thumbnails inline, or
an expandable player for video and audio (play/pause, seek, volume). Animated
GIFs and videos play on click.

**Details when you need them.** A Properties dialog modelled on Nautilus, with
editable name, MIME type, size, timestamps, free space, link targets, and a
live permissions/ownership editor.

**Selection.** Ctrl+click, Shift+click, Shift+Arrow, and Ctrl+Shift+Arrow
selection, with a context menu that acts on the whole selection at once.

**Configurable context menus and hotkeys.** Every context menu is an ordered list of rules, and a rule can
match on almost anything: whether the row is a directory, files with no
extension, one or more filename extensions, a regular expression against the
full path, a multi-row selection, or everything else as a fallback. The first
rule that matches supplies the whole menu, so you can give `.rs` files their own
actions, give files under `~/Projects` a different set, and bind keys to any of
them. Menus can nest. Rules can live in separate files so they do not clutter
the main config. See [Context menus](#context-menus).

> Docking needs the `wlr-layer-shell` protocol. On a compositor that does not
> provide it (GNOME/Mutter, KDE/KWin) the panel cannot dock: it opens as a
> normal, freely floating window instead and prints a one-line notice on
> startup. Everything else works; it just does not stick to the screen edge.

## Usage

```
tree-space [OPTIONS] [PATH ...]
```

Run with no arguments it starts the panel (or toggles an already-running one).
Passing one or more paths opens a pane for each — a directory opens itself, a
file opens its containing folder and selects the file.

| Argument | Meaning |
| --- | --- |
| `PATH ...` | Paths to open. A directory opens as a pane (an already-open directory is never duplicated). A file opens its folder and selects the file. |
| `-s, --side <left\|right>` | Which screen edge to dock to. Defaults to the configured side. |
| `--select <PATH>` | Reveal a path: open its containing folder and select it. This is what the `.desktop` entry uses. |
| `-H, --hidden` | Launch (or keep) the panel hidden. Never shows it. |
| `-w, --width <W>` | Resize a dock: `420` is an absolute width in px, `+40`/`-40` is a delta. `--side` picks the dock. Never changes visibility. |
| `-h, --help` | Print the usage message. |

```
tree-space                        # start the panel, or toggle it if it is open
tree-space ~/dev                  # open a pane rooted there
tree-space ~/notes.md             # open the folder and select notes.md
tree-space --side right /tmp      # dock a pane to the right edge
tree-space --hidden               # start hidden (or hide a running panel)
tree-space --width +40            # widen the running panel by 40px
TREE_SPACE_DIRS=/a:/b tree-space  # open panes for directories from the environment
```

Only one panel runs at a time. A second invocation forwards its intent to the
running instance over a socket and exits; see
[Single instance](#single-instance) for the exact rules.

## Configuration

Everything is optional. On first launch tree-space writes a commented
`config.toml` and `main.css` to `~/.config/tree-space/` for you to edit. A
partial config is merged with the defaults, a broken one falls back to the
defaults with a message in the status bar, and out-of-range values are clamped.

```toml
[panel]
side = "left"      # which edge to dock to: "left" | "right"
layer = "bottom"   # stacking layer: "background" | "bottom" | "top" | "overlay"
width = 300        # width in px
margin = 0         # gap from the screen edge in px

[tree]
dirs_first = true          # list directories above files
sort_key = "name"          # "name" | "size" | "modified" | "type"
sort_ascending = true
show_hidden = false
font_size = 13
icon_size = 18
confirm_drop_move = false  # ask before a drag-and-drop move
```

Two things are worth calling out:

- **Context menus and hotkeys** are configured with `[[context_menu.rules]]`.
  This is the heart of the project; the full syntax is in
  [Context menus](#context-menus).
- **The stylesheet** is a plain CSS file at `~/.config/tree-space/main.css`. Edit
  it and restart to restyle the tree, menus, dialogs, and drag badges. See
  [Styling](#styling).

## Contributing

The project is developed against Hyprland and is not yet tested on other
compositors; reports and fixes for those are welcome. It is Wayland-only by
design and there is no plan to support X11.

If you hit something that a file manager "should" do, that is exactly the kind
of issue worth filing — feature parity is the goal.

---

# Reference

The rest of this document is the detailed reference: the exact menu syntax,
every built-in action and its default shortcut, the command-line behaviour, and
the internals of a few features.

## Context menus

Right-click menus on tree rows are configurable. The config is an ordered list
of rules, checked top-down; the **first rule with a matching entry wins** and
supplies the whole menu. Rows that no rule claims get the built-in default menu
(the classic file-manager items with their default shortcuts).

```toml
[[context_menu.rules]]
matches = ["ext:rs", "ext:toml"]                       # files ending in .rs or .toml
items = [
    "Open",
    { command = "foot -D {dir}", label = "Open in Terminal" },
    "---",                                             # horizontal divider
    { action = "Cut", shortcut = "Ctrl+x" },
    { command = "cargo fmt", label = "Format", shortcut = "Ctrl+Shift+f" },
]

[[context_menu.rules]]
matches = ["dir"]                                      # any directory
items = ["Open in Split View", "New Folder", "Move to Trash", "Delete Permanently"]

[[context_menu.rules]]
matches = ["noext"]                                    # files with no extension
items = ["Open", { command = "sh {path}" }]

[[context_menu.rules]]
matches = ['regex:.*\.lock$']                          # lockfiles anywhere in the path
items = ["Open", { command = "rm {path}", label = "Discard lock" }]

[[context_menu.rules]]
matches = ['regex:^/home/me/Projects/']                # anything under this directory
items = ["Open", { command = "foot -D {dir}", label = "Open in Terminal" }, "Open in Split View"]

[[context_menu.rules]]
matches = ["fallback"]                                 # anything not claimed above
items = ["Open", "Duplicate", "Copy Path"]
```

Right-clicking the blank area below the rows opens the menu for the directory
currently open in the pane. Left-clicking that area clears the selection; while
nothing is selected, action shortcuts target the open directory (`New File`,
`Paste`, `Open in Terminal`, ...) and row-specific ones (`Rename`, `Delete`, ...)
do nothing.

### `matches`

A rule matches a row when any of its matchers does.

| Matcher | Matches |
| --- | --- |
| `"dir"` / `"directory"` | Directories. |
| `"noext"` | Files with no extension (no `.` in the name past the leading dot of a hidden file). |
| `"ext:NAME"` | Files whose name ends with `.NAME`, case-insensitive. Multi-part suffixes match from the end (`ext:gz` matches both `a.gz` and `a.tar.gz`), so put more specific rules first. Only applies to files. |
| `"regex:PATTERN"` | The **full row path** against a Rust regex. Applies to files and directories alike. Case-sensitive by default; opt out with `(?i)`. Because the whole path is matched, name-only patterns should anchor at the end, e.g. `'regex:\.md$'`. |
| `"fallback"` / `"*"` / `"all"` | Everything. |
| `"multi"` | A multi-row selection (two or more rows). Describes the selection rather than one row, so it never matches a single path; when more than one row is selected, a `multi` rule takes precedence over the per-path rules. |

> Regex patterns live in a TOML string, so `\` needs escaping. Use a literal
> string to write the pattern verbatim: `matches = ['regex:\.md$']`. A pattern
> that fails to compile falls the whole config back to defaults, reported in the
> status bar.

### Splitting rules across files (`include`)

A rule can be an **include** instead of a match, so extra rules live in their own
files yet still apply at the right point in the first-match-wins order:

```toml
[[context_menu.rules]]
matches = ["dir"]
items = [ ... ]

[[context_menu.rules]]
include = "rules.d"        # every *.toml in rules.d, spliced in here, in name order

[[context_menu.rules]]
matches = ["fallback"]
items = [ ... ]
```

Every `*.toml` directly inside `rules.d` (relative to the config file, or
absolute) supplies its own `[[context_menu.rules]]`, inserted exactly where the
include rule sits — so your rules are checked **before** the `fallback` rule
without editing the main config. Files are read in filename order, so use
numeric prefixes (`10-git.toml`, `20-images.toml`) to control precedence.
Includes nest. A missing directory contributes nothing; an unparsable drop-in is
reported in the status bar and skipped, and the rest still loads.

For example, `~/.config/tree-space/rules.d/10-projects.toml`:

```toml
[[context_menu.rules]]
matches = ['regex:^/home/me/Projects/']
items = [
    "Open in Split View",
    { command = "code {dir}", label = "Open in VS Code" },
]
```

### `items`

Each entry takes one of four forms:

```toml
items = [
    "Open",                                            # built-in, plain string
    { action = "Cut", shortcut = "Ctrl+x" },           # built-in with an override
    { command = "code {path}", label = "Open in editor", shortcut = "Ctrl+e" },
    { label = "More", items = [ "Properties", "Duplicate" ] },  # submenu
]
```

`"---"` (or `"separator"`) inserts a horizontal divider.

A **submenu** (`{ label = "...", items = [ ... ] }`) opens a nested menu when
clicked. Its `items` may contain anything a top-level menu can, including
further submenus, nested to any depth. Shortcuts declared inside submenus stay
live exactly like top-level ones.

Any table entry (a built-in, a custom command, or a submenu) accepts **`hidden =
true`**: the item is not drawn, but its shortcut still fires. This is how you
bind a key without cluttering the menu — for example, add
`{ action = "Paste", shortcut = "Ctrl+Shift+v" }` with `hidden = true` next to
the visible Paste. A plain-string built-in cannot be hidden; write it as a table
(`{ action = "Up One Level", hidden = true }`) instead.

### Custom commands

- `{path}` is replaced with the row's full path.
- `{dir}` is replaced with the row itself if it is a directory, otherwise with its parent.
- If the command contains neither marker, the row's path is appended as the final argument.
- The template is split on whitespace; tokens are not shell-interpreted.
- `label` is optional and defaults to the command text.

A custom command in a `multi` rule runs once per selected row, top to bottom.

### Shortcuts

Any item, built-in or custom, can carry a `shortcut`, written as modifiers joined
with `+` (`"Ctrl+x"`, `"Ctrl+Shift+m"`, `"F2"`, `"Delete"`). The native GTK
`<Control>x` spelling works too. While the tree has focus the key fires the item
against the row under the keyboard cursor (the row is selected first, so
selection-based actions behave exactly as from the menu). Shortcuts are collected
from every rule, so a binding stays live even when the current row would show a
different menu; the first binding for a key wins.

| Action | Default |
| --- | --- |
| Cut / Copy / Paste | `Ctrl+x` / `Ctrl+c` / `Ctrl+v` |
| Duplicate | `Ctrl+d` |
| New File / New Folder | `Ctrl+n` / `Ctrl+Shift+n` |
| Create Link | `Ctrl+Shift+m` |
| View Thumbnail | `Ctrl+t` |
| Rename | `F2` |
| Move to Trash / Delete Permanently | `Delete` / `Shift+Delete` |
| Toggle Hidden Files | `Ctrl+h` |
| Sort by Name / Size / Modified / Type | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` / `Ctrl+4` |
| Reverse Sort Order | `Ctrl+Shift+r` |
| Back / Forward / Up One Level (pane) | `Alt+Left` / `Alt+Right` / `Alt+Up` |
| Close Pane (pane) | `Ctrl+w` |

A plain-string built-in uses its default shortcut; the table form
(`{ action = "Cut", shortcut = "Ctrl+Alt+x" }`) overrides it. `Ctrl+o` (open a
folder) and the navigation keys (`↑`, `↓`, `←`, `→`, Enter, Esc) are fixed.

### Built-in actions

The plain-string spelling is also the menu label.

- `"Open"` — make a directory this pane's root (replacing the current one), or open a file in its default app. Expanding and collapsing stay on plain left-click and the arrow keys.
- `"In new panel"` — open a directory as a new pane in this dock (equivalent to split view).
- `"In opposite panel"` — open a directory in the dock on the other side of the screen. The label follows the side the tree is on ("In right panel" / "In left panel").
- `"Open in Split View"` — open a directory as a new split pane.
- `"Open With..."` — pick an application for the row.
- `"Open With Default"` — open the file with its default app; the label names the app ("Open With Firefox").
- `"View Thumbnail"` — toggle an inline preview below the row. See [Previews](#previews).
- `"New File"` / `"New Folder"` — create in the row's directory.
- `"Cut"` / `"Copy"` / `"Paste"` — clipboard operations on the selection.
- `"Rename"` / `"Duplicate"` — rename or duplicate the row.
- `"Create Link"` — create a symlink next to the row.
- `"Properties"` — open the Properties dialog.
- `"Copy Path"` / `"Copy Relative Path"` — copy the absolute or root-relative path.
- `"Move to Trash"` / `"Delete Permanently"`.

The panel/split actions (`"In new panel"`, `"In opposite panel"`, `"Open in Split
View"`) only apply to directories and are dropped from file rows; `"Open With
Default"` only applies to files.

## Previews

`"View Thumbnail"` toggles an inline preview below a row: a preview of an image
file, or of every previewable file inside a directory. Images show inline;
animated GIFs and videos show their first frame and play on click (videos
looping, with audio, via the built-in player). For audio files the item reads
**"Show Player"** and expands a compact transport: play/pause, seek, elapsed and
total time, and volume.

Previews are sized to the column (images never enlarge past their natural size)
and are non-persistent — they disappear when the directory collapses or the pane
closes.

> Playback goes through GStreamer, so the usual codec packages must be installed:
> `gst-plugins-good` for common containers and formats (MP4, MKV, WebM, MOV,
> MP3, ...), plus `gst-libav` / `gst-plugins-ugly` for some codecs. Without them a
> preview stays blank (GIFs use gdk-pixbuf and always work).
>
> Audio playback is behind the default `audio` Cargo feature. Build with
> `--no-default-features` to drop the `gstreamer` binding dependency entirely;
> audio files then get no inline player but still open normally.

## Properties

`"Properties"` opens a dialog with three tabs, modelled on Nautilus:

- **Basic** — an editable **Name** field (Enter renames the row, updating the model, watcher, and selection in place), the MIME **Type**, the **Size** (bytes for a file, "N items" for a directory), the **Link target** for a symlink, the **Parent folder**, the filesystem **Free space**, and the **Accessed / Modified / Created** timestamps.
- **Permissions** — **Owner** and **Group** pickers (searchable; changing one runs `chown` for that id only) and the classic **Read / Write / Execute** matrix for owner/group/others, plus **Set user ID**, **Set group ID**, and **Sticky**. Every change applies immediately and reverts with an error in the footer if the kernel refuses (for example a broken symlink, or an owner change as a non-root user).
- **Image** — for image files, the MIME type and pixel **Dimensions**, read from the file header (PNG, JPEG, GIF, WebP, BMP).

When several rows are selected, Properties shows a read-only summary of the
selection: the count, combined size, shared parent folder, and the list of names.

## Pane menu (hamburger)

The ☰ button in each pane's toolbar opens a configurable menu. Unlike the
per-row context menus it has no match rules: `[pane_menu] items` is one flat list
of actions that apply to the whole pane. It uses the same item syntax as the
context menus.

```toml
[pane_menu]
items = [
    "Open Folder...",
    "Filter...",
    "Split View",
    "---",
    { action = "Back", shortcut = "Alt+Left" },
    { action = "Forward", shortcut = "Alt+Right" },
    { action = "Up One Level", shortcut = "Alt+Up" },
    "---",
    { action = "Toggle Hidden Files", shortcut = "Ctrl+h" },
    { action = "Sort by Name", shortcut = "Ctrl+1" },
    { action = "Sort by Size", shortcut = "Ctrl+2" },
    { action = "Sort by Modified", shortcut = "Ctrl+3" },
    { action = "Sort by Type", shortcut = "Ctrl+4" },
    { action = "Reverse Sort Order", shortcut = "Ctrl+Shift+r" },
    "---",
    "Collapse",
    { action = "Close Pane", shortcut = "Ctrl+w" },
]
```

Pane-menu actions:

- `"Open Folder..."` / `"Filter..."` / `"Split View"` / `"Collapse"` / `"Close Pane"` — the toolbar's own actions.
- `"Back"` / `"Forward"` / `"Up One Level"` — navigation. Each pane keeps its own visit history; `Up One Level` opens the enclosing folder (a no-op at the filesystem root).
- `"Toggle Hidden Files"`, `"Sort by Name/Size/Modified/Type"`, `"Reverse Sort Order"` — view actions applied to every open directory in the pane for the current session. The `[tree]` keys make them permanent.

Shortcuts on pane-menu items fire anywhere in the pane — the tree, the path
entry, the filter bar, or with nothing focused — so a pane action like
`"Split View"` works regardless of what is selected. Row shortcuts (context-menu
items) still require the tree to have focus.

## Styling

`~/.config/tree-space/main.css` (written on first launch) defines the panel's
look. Edit it and restart to restyle the tree, menus, property dialogs, badges,
and everything else. The window class names (`tree-row`, `tree-menu`,
`hamburger-menu`, `props`, ...) are what the shipped file targets, so you can
adapt its rules directly.

`tree.font_size` from the config is applied **after** your stylesheet, so it
always wins over any font size you set for the row and menu fonts.

## Startup

Where the panel opens when launched with **no path argument**:

```toml
startup = "home"                      # always open the home directory (default)
# startup = "last"                    # reopen the last-used directory
# startup = { path = "/some/dir" }    # always open a fixed directory (~ = $HOME)
```

An explicit path argument on the command line always takes precedence.

## Selecting rows

- **Click** a row to select it; Ctrl+click toggles a row into or out of the selection; Shift+click extends from the last anchor.
- **Shift+Up / Shift+Down** extend the selection with the keyboard; **Ctrl+Shift+Up / Ctrl+Shift+Down** move the cursor while keeping the anchor, so you can grow or shrink the block from either end.
- **Ctrl+a** selects every visible row.

Right-clicking a row that is part of a multi-selection keeps the whole selection
and opens the [multi-select menu](#matches); right-clicking any other row
collapses onto it. Cut, Copy, Move to Trash, Delete Permanently, Copy Path, Copy
Relative Path, and Properties all act on every selected row.

## Drag and drop

Drag rows out to other applications to hand them the files. Dragging a row that
is part of a multi-row selection drags the whole selection. The drop advertises
both a `GdkFileList` and a `text/uri-list` payload, so GTK and non-GTK apps all
accept it, and a small "N items" badge follows the pointer.

Dragging a row onto a directory inside the panel, or dropping files in from
another application, moves them there. The move happens immediately by default;
set `tree.confirm_drop_move = true` to be asked first. Hold **Ctrl** while
dropping to copy instead. Dropping a row onto the directory it already lives in
is ignored. When a drag leaves the panel and the receiving application takes it
as a **move**, the source rows are sent to the trash.

## Single instance

Only one panel runs at a time. When an instance is already alive, a new
invocation forwards its intent over the instance socket and exits:

- no path, no `--side` — toggles every dock together: hides them all when any is visible, otherwise shows them all.
- `--side X` (no path) — toggles just that side, creating and showing a dock there if none exists yet.
- `--side X --hidden` — hides just that side (a no-op when it does not exist).
- `--hidden` alone — hides every dock. It never shows.
- with path(s) — adds a pane for each directory (never duplicating an open root), then shows the panel.
- with `--select P` / a file path — opens a pane for `P`'s folder, selects `P`, then shows the panel.
- `--width W` — resizes a dock and never touches visibility.

Directories may also be passed via `TREE_SPACE_DIRS` as a colon-separated list.

## Using tree-space as your file manager

tree-space registers itself as an XDG handler for folders (`inode/directory`) and
`file://` URIs, so `xdg-open <folder>`, portal "Show in folder" requests, and
double-clicking a folder elsewhere can all route here. The `.desktop` entry
(`resources/tree-space.desktop`) runs `tree-space --select %u`, which reveals the
path in the running panel or starts one.

Because tree-space is a docked panel rather than a normal window, "opening" a
folder shows and focuses the panel instead of spawning a window. The
`GApplication` advertises `HANDLES_OPEN`, so environments that activate it as an
application rather than running `Exec` directly are handled too; every opened
file is funnelled through the same socket.

## Resizing the panel

The panel is a layer-shell surface, so the compositor's own window resizing does
not apply. The left and right docks are sized independently:

- **Super + right-drag** anywhere on a dock resizes it live (a left dock grows as you drag right; a right dock mirrors that). A plain right-drag without the modifier still opens the row menu.
- **Super + plus / Super + minus** step that dock's width.
- **`tree-space --width +-N`** changes a dock's width from outside; `--side` picks which.

Each dock's width is remembered across launches. `[panel] width` is only the
initial default.

> Hyprland grabs `SUPER + plus/minus` and `SUPER + right-drag` globally, so the
> in-app shortcuts usually will not reach the panel. Bind the CLI instead, for
> example in `~/.config/hypr/bindings.lua`:
>
> ```lua
> o.bind("SUPER + Minus", "Narrow tree-space", "tree-space --width -24")
> o.bind("SUPER + Equal", "Widen tree-space",  "tree-space --width +24")
> ```
>
> Those target the primary dock; add `--side right` for the right dock.

## Keyboard focus

The panel uses `KeyboardMode::OnDemand`, so the compositor grants it keyboard
focus when it is mapped and while the pointer moves over it, and drops the claim
when the pointer leaves. Dropping the claim on pointer-leave matters on
Hyprland: a layer surface that keeps focus does not update the compositor's
notion of the focused window, which makes clicking the previously-focused window
a no-op. The launch pane and every newly split pane are handed focus
automatically, so a fresh panel or split is usable without a click.

## Building

```
cargo build --release
```

Wayland-only; there is no X11 support. The inline audio player is an optional
feature (on by default); build without the `gstreamer` binding dependency with:

```
cargo build --release --no-default-features
```

## License

MIT
