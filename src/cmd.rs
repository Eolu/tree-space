//! Launch arguments and the IPC wire format for controlling a running panel.
//!
//! A tree-space invocation parses into a [`Command`]:
//!
//! * `tree-space`                     → toggle every dock together
//! * `tree-space --side right`        → toggle just the right dock
//! * `tree-space <dir>...`              → open the given directory (pane) root(s)
//! * `tree-space --side left <dir>...`  → open the given root(s) in the left dock
//! * `tree-space <file>...`             → reveal each file in its parent directory
//! * `tree-space --select <path>`     → reveal an explicit path (file or dir)
//! * `tree-space --hidden`            → launch/keep hidden (never shows)
//! * `tree-space --width 420`         → set the panel width (absolute pixels)
//! * `tree-space --width +40`         → widen the running panel by 40px (`-40` narrows)
//!
//! If a tree-space server is already running, the new invocation serializes
//! its [`Command`] to the instance socket and exits; the server feeds it back
//! to the app via [`crate::ui::app::AppMsg::LaunchRequest`]. The wire format is
//! deliberately simple: `side=`/`root=`/`hidden=`/`width=` tokens joined with
//! NULs (impossible in Unix path components) and terminated by a newline.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::PanelSide;

/// Turn a possibly-relative path (e.g. `.`, `sub/dir`) into an absolute one by
/// resolving it against the current working directory, without touching the
/// filesystem (no symlink resolution, and it works for paths that do not exist
/// yet). A path that is already absolute is returned unchanged.
///
/// This is applied at parse time so every downstream consumer — deduplication,
/// the session file, and the toolbar's path entry — works with one canonical
/// spelling instead of the raw argument.
fn absolutize(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

/// The value of a `--width` argument: an absolute width or a signed delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidthArg {
    /// Set the width to exactly this many pixels.
    To(u32),
    /// Change the width by this many pixels (negative narrows).
    By(i32),
}

impl WidthArg {
    /// Parse a raw `--width` value: a leading `+`/`-` makes it a delta,
    /// anything else an absolute pixel width.
    fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.starts_with(['+', '-']) {
            value.parse::<i32>().ok().map(Self::By)
        } else {
            value.parse::<u32>().ok().map(Self::To)
        }
    }

    fn encode(self) -> String {
        match self {
            Self::To(px) => px.to_string(),
            Self::By(delta) if delta >= 0 => format!("+{delta}"),
            Self::By(delta) => delta.to_string(),
        }
    }
}

/// What a tree-space invocation wants the panel to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    /// An explicit `--side` override. Absent means "use the configured side".
    pub side: Option<PanelSide>,
    /// Directory roots to open as panes. Empty means this is a show/toggle.
    pub roots: Vec<PathBuf>,
    /// Files (or paths) to reveal: each opens a pane rooted at its containing
    /// directory and selects the path once the pane is loaded. Populated by
    /// `--select PATH` and by positional arguments that name a non-directory.
    pub reveal: Vec<PathBuf>,
    /// `--hidden`: do everything else as usual, but ensure the panel ends up
    /// hidden — hide it if showing, leave it hidden if already hidden, and
    /// start hidden when this launches the panel.
    pub hidden: bool,
    /// `--width`: resize the panel (absolute or relative) without touching its
    /// visibility.
    pub width: Option<WidthArg>,
}

impl Command {
    /// Parse the arguments of a tree-space invocation (everything after
    /// argv[0], as `OsString`s). Only arguments that are existing directories
    /// become roots; `--side left|right` (or `--side=left|right`) selects the
    /// side override. Unknown flags are ignored for forward compatibility.
    pub fn parse<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut side = None;
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut reveal: Vec<PathBuf> = Vec::new();
        let mut hidden = false;
        let mut width = None;
        let mut args = args.into_iter().map(Into::into);
        while let Some(arg) = args.next() {
            let text = arg.to_string_lossy();
            let mut split = text.splitn(2, '=');
            let (flag, inline_value) = (split.next().unwrap_or(""), split.next());
            match flag {
                "--side" | "-s" => {
                    let value = match inline_value {
                        Some(v) => Some(v.to_string()),
                        None => args.next().map(|v| v.to_string_lossy().into_owned()),
                    };
                    match value.as_deref() {
                        Some("left") => side = Some(PanelSide::Left),
                        Some("right") => side = Some(PanelSide::Right),
                        _ => {} // invalid side value is ignored
                    }
                }
                "--hidden" | "--hide" | "-H" => hidden = true,
                "--width" | "-w" => {
                    let value = match inline_value {
                        Some(v) => Some(v.to_string()),
                        None => args.next().map(|v| v.to_string_lossy().into_owned()),
                    };
                    if let Some(parsed) = value.as_deref().and_then(WidthArg::parse) {
                        width = Some(parsed);
                    }
                }
                "--select" => {
                    // Accept `--select PATH` and `--select=PATH`; a lone
                    // `--select` (with no value) is ignored.
                    let value = match inline_value {
                        Some(v) => Some(v.to_string()),
                        None => args.next().map(|v| v.to_string_lossy().into_owned()),
                    };
                    if let Some(value) = value
                        && !value.is_empty()
                    {
                        reveal.push(absolutize(PathBuf::from(value)));
                    }
                }
                _ => {
                    if arg.to_str().is_some_and(|s| s.starts_with('-')) {
                        continue;
                    }
                    let path = absolutize(PathBuf::from(&arg));
                    if path.is_dir() {
                        roots.push(path);
                    } else {
                        // A file (or any non-directory path): reveal it in a
                        // pane rooted at its parent.
                        reveal.push(path);
                    }
                }
            }
        }
        Command { side, roots, reveal, hidden, width }
    }

    /// Encode into the IPC wire format (a single line, NUL-separated tokens).
    pub fn encode(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match self.side {
            Some(PanelSide::Left) => parts.push("side=left".to_owned()),
            Some(PanelSide::Right) => parts.push("side=right".to_owned()),
            None => {}
        }
        for root in &self.roots {
            parts.push(format!("root={}", root.to_string_lossy()));
        }
        for path in &self.reveal {
            parts.push(format!("select={}", path.to_string_lossy()));
        }
        if self.hidden {
            parts.push("hidden=1".to_owned());
        }
        if let Some(width) = self.width {
            parts.push(format!("width={}", width.encode()));
        }
        parts.join("\0")
    }

    /// Decode a line from the IPC wire format back into a [`Command`]. An empty
    /// (or whitespace-only) line decodes to a plain toggle.
    pub fn decode(line: &str) -> Option<Command> {
        let mut side = None;
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut reveal: Vec<PathBuf> = Vec::new();
        let mut hidden = false;
        let mut width = None;
        for token in line.split('\0') {
            if token.is_empty() {
                continue;
            }
            let (name, value) = token.split_once('=')?;
            match name {
                "side" => match value {
                    "left" => side = Some(PanelSide::Left),
                    "right" => side = Some(PanelSide::Right),
                    _ => return None,
                },
                "root" => roots.push(PathBuf::from(value)),
                "select" => reveal.push(PathBuf::from(value)),
                "hidden" => hidden = value == "1" || value.eq_ignore_ascii_case("true"),
                "width" => width = Some(WidthArg::parse(value)?),
                _ => return None,
            }
        }
        Some(Command { side, roots, reveal, hidden, width })
    }

    /// Does this command carry no directories and ask for the normal toggle
    /// (neither a side override, `--hidden`, nor `--width`)?
    pub fn is_toggle(&self) -> bool {
        self.roots.is_empty()
            && self.reveal.is_empty()
            && self.side.is_none()
            && !self.hidden
            && self.width.is_none()
    }

    /// Resolve each reveal path into a pane root plus an optional child to
    /// select. A directory (or the current directory `.`) opens as its own
    /// root with nothing to select; a file opens a pane rooted at its parent
    /// directory and selects the file. A path with no parent component (e.g.
    /// `/`) is skipped.
    pub fn reveal_targets(&self) -> Vec<(PathBuf, Option<PathBuf>)> {
        self.reveal
            .iter()
            .filter_map(|path| {
                if path.is_dir() {
                    Some((path.clone(), None))
                } else {
                    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
                    Some((parent.to_path_buf(), Some(path.clone())))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(args: &[&str]) -> Command {
        Command::parse(args.iter().map(|s| OsString::from(*s)))
    }

    #[test]
    fn no_args_is_a_toggle() {
        let cmd = parse_str(&[]);
        assert!(cmd.side.is_none());
        assert!(cmd.is_toggle());
    }

    #[test]
    fn side_flag_parses_both_inline_and_adjacent() {
        assert_eq!(parse_str(&["--side", "right"]).side, Some(PanelSide::Right));
        assert_eq!(parse_str(&["--side=left"]).side, Some(PanelSide::Left));
        assert_eq!(parse_str(&["-s", "right"]).side, Some(PanelSide::Right));
        assert_eq!(parse_str(&["-s=left"]).side, Some(PanelSide::Left));
    }

    #[test]
    fn invalid_side_value_is_ignored() {
        assert!(parse_str(&["--side", "up"]).side.is_none());
        assert!(parse_str(&["--side=diagonal"]).side.is_none());
    }

    #[test]
    fn directories_become_roots_and_flags_are_skipped() {
        let cmd = parse_str(&["--side", "left", "/etc", "--bogus", "/usr"]);
        assert_eq!(cmd.side, Some(PanelSide::Left));
        assert_eq!(cmd.roots, vec![PathBuf::from("/etc"), PathBuf::from("/usr")]);
    }

    #[test]
    fn non_directory_arguments_become_reveals() {
        // A path that is not an existing directory is revealed, not rooted.
        let cmd = parse_str(&["/definitely/not/a/dir"]);
        assert!(cmd.roots.is_empty());
        assert_eq!(cmd.reveal, vec![PathBuf::from("/definitely/not/a/dir")]);
        assert!(!cmd.is_toggle());
    }

    #[test]
    fn relative_paths_are_made_absolute() {
        // `.` names the current directory; it must be stored as an absolute
        // path so dedup and the toolbar agree on one spelling.
        let cmd = parse_str(&["."]);
        assert_eq!(cmd.roots.len(), 1, "`.` should resolve to an existing dir");
        assert!(cmd.roots[0].is_absolute(), "{:?} should be absolute", cmd.roots[0]);

        // A relative *reveal* is likewise absolutized (parent of "." is the
        // parent directory, and the target is the absolute cwd).
        let cmd = parse_str(&["--select", "."]);
        assert!(cmd.reveal[0].is_absolute(), "{:?} should be absolute", cmd.reveal[0]);
    }

    #[test]
    fn select_flag_parses_adjacent_inline_and_repeated() {
        assert_eq!(
            parse_str(&["--select", "/tmp/f.txt"]).reveal,
            vec![PathBuf::from("/tmp/f.txt")]
        );
        assert_eq!(parse_str(&["--select=/tmp/f.txt"]).reveal, vec![PathBuf::from("/tmp/f.txt")]);
        let two = parse_str(&["--select", "/a", "--select", "/b"]);
        assert_eq!(two.reveal, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        // A lone `--select` with no value is ignored.
        assert!(parse_str(&["--select"]).reveal.is_empty());
    }

    #[test]
    fn encode_decode_reveal() {
        let cmd = Command {
            reveal: vec![PathBuf::from("/home/eolu/notes.md")],
            ..Command::default()
        };
        assert_eq!(cmd.encode(), "select=/home/eolu/notes.md");
        assert_eq!(Command::decode(&cmd.encode()), Some(cmd));
    }

    #[test]
    fn reveal_targets_root_at_the_parent_for_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.md");
        std::fs::write(&file, "").unwrap();
        let sub = dir.path().join("photos");
        std::fs::create_dir(&sub).unwrap();

        let cmd = Command {
            reveal: vec![file.clone(), sub.clone()],
            ..Command::default()
        };
        let targets = cmd.reveal_targets();
        // A file reveals in its parent, selecting the file.
        assert_eq!(targets[0], (dir.path().to_path_buf(), Some(file)));
        // A directory opens as its own root with nothing to select.
        assert_eq!(targets[1], (sub, None));
    }

    #[test]
    fn reveal_targets_skip_rootless_files() {
        // A bare relative file name has an empty parent, so there is no
        // containing folder to open and it is skipped.
        let cmd = Command {
            reveal: vec![PathBuf::from("notes.md")],
            ..Command::default()
        };
        assert!(cmd.reveal_targets().is_empty());
    }

    #[test]
    fn hidden_flag_parses() {
        assert!(parse_str(&["--hidden"]).hidden);
        assert!(parse_str(&["--hide"]).hidden);
        assert!(parse_str(&["-H"]).hidden);
        assert!(!parse_str(&[]).hidden);
        // Hidden is orthogonal to side and roots.
        let cmd = parse_str(&["--hidden", "--side", "right", "/etc"]);
        assert!(cmd.hidden);
        assert_eq!(cmd.side, Some(PanelSide::Right));
        assert_eq!(cmd.roots, vec![PathBuf::from("/etc")]);
        assert!(!cmd.is_toggle());
    }

    #[test]
    fn encode_decode_round_trips() {
        let cmd = Command {
            side: Some(PanelSide::Right),
            roots: vec![PathBuf::from("/home/eolu/Projects"), PathBuf::from("/tmp")],
            reveal: vec![PathBuf::from("/home/eolu/some file.txt")],
            hidden: false,
            width: None,
        };
        assert_eq!(Command::decode(&cmd.encode()), Some(cmd));
    }

    #[test]
    fn encode_decode_hidden() {
        let cmd = Command { hidden: true, ..Command::default() };
        assert_eq!(Command::decode(&cmd.encode()), Some(cmd.clone()));
        assert_eq!(Command::decode("hidden=1"), Some(cmd));
    }

    #[test]
    fn encode_decode_toggle_and_empty() {
        let cmd = Command::default();
        assert_eq!(Command::decode(&cmd.encode()), Some(Command::default()));
        assert_eq!(Command::decode(""), Some(Command::default()));
        assert_eq!(Command::decode("   "), None);
    }

    #[test]
    fn decode_rejects_unknown_tokens() {
        assert_eq!(Command::decode("bogus=x"), None);
        assert_eq!(Command::decode("side=up"), None);
    }

    #[test]
    fn decode_keeps_paths_with_spaces() {
        let cmd = Command {
            side: None,
            roots: vec![PathBuf::from("/home/eolu/My Projects")],
            hidden: false,
            width: None,
            ..Command::default()
        };
        assert_eq!(Command::decode(&cmd.encode()), Some(cmd));
    }

    #[test]
    fn width_flag_parses_absolute_and_relative() {
        assert_eq!(parse_str(&["--width", "420"]).width, Some(WidthArg::To(420)));
        assert_eq!(parse_str(&["--width=420"]).width, Some(WidthArg::To(420)));
        assert_eq!(parse_str(&["-w", "+40"]).width, Some(WidthArg::By(40)));
        assert_eq!(parse_str(&["--width", "-40"]).width, Some(WidthArg::By(-40)));
        assert_eq!(parse_str(&["--width=garbage"]).width, None);
        assert!(parse_str(&["--width", "420"]).width.is_some());
        // A width change is never a plain toggle.
        assert!(!parse_str(&["--width", "+40"]).is_toggle());
    }

    #[test]
    fn encode_decode_width() {
        let grow = Command { width: Some(WidthArg::By(40)), ..Command::default() };
        assert_eq!(grow.encode(), "width=+40");
        assert_eq!(Command::decode(&grow.encode()), Some(grow));

        let shrink = Command { width: Some(WidthArg::By(-40)), ..Command::default() };
        assert_eq!(shrink.encode(), "width=-40");
        assert_eq!(Command::decode(&shrink.encode()), Some(shrink));

        let exact = Command { width: Some(WidthArg::To(420)), ..Command::default() };
        assert_eq!(exact.encode(), "width=420");
        assert_eq!(Command::decode(&exact.encode()), Some(exact));

        assert_eq!(Command::decode("width=+oops"), None);
    }
}