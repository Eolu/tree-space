//! Paths a background worker is permanently deleting right now.
//!
//! Deleting a large tree runs off the main thread so the panel stays
//! responsive. While it runs, that tree is only half there: reading it (copy,
//! duplicate), writing into it (paste, drop, new file) or transforming it
//! (rename, move, trash) would race the worker — and a file dropped into a
//! directory that finishes deleting a moment later would vanish without anyone
//! asking for it.
//!
//! The panel is single-instance and several panes can show overlapping paths, so
//! the registry is process-wide rather than per-pane: every pane refuses
//! operations on a marked path. Marks are owned by a [`DeleteGuard`] and are
//! released when it drops, so a worker that ends early cannot leave a path
//! locked forever.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// How many guards currently hold each deleting path, plus the canonical
/// spelling of each (a row can name one directory through a symlinked parent,
/// and another pane may use the other name).
static DELETING: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Mark `paths` as being deleted until the returned guard is dropped.
///
/// Marks are reference counted, so two guards for the same path do not release
/// each other's claim when one of them drops.
pub fn begin(paths: impl IntoIterator<Item = PathBuf>) -> DeleteGuard {
    let mut unique = HashSet::new();
    let mut deleting = lock();
    let mut marks = Vec::new();
    for path in paths {
        if !unique.insert(path.clone()) {
            continue;
        }
        *deleting.entry(path.clone()).or_insert(0) += 1;
        marks.push(path);
    }
    DeleteGuard { marks }
}

/// Whether `path` is being deleted, or lies inside a path that is.
///
/// Whole components only, so `/a/bc` is not inside `/a/b`.
pub fn is_deleting(path: &Path) -> bool {
    let deleting = lock();
    deleting.keys().any(|marked| path.starts_with(marked))
}

/// Keeps paths marked while it is alive; dropping it releases its claims.
#[derive(Debug)]
pub struct DeleteGuard {
    marks: Vec<PathBuf>,
}

impl Drop for DeleteGuard {
    fn drop(&mut self) {
        if self.marks.is_empty() {
            return;
        }
        let mut deleting = lock();
        for path in &self.marks {
            let Some(count) = deleting.get_mut(path) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                deleting.remove(path);
            }
        }
    }
}

/// A poisoned lock is still consistent here — every update is a single map
/// operation on the registry — so recover instead of propagating the panic.
fn lock() -> MutexGuard<'static, HashMap<PathBuf, usize>> {
    DELETING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that never exists on disk; the registry only compares names.
    fn sample(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tree-space-pending-{name}"))
    }

    #[test]
    fn marks_a_path_and_everything_below_it() {
        let path = sample("dir");
        assert!(!is_deleting(&path));
        let guard = begin([path.clone()]);
        assert!(is_deleting(&path));
        assert!(is_deleting(&path.join("child")));
        assert!(is_deleting(&path.join("child/deeper")));
        // A sibling whose *name* starts the same is not inside it.
        assert!(!is_deleting(&sample("dir2")));
        drop(guard);
        assert!(!is_deleting(&path));
    }

    #[test]
    fn overlapping_guards_do_not_unmark_each_other() {
        let path = sample("shared");
        let first = begin([path.clone()]);
        let second = begin([path.clone()]);
        drop(first);
        assert!(is_deleting(&path), "the second guard still holds it");
        drop(second);
        assert!(!is_deleting(&path));
    }

    #[test]
    fn duplicate_paths_in_one_guard_are_marked_once() {
        let path = sample("twice");
        let guard = begin([path.clone(), path.clone()]);
        assert!(is_deleting(&path));
        drop(guard);
        assert!(!is_deleting(&path));
    }
}
