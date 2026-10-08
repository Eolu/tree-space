//! Filesystem watching via `notify` (inotify on Linux).
//!
//! [`spawn`] registers a watch on the root directory only — intentionally
//! **non-recursive** so that opening a large tree is instant (a recursive
//! join on the inotify backend walks the whole subtree, which blocks the UI
//! exactly when the user is waiting for the first listing). The tree adds
//! watches for individual directories as they are expanded, so auto-updates
//! still reach every directory the user can actually see.
//!
//! Every raw notify event passes through the pure [`event_to_changes`]
//! translation onto the model's [`Change`] vocabulary.

use std::path::{Path, PathBuf};

use notify::Event;
pub use notify::{Error, RecommendedWatcher, RecursiveMode, Watcher};

use super::model::Change;

/// Spawn a recursive watcher on `root`; every translated change is handed to
/// `on_change` (which the caller typically fires into a component sender).
///
/// The returned [`RecommendedWatcher`] must be kept alive for events to flow.
pub fn spawn(
    root: &Path,
    mut on_change: impl FnMut(Change) + Send + 'static,
) -> Result<RecommendedWatcher, Error> {
    let root = root.to_path_buf();
    let watched_root = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
        // Drop watch errors (e.g. a permission-denied subdir); the tree
        // just won't auto-update for that subtree.
        if let Ok(event) = res {
            for change in event_to_changes(&event, &root) {
                on_change(change);
            }
        }
    })?;
    watcher.watch(&watched_root, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Translate one raw notify event into model changes.
///
/// Only structural events (create/remove/rename) are surfaced plus a coarse
/// [rescan][Change::Rescan] on `need_rescan()`. Metadata and data rewrites are
/// ignored (the tree shows names and directories only). Events outside `root`
/// are dropped.
pub fn event_to_changes(event: &Event, root: &Path) -> Vec<Change> {
    if event.need_rescan() {
        return vec![Change::Rescan];
    }

    use notify::event::{EventKind, ModifyKind, RenameMode};

    let mut changes = Vec::new();
    match &event.kind {
        EventKind::Create(_) => push_change(&mut changes, event.paths.first(), root, |p| {
            Change::Created { path: p }
        }),
        EventKind::Remove(_) => push_change(&mut changes, event.paths.first(), root, |p| {
            Change::Removed { path: p }
        }),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let (Some(from), Some(to)) = (event.paths.first(), event.paths.get(1))
                && under_root(root, from)
            {
                changes.push(Change::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                });
            }
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            push_change(&mut changes, event.paths.first(), root, |p| {
                Change::Created { path: p }
            });
        }
        EventKind::Modify(ModifyKind::Name(mode))
            if !matches!(mode, RenameMode::To | RenameMode::Both) =>
        {
            push_change(&mut changes, event.paths.first(), root, |p| {
                Change::Removed { path: p }
            });
        }
        EventKind::Modify(ModifyKind::Metadata(_)) => {
            push_change(&mut changes, event.paths.first(), root, |p| {
                Change::Modified { path: p }
            });
        }
        // Data rewrites and everything else do not change tree structure.
        _ => {}
    }
    changes
}

fn push_change(
    changes: &mut Vec<Change>,
    path: Option<&PathBuf>,
    root: &Path,
    build: impl Fn(PathBuf) -> Change,
) {
    if let Some(path) = path
        && under_root(root, path)
    {
        changes.push(build(path.to_path_buf()));
    }
}

fn under_root(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind, RenameMode};

    fn ev(kind: EventKind, paths: &[&str]) -> Event {
        let mut event = Event::new(kind);
        for p in paths {
            event = event.add_path(PathBuf::from(p));
        }
        event
    }

    const ROOT: &str = "/home/u/src";

    #[test]
    fn creates_and_removes_map_directly() {
        let e = ev(EventKind::Create(CreateKind::File), &["/home/u/src/a.txt"]);
        assert_eq!(
            event_to_changes(&e, Path::new(ROOT)),
            vec![Change::Created {
                path: "/home/u/src/a.txt".into()
            }]
        );

        let e = ev(EventKind::Remove(RemoveKind::Folder), &["/home/u/src/dir"]);
        assert_eq!(
            event_to_changes(&e, Path::new(ROOT)),
            vec![Change::Removed {
                path: "/home/u/src/dir".into()
            }]
        );
    }

    #[test]
    fn rename_both_preserves_from_and_to() {
        let e = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &["/home/u/src/old.txt", "/home/u/src/new.txt"],
        );
        assert_eq!(
            event_to_changes(&e, Path::new(ROOT)),
            vec![Change::Renamed {
                from: "/home/u/src/old.txt".into(),
                to: "/home/u/src/new.txt".into()
            }]
        );
    }

    #[test]
    fn rename_single_sides_become_removed_and_created() {
        let from_only = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            &["/home/u/src/x"],
        );
        assert_eq!(
            event_to_changes(&from_only, Path::new(ROOT)),
            vec![Change::Removed {
                path: "/home/u/src/x".into()
            }]
        );

        let to_only = ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            &["/home/u/src/y"],
        );
        assert_eq!(
            event_to_changes(&to_only, Path::new(ROOT)),
            vec![Change::Created {
                path: "/home/u/src/y".into()
            }]
        );
    }

    #[test]
    fn metadata_change_becomes_modified() {
        let e = ev(
            EventKind::Modify(ModifyKind::Metadata(notify::event::MetadataKind::WriteTime)),
            &["/home/u/src/a.txt"],
        );
        assert_eq!(
            event_to_changes(&e, Path::new(ROOT)),
            vec![Change::Modified {
                path: "/home/u/src/a.txt".into()
            }]
        );
    }

    #[test]
    fn data_writes_are_ignored() {
        let e = ev(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Content)),
            &["/home/u/src/log.txt"],
        );
        assert_eq!(event_to_changes(&e, Path::new(ROOT)), vec![]);
    }

    #[test]
    fn events_outside_root_are_dropped() {
        let e = ev(
            EventKind::Create(CreateKind::File),
            &["/tmp/elsewhere/f.txt"],
        );
        assert_eq!(event_to_changes(&e, Path::new(ROOT)), vec![]);
    }

    #[test]
    fn root_itself_is_in_scope() {
        let e = ev(EventKind::Create(CreateKind::Folder), &[ROOT]);
        assert_eq!(event_to_changes(&e, Path::new(ROOT)).len(), 1);
    }

    #[test]
    fn events_without_paths_are_silently_ignored() {
        let e = ev(EventKind::Create(CreateKind::File), &[]);
        assert_eq!(event_to_changes(&e, Path::new(ROOT)), vec![]);
    }

    #[test]
    fn generic_kinds_are_ignored() {
        let e = ev(EventKind::Any, &["/home/u/src/a"]);
        assert_eq!(event_to_changes(&e, Path::new(ROOT)), vec![]);
        let e = ev(
            EventKind::Access(notify::event::AccessKind::Open(
                notify::event::AccessMode::Any,
            )),
            &["/home/u/src/a"],
        );
        assert_eq!(event_to_changes(&e, Path::new(ROOT)), vec![]);
    }

    #[test]
    fn rescan_flag_dominates_everything() {
        let e =
            Event::new(EventKind::Create(CreateKind::File)).add_path("/home/u/src/a.txt".into());
        let needs_rescan = Event {
            attrs: {
                let mut a = notify::event::EventAttributes::new();
                a.set_flag(notify::event::Flag::Rescan);
                a
            },
            ..e
        };
        assert_eq!(
            event_to_changes(&needs_rescan, Path::new(ROOT)),
            vec![Change::Rescan]
        );
    }
}
