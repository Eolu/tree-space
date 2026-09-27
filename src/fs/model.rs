//! A lazy file-tree model.
//!
//! The model is deliberately free of GTK and I/O: it stores a small directory
//! cache keyed by path, expands lazily (children are only read when a directory
//! is expanded), and translates filesystem change events into incremental
//! cache updates. Directory content enters through the [`DirSource`] trait, so
//! the model can be unit-tested against an in-memory filesystem and driven by
//! the real `std::fs` reader in production ([`StdDirSource`]).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A single directory entry as discovered by a [`DirSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    /// File name (file stem last component), not the full path.
    pub name: String,
    /// Absolute path of the entry.
    pub path: PathBuf,
    /// Whether the entry is a directory (following symlinks).
    pub is_dir: bool,
    /// Whether the entry is a symlink.
    pub is_symlink: bool,
    /// Size in bytes (0 for directories; metadata that failed is 0).
    pub size: u64,
    /// Last modification time, when the source could read it.
    pub modified: Option<SystemTime>,
}

/// The seam from which the model obtains directory listings.
///
/// Implementations must return entries in arbitrary order; the model sorts.
pub trait DirSource {
    fn list_dir(&self, path: &Path) -> io::Result<Vec<EntryInfo>>;
}

/// Reads directory listings from the real filesystem via `std::fs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdDirSource;

impl DirSource for StdDirSource {
    fn list_dir(&self, path: &Path) -> io::Result<Vec<EntryInfo>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let is_symlink = file_type.is_symlink();
            // Following the symlink tells us whether it points at a directory,
            // which decides whether the entry can be expanded in the tree.
            let is_dir = if file_type.is_dir() {
                true
            } else if is_symlink {
                fs::metadata(entry.path()).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                false
            };
            let meta = entry.metadata().ok();
            let size = if is_dir {
                0
            } else {
                meta.as_ref().map(|m| m.len()).unwrap_or(0)
            };
            let modified = meta.as_ref().and_then(|m| m.modified().ok());
            entries.push(EntryInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                path: entry.path(),
                is_dir,
                is_symlink,
                size,
                modified,
            });
        }
        Ok(entries)
    }
}

/// The primary key used to order entries within a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Case-insensitive name.
    Name,
    /// Size in bytes (directories sort as 0 unless `dirs_first`).
    Size,
    /// Last modification time; entries without one sort first when ascending.
    Modified,
    /// Kind: directories, then grouped by extension, then name.
    Type,
}

impl SortKey {
    /// Short configuration token (`name`, `size`, `modified`, `type`).
    pub fn as_str(&self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Type => "type",
        }
    }

    /// Parse a configuration token; unknown values are an error.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "name" => Some(SortKey::Name),
            "size" => Some(SortKey::Size),
            "modified" | "mtime" | "date" => Some(SortKey::Modified),
            "type" | "kind" | "ext" | "extension" => Some(SortKey::Type),
            _ => None,
        }
    }
}

/// Sorting rules applied to every directory listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortOptions {
    /// Directories before files; names always sorted alphabetically
    /// (case-insensitively).
    pub dirs_first: bool,
    /// Primary ordering key.
    pub key: SortKey,
    /// Ascending when true, descending when false.
    pub ascending: bool,
}

impl Default for SortOptions {
    fn default() -> Self {
        Self {
            dirs_first: true,
            key: SortKey::Name,
            ascending: true,
        }
    }
}

/// The extension used as a type key: lowercase text after the final dot, or an
/// empty string when the name has none (or is a dotfile without an extension).
fn type_key(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_lowercase(),
        _ => String::new(),
    }
}

/// Compare two entries under the given rules. Directories always float to the
/// top when `dirs_first` is set (regardless of key or direction); the primary
/// `key` then orders within a group, with a lowercase-name tie-break for
/// stability.
fn compare_entries(a: &EntryInfo, b: &EntryInfo, opts: SortOptions) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    if opts.dirs_first {
        match (a.is_dir, b.is_dir) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
    }

    // Primary key. For Name, direction is applied normally; Size/Modified/
    // Type fall back to ascending name when the primary values tie.
    let primary = match opts.key {
        SortKey::Name => a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name)),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Modified => a.modified.cmp(&b.modified),
        SortKey::Type => type_key(&a.name)
            .cmp(&type_key(&b.name))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
    };

    if primary == Ordering::Equal && opts.key != SortKey::Name {
        let tie = a
            .name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name));
        return if opts.ascending { tie } else { tie.reverse() };
    }

    if opts.ascending {
        primary
    } else {
        primary.reverse()
    }
}

/// Compare entries, ready for use with `sort_by`.
fn sort_dir_entries(entries: &mut [EntryInfo], opts: SortOptions) {
    entries.sort_by(|a, b| compare_entries(a, b, opts));
}

/// Per-directory cached state inside the model.
#[derive(Debug, Default)]
pub struct DirNode {
    /// Whether the listing has been read (at least once).
    pub loaded: bool,
    /// Whether children should be rendered.
    pub expanded: bool,
    /// The last listing read (sorted, hidden-filtered).
    pub entries: Vec<EntryInfo>,
}

/// A change that happened on disk, in model vocabulary. Produced by
/// [`super::watcher`] from raw notify events and by file operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created { path: PathBuf },
    Removed { path: PathBuf },
    Renamed { from: PathBuf, to: PathBuf },
    Modified { path: PathBuf },
    /// The backing store is in an unknown state; drop every cache.
    Rescan,
}

/// One rendered line of the tree. The UI derives both its widget and its
/// indentation from this value; no GTK types leak in here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleRow {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub expanded: bool,
    pub has_children: bool,
    /// Whether the name matched the current filter. Used for highlighting.
    pub matches: bool,
}

/// The lazy tree model. All methods are `&mut self` because expand/collapse
/// mutate the cache; the UI mutates it freely since it owns it.
pub struct TreeModel {
    root: PathBuf,
    sort: SortOptions,
    show_hidden: bool,
    filter: String,
    /// Keyed by directory path. The root always has a node; other directories
    /// get nodes when first expanded and are dropped when collapsed again.
    dirs: HashMap<PathBuf, DirNode>,
}

impl TreeModel {
    /// Create a model rooted at `root`, applying `sort` and hiding dotfiles
    /// unless `show_hidden`. The root node is created eagerly but its listing
    /// is only read on demand (lazy — the panel can start without blocking).
    pub fn new(root: PathBuf, sort: SortOptions, show_hidden: bool) -> Self {
        let root_node = DirNode {
            loaded: false,
            expanded: true,
            entries: Vec::new(),
        };
        let mut dirs = HashMap::new();
        dirs.insert(root.clone(), root_node);
        Self {
            root,
            sort,
            show_hidden,
            filter: String::new(),
            dirs,
        }
    }

    /// The tree root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The current filter (lowercased). Empty means "no filter".
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Set the type-ahead filter. Empty string clears it.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.trim().to_lowercase();
    }

    /// Current sorting rules.
    pub fn sort(&self) -> SortOptions {
        self.sort
    }

    /// Replace the sorting rules. Callers must re-read loaded listings
    /// (see [`Self::reload_all`]) for the change to take effect, since nodes
    /// cache their already-sorted entries.
    pub fn set_sort(&mut self, sort: SortOptions) {
        self.sort = sort;
    }

    /// Whether hidden (dot) entries are currently shown.
    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    /// Toggle hidden-file visibility. Callers must re-read loaded listings
    /// (see [`Self::reload_all`]) for the change to take effect.
    pub fn set_show_hidden(&mut self, show_hidden: bool) {
        self.show_hidden = show_hidden;
    }

    /// Re-read every loaded directory so sort/hidden changes take effect.
    /// Expanded nodes are refreshed; collapses are unaffected. Errors on a
    /// single directory are ignored (that node simply becomes empty).
    pub fn reload_all(&mut self, src: &dyn DirSource) {
        let paths: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|(_, node)| node.loaded)
            .map(|(path, _)| path.clone())
            .collect();
        for path in paths {
            let _ = self.reload(&path, src);
        }
    }

    /// Whether `path` (strictly below, or equal to, the root) belongs to this tree.
    pub fn under_root(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// True when the cached node for `path` is considered expanded.
    pub fn is_expanded(&self, path: &Path) -> bool {
        self.dirs.get(path).is_some_and(|n| n.expanded)
    }

    /// Cached listing for `path`, if that directory has been loaded.
    pub fn node(&self, path: &Path) -> Option<&DirNode> {
        self.dirs.get(path)
    }

    /// Paths of every directory that is expanded *and* loaded — the set of
    /// directories the UI should be watching (the root is always in this set
    /// once open_root has expanded it). Used to keep the filesystem watcher
    /// aligned with the lazily-expanded tree.
    pub fn expanded_loaded_dirs(&self) -> Vec<PathBuf> {
        self.dirs
            .iter()
            .filter(|(_, node)| node.loaded && node.expanded)
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Expand `path`, reading its listing the first time. Collapsed state is
    /// untouched: expanding a previously loaded (then collapsed) node is cheap.
    pub fn expand(&mut self, path: &Path, src: &dyn DirSource) -> Result<(), io::Error> {
        if let Some(node) = self.dirs.get_mut(path)
            && node.loaded
            && node.expanded
        {
            return Ok(());
        }
        let result = self.reload(path, src);
        // `reload` preserves the old flag (absent for a fresh node -> false);
        // force the expansion the caller asked for.
        if let Some(node) = self.dirs.get_mut(path) {
            node.expanded = true;
        }
        result
    }

    /// Collapse `path` and drop its cached listing to free memory.
    pub fn collapse(&mut self, path: &Path) {
        if let Some(node) = self.dirs.get_mut(path) {
            node.expanded = false;
            node.entries.clear();
            node.loaded = false;
            self.remove_subtree_suffixes(path);
        }
    }

    /// Toggle expansion state of `path`, loading children on expand.
    pub fn toggle(&mut self, path: &Path, src: &dyn DirSource) -> Result<(), io::Error> {
        let expanded = self.is_expanded(path);
        if expanded {
            self.collapse(path);
            Ok(())
        } else {
            self.expand(path, src)
        }
    }

    /// Reload a previously loaded directory (e.g. after a change event).
    fn reload(&mut self, path: &Path, src: &dyn DirSource) -> Result<(), io::Error> {
        // Retain the expanded flag; reset everything else.
        let expanded = self.dirs.get(path).is_some_and(|n| n.expanded);
        let mut node = DirNode {
            // An unreadable directory becomes an empty (but loaded) node so we
            // do not retry every render; the error is returned to the caller.
            loaded: true,
            expanded,
            entries: Vec::new(),
        };
        let result = src.list_dir(path);
        match result {
            Ok(list) => {
                let mut entries = list;
                if !self.show_hidden {
                    entries.retain(|e| !is_hidden(&e.name));
                }
                sort_dir_entries(&mut entries, self.sort);
                node.entries = entries;
            }
            Err(err) => {
                self.dirs.insert(path.to_path_buf(), node);
                return Err(err);
            }
        }
        self.dirs.insert(path.to_path_buf(), node);
        Ok(())
    }

    /// Apply a change event. Only loaded directories are refreshed, so unloaded
    /// parts of the tree stay untouched (cheap) and lazily re-read later anyway.
    pub fn apply(&mut self, change: &Change, src: &dyn DirSource) {
        match change {
            Change::Created { path } => self.refresh_parent(path, src),
            Change::Removed { path } => {
                self.remove_subtree(path);
                self.refresh_parent(path, src);
            }
            Change::Renamed { from, to } => {
                self.remove_subtree(from);
                self.refresh_parent(from, src);
                self.refresh_parent(to, src);
            }
            Change::Modified { path } => {
                // Content and metadata edits do not change the tree structure;
                // only refresh a directory node if the modification touched one
                // (covers renames by tools that report Modify instead of
                // Create/Remove).
                if self.dirs.get(path).is_some_and(|n| n.loaded) {
                    let _ = self.reload(path, src);
                }
            }
            Change::Rescan => {
                // The backing store is untrusted (e.g. an inotify queue
                // overflow). Reload every directory that was loaded *and*
                // expanded — exactly the set the user can see — rather than
                // dropping the whole cache and reloading only the root, which
                // made a rescan look like a full tree collapse. Nodes that were
                // loaded but collapsed are simply dropped and lazily re-read
                // when next expanded.
                //
                // The reload is capped so a pathological tree cannot turn a
                // single rescan into an unbounded synchronous sweep; past the
                // cap the remaining nodes just fall back to lazy loading.
                const MAX_RESCAN_DIRS: usize = 512;
                let mut reload: Vec<PathBuf> = self
                    .dirs
                    .iter()
                    .filter(|(_, node)| node.loaded && node.expanded)
                    .map(|(path, _)| path.clone())
                    .collect();
                reload.sort();
                reload.truncate(MAX_RESCAN_DIRS);
                for node in self.dirs.values_mut() {
                    node.loaded = false;
                    node.entries.clear();
                }
                for path in reload {
                    let _ = self.reload(&path, src);
                }
            }
        }
    }

    /// Refresh the parent dir of `path` if there is one and it is loaded.
    fn refresh_parent(&mut self, path: &Path, src: &dyn DirSource) {
        let Some(parent) = path.parent() else {
            return; // root itself — we do not model / below the root.
        };
        if self.dirs.get(parent).is_some_and(|n| n.loaded) {
            let _ = self.reload(parent, src);
        }
    }

    /// Drop the node at `path` and every descendant cache below it.
    fn remove_subtree(&mut self, path: &Path) {
        let keys: Vec<PathBuf> = self
            .dirs
            .keys()
            .filter(|k| k.starts_with(path))
            .cloned()
            .collect();
        for key in keys {
            self.dirs.remove(&key);
        }
    }

    /// Remove cached descendants of `path`, but keep `path` itself.
    /// Used by [`Self::collapse`] so that re-expanding still knows the node.
    fn remove_subtree_suffixes(&mut self, path: &Path) {
        let keys: Vec<PathBuf> = self
            .dirs
            .keys()
            .filter(|k| k.starts_with(path) && *k != path)
            .cloned()
            .collect();
        for key in keys {
            self.dirs.remove(&key);
        }
    }

    /// Render the visible rows of the tree in display order.
    ///
    /// The root directory itself is **not** included as a row — its path is
    /// shown in the toolbar path entry instead. Children of the root start at
    /// depth 0.
    ///
    /// Filter semantics (MVP): the filter narrows the children shown beneath
    /// each expanded directory to name matches, and keeps a directory row
    /// visible (even when the name itself does not match) as long as a *loaded*
    /// descendant matches — the classic "show the path to the match" behaviour.
    /// Searching *inside* not-yet-expanded directories is intentionally not
    /// performed, to preserve lazy loading — full-tree search arrives with
    /// Phase 2 (ripgrep).
    pub fn visible_rows(&self) -> Vec<VisibleRow> {
        let mut rows = Vec::new();
        self.visible_rows_into(&mut rows);
        rows
    }

    /// Same as [`Self::visible_rows`], but clears and refills a caller-owned
    /// buffer. Reusing one `Vec` across rebuilds avoids re-allocating the full
    /// row list (and its `PathBuf`/`String` payloads) on every interaction.
    pub fn visible_rows_into(&self, out: &mut Vec<VisibleRow>) {
        out.clear();
        self.collect_children(&self.root, 0, out);
    }

    /// Render the children of `path` (one level at a time) into `out`.
    ///
    /// Each directory renders exactly one row — the one produced at its parent —
    /// so recursion here emits *children only*, never the directory itself.
    fn collect_children(&self, path: &Path, depth: usize, out: &mut Vec<VisibleRow>) {
        let Some(node) = self.dirs.get(path) else {
            return;
        };
        if !node.expanded || !node.loaded {
            return;
        }
        let filtering = !self.filter.is_empty();
        for entry in &node.entries {
            let name_matches =
                filtering && entry.name.to_lowercase().contains(&self.filter);
            // Directories are kept when their own name matches or any loaded
            // descendant does (so the path to the match stays visible).
            let kept = if filtering {
                if entry.is_dir {
                    name_matches || self.subtree_has_match(&entry.path)
                } else {
                    name_matches
                }
            } else {
                true
            };
            if !kept {
                continue;
            }
            let is_dir = entry.is_dir;
            let child_node = self.dirs.get(&entry.path);
            let expanded = child_node.is_some_and(|n| n.expanded);
            let has_children = child_node.is_some_and(|n| n.loaded && !n.entries.is_empty());
            out.push(VisibleRow {
                path: entry.path.clone(),
                name: entry.name.clone(),
                depth,
                is_dir,
                is_symlink: entry.is_symlink,
                expanded,
                has_children,
                matches: filtering && kept,
            });
            if is_dir {
                self.collect_children(&entry.path, depth + 1, out);
            }
        }
    }

    /// Whether any *loaded* descendant of `dir` matches the filter. Only
    /// expanded (thus loaded) directories are scanned, honouring the lazy
    /// "no peeking" rule of [`Self::visible_rows`].
    fn subtree_has_match(&self, dir: &Path) -> bool {
        let Some(node) = self.dirs.get(dir) else {
            return false;
        };
        if !node.loaded || !node.expanded {
            return false;
        }
        node.entries.iter().any(|e| {
            e.name.to_lowercase().contains(&self.filter)
                || (e.is_dir && self.subtree_has_match(&e.path))
        })
    }
}

/// A dotfile unless it is exactly `.` or `..` (never present in listings, but
/// staying defensive keeps dots on `.`/`..` safe if they ever leak through).
pub(crate) fn is_hidden(name: &str) -> bool {
    name.starts_with('.') && name != "." && name != ".."
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory filesystem for tests.
    #[derive(Default)]
    struct MemFs {
        dirs: HashMap<PathBuf, Vec<EntryInfo>>,
    }

    impl MemFs {
        fn dir(&mut self, path: impl AsRef<Path>, children: Vec<(&str, bool, bool)>) {
            let path = path.as_ref();
            let entries = children
                .into_iter()
                .map(|(name, is_dir, is_symlink)| EntryInfo {
                    name: name.to_owned(),
                    path: path.join(name),
                    is_dir,
                    is_symlink,
                    size: 0,
                    modified: None,
                })
                .collect();
            self.dirs.insert(path.to_path_buf(), entries);
        }

        /// Like [`Self::dir`], but each child carries a `(size, mtime_secs)`
        /// pair so size/date sorting can be exercised.
        fn dir_sized(&mut self, path: impl AsRef<Path>, children: Vec<(&str, bool, bool, u64, u64)>) {
            let path = path.as_ref();
            let entries = children
                .into_iter()
                .map(|(name, is_dir, is_symlink, size, secs)| EntryInfo {
                    name: name.to_owned(),
                    path: path.join(name),
                    is_dir,
                    is_symlink,
                    size,
                    modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)),
                })
                .collect();
            self.dirs.insert(path.to_path_buf(), entries);
        }
    }

    impl DirSource for MemFs {
        fn list_dir(&self, path: &Path) -> io::Result<Vec<EntryInfo>> {
            self.dirs.get(path).cloned().ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no such dir: {}", path.display()))
            })
        }
    }

    fn row_names(rows: &[VisibleRow]) -> Vec<String> {
        rows.iter().map(|r| r.name.clone()).collect()
    }

    #[test]
    fn visible_rows_into_reuses_and_clears_buffer() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("a", true, false), ("b.txt", false, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();

        // A buffer pre-seeded with a stale row must be cleared, not appended to,
        // yet keep its allocation.
        let mut buf = vec![VisibleRow {
            path: "/stale".into(),
            name: "stale".into(),
            depth: 9,
            is_dir: false,
            is_symlink: false,
            expanded: false,
            has_children: false,
            matches: false,
        }];
        model.visible_rows_into(&mut buf);
        assert_eq!(row_names(&buf), vec!["a", "b.txt"]);

        // Refilling with a collapsed tree empties it.
        model.collapse(Path::new("/r"));
        model.visible_rows_into(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn starts_with_just_the_root() {
        // Root itself is not a row (it shows in the toolbar); no children loaded
        // yet, so visible_rows is empty.
        let model = TreeModel::new("/r".into(), SortOptions::default(), false);
        let rows = model.visible_rows();
        assert_eq!(rows.len(), 0);
    }

    #[test]
    fn expand_is_lazy_and_shows_sorted_children() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("z.txt", false, false), ("a", true, false), ("b.txt", false, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);

        model.expand(Path::new("/r"), &fs).unwrap();
        let rows = model.visible_rows();
        // Root row is now absent; children of root start at depth 0.
        assert_eq!(row_names(&rows), vec!["a", "b.txt", "z.txt"]); // dirs first
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].is_dir);
    }

    #[test]
    fn nested_expansion_and_depth() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("a", true, false)]);
        fs.dir("/r/a", vec![("b", true, false)]);
        fs.dir("/r/a/b", vec![("file.txt", false, false)]);

        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        model.expand(Path::new("/r/a"), &fs).unwrap();
        model.expand(Path::new("/r/a/b"), &fs).unwrap();

        let rows = model.visible_rows();
        // Root excluded: depths are 0, 1, 2 instead of 0, 1, 2, 3.
        let depths: Vec<usize> = rows.iter().map(|r| r.depth).collect();
        assert_eq!(depths, vec![0, 1, 2]);
        assert_eq!(row_names(&rows), vec!["a", "b", "file.txt"]);
    }

    #[test]
    fn collapse_hides_children() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("a", true, false)]);
        fs.dir("/r/a", vec![("x", false, false)]);

        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        model.expand(Path::new("/r/a"), &fs).unwrap();
        // Root not shown; "a" and "x" remain.
        assert_eq!(row_names(&model.visible_rows()), vec!["a", "x"]);

        model.collapse(Path::new("/r/a"));
        assert_eq!(row_names(&model.visible_rows()), vec!["a"]);
        assert!(!model.is_expanded(Path::new("/r/a")));
    }

    #[test]
    fn sorting_respects_dirs_first_and_case() {
        let mut fs = MemFs::default();
        fs.dir(
            "/r",
            vec![
                ("beta", false, false),
                ("Alpha", false, false),
                ("GAMMA", true, false),
                ("alpha", false, false),
            ],
        );
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        // Root excluded; dirs first, then case-insensitive alphabetical.
        assert_eq!(row_names(&model.visible_rows()), vec!["GAMMA", "Alpha", "alpha", "beta"]);
    }

    #[test]
    fn dirs_first_disabled_interleaves() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("b", true, false), ("a", false, false)]);
        let mut model = TreeModel::new(
            "/r".into(),
            SortOptions { dirs_first: false, ..SortOptions::default() },
            false,
        );
        model.expand(Path::new("/r"), &fs).unwrap();
        // Root excluded.
        assert_eq!(row_names(&model.visible_rows()), vec!["a", "b"]);
    }

    #[test]
    fn sort_by_size_orders_within_groups_and_descends() {
        let mut fs = MemFs::default();
        fs.dir_sized(
            "/r",
            vec![
                ("small.txt", false, false, 10, 0),
                ("big.txt", false, false, 900, 0),
                ("mid.txt", false, false, 100, 0),
            ],
        );
        let asc = SortOptions { key: SortKey::Size, ..SortOptions::default() };
        let mut model = TreeModel::new("/r".into(), asc, false);
        model.expand(Path::new("/r"), &fs).unwrap();
        assert_eq!(row_names(&model.visible_rows()), vec!["small.txt", "mid.txt", "big.txt"]);

        let desc = SortOptions {
            key: SortKey::Size,
            ascending: false,
            ..SortOptions::default()
        };
        let mut model = TreeModel::new("/r".into(), desc, false);
        model.expand(Path::new("/r"), &fs).unwrap();
        assert_eq!(row_names(&model.visible_rows()), vec!["big.txt", "mid.txt", "small.txt"]);
    }

    #[test]
    fn sort_by_modified_orders_by_time() {
        let mut fs = MemFs::default();
        fs.dir_sized(
            "/r",
            vec![
                ("old.txt", false, false, 1, 100),
                ("new.txt", false, false, 1, 300),
                ("mid.txt", false, false, 1, 200),
            ],
        );
        let opts = SortOptions { key: SortKey::Modified, ..SortOptions::default() };
        let mut model = TreeModel::new("/r".into(), opts, false);
        model.expand(Path::new("/r"), &fs).unwrap();
        assert_eq!(row_names(&model.visible_rows()), vec!["old.txt", "mid.txt", "new.txt"]);
    }

    #[test]
    fn sort_by_type_groups_by_extension() {
        let mut fs = MemFs::default();
        fs.dir(
            "/r",
            vec![
                ("a.txt", false, false),
                ("b.rs", false, false),
                ("c.txt", false, false),
                ("d.md", false, false),
            ],
        );
        let opts = SortOptions { key: SortKey::Type, ..SortOptions::default() };
        let mut model = TreeModel::new("/r".into(), opts, false);
        model.expand(Path::new("/r"), &fs).unwrap();
        // Extension groups: md, rs, txt; alphabetical within txt.
        assert_eq!(row_names(&model.visible_rows()), vec!["d.md", "b.rs", "a.txt", "c.txt"]);
    }

    #[test]
    fn reload_all_applies_hidden_toggle() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![(".hidden", false, false), ("shown", false, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        assert_eq!(row_names(&model.visible_rows()), vec!["shown"]);

        model.set_show_hidden(true);
        model.reload_all(&fs);
        assert_eq!(row_names(&model.visible_rows()), vec![".hidden", "shown"]);
    }

    #[test]
    fn hidden_entries_are_skipped_unless_enabled() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![(".git", true, false), (".env", false, false), ("src", true, false)]);

        let mut hidden = TreeModel::new("/r".into(), SortOptions::default(), false);
        hidden.expand(Path::new("/r"), &fs).unwrap();
        // Root excluded.
        assert_eq!(row_names(&hidden.visible_rows()), vec!["src"]);

        let mut visible = TreeModel::new("/r".into(), SortOptions::default(), true);
        visible.expand(Path::new("/r"), &fs).unwrap();
        // Root excluded; dirs first: .git and src before the file .env.
        assert_eq!(row_names(&visible.visible_rows()), vec![".git", "src", ".env"]);
    }

    #[test]
    fn filter_narrows_visible_rows() {
        let mut fs = MemFs::default();
        fs.dir(
            "/r",
            vec![
                ("main.rs", false, false),
                ("README.md", false, false),
                ("src", true, false),
            ],
        );
        fs.dir("/r/src", vec![("main.rs", false, false), ("test.rs", false, false)]);

        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        model.expand(Path::new("/r/src"), &fs).unwrap();
        model.set_filter("MAIN");
        let rows = model.visible_rows();
        // Root excluded; dirs first, so `src` (kept because a descendant matches)
        // precedes the matching file; then src's own matching child.
        assert_eq!(row_names(&rows), vec!["src", "main.rs", "main.rs"]);
        assert!(rows.iter().all(|r| r.matches));
        model.set_filter("");
        // 5 items: main.rs, README.md, src, src/main.rs, src/test.rs (root not counted)
        assert_eq!(row_names(&model.visible_rows()).len(), 5);
    }

    #[test]
    fn filter_does_not_peek_into_collapsed_dirs() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("src", true, false)]);
        fs.dir("/r/src", vec![("needle.txt", false, false)]);

        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        model.set_filter("needle");
        // src is not visible because its unloaded child is not peeked.
        assert_eq!(row_names(&model.visible_rows()), Vec::<String>::new()); // src not matched, not peeked
    }

    #[test]
    fn created_event_refreshes_loaded_parent() {
        let mut fs = MemFs::default();
        fs.dir("/r", Vec::new());
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        assert!(model.visible_rows().iter().all(|r| r.name != "new.txt"));

        fs.dir("/r", vec![("new.txt", false, false)]);
        model.apply(&Change::Created { path: "/r/new.txt".into() }, &fs);
        assert!(row_names(&model.visible_rows()).contains(&"new.txt".to_owned()));
    }

    #[test]
    fn created_event_inside_no_loaded_parent_is_ignored_cheaply() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("deep", true, false)]);
        fs.dir("/r/deep", vec![("a.txt", false, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        // "deep" never expanded => not loaded, event is dropped.
        fs.dir("/r/deep", vec![("a.txt", false, false), ("b.txt", false, false)]);
        model.apply(&Change::Created { path: "/r/deep/b.txt".into() }, &fs);
        let names = row_names(&model.visible_rows());
        assert!(!names.contains(&"b.txt".to_owned()));
    }

    #[test]
    fn removed_event_drops_subtree_cache() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("a", true, false), ("keep.txt", false, false)]);
        fs.dir("/r/a", vec![("x", false, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        model.expand(Path::new("/r/a"), &fs).unwrap();
        // Root not shown; a, x, keep.txt remain.
        assert_eq!(row_names(&model.visible_rows()), vec!["a", "x", "keep.txt"]);

        fs.dir("/r", vec![("keep.txt", false, false)]);
        model.apply(&Change::Removed { path: "/r/a".into() }, &fs);
        assert_eq!(row_names(&model.visible_rows()), vec!["keep.txt"]);
        assert!(!model.dirs.contains_key(Path::new("/r/a")), "subtree cache must be dropped");
    }

    #[test]
    fn renamed_event_moves_entries() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("old.txt", false, false), ("other", true, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();

        fs.dir("/r", vec![("other", true, false), ("new.txt", false, false)]);
        model.apply(
            &Change::Renamed { from: "/r/old.txt".into(), to: "/r/new.txt".into() },
            &fs,
        );
        let names = row_names(&model.visible_rows());
        assert!(!names.contains(&"old.txt".to_owned()));
        assert!(names.contains(&"new.txt".to_owned()));
    }

    #[test]
    fn rescan_reloads_even_after_repeated_cache() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("a", true, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();

        // Simulate a torn event stream; rescan forces a full reload.
        fs.dir("/r", vec![("a", true, false), ("new", false, false)]);
        model.apply(&Change::Rescan, &fs);
        assert!(row_names(&model.visible_rows()).contains(&"new".to_owned()));
    }

    #[test]
    fn expand_error_does_not_panic_and_drops_cache_state() {
        let fs = MemFs::default(); // /r missing entirely
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        assert!(model.expand(Path::new("/r"), &fs).is_err());
        // Root is not a row; no children to show. Renders to 0 rows.
        assert_eq!(model.visible_rows().len(), 0);
    }

    #[test]
    fn unreadable_expanded_dir_reports_error() {
        let mut fs = MemFs::default();
        fs.dir("/r", vec![("locked", true, false)]);
        let mut model = TreeModel::new("/r".into(), SortOptions::default(), false);
        model.expand(Path::new("/r"), &fs).unwrap();
        assert!(model.expand(Path::new("/r/locked"), &fs).is_err());
    }

    #[test]
    fn is_hidden_rules() {
        assert!(is_hidden(".gitignore"));
        assert!(!is_hidden("."));
        assert!(!is_hidden(".."));
        assert!(!is_hidden("visible"));
    }
}