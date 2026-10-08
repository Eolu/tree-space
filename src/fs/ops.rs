//! File operations: create, rename, copy, move, duplicate and trash.
//!
//! Production uses [`FileOps`] over `std::fs` for everything except trashing,
//! which goes through the [`TrashService`] seam so tests can record calls
//! instead of putting real files in the user's trash. The trash implementation
//! that ships ([`GioTrash`]) delegates to the platform's trash via GIO, so the
//! behaviour matches what GNOME/Nautilus calls "move to trash".

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use relm4::gtk::gio::prelude::FileExt;

/// Error type for file operations. Wraps [`io::Error`] and adds a few
/// higher-level outcomes produced here (name conflicts, cross-device moves).
#[derive(Debug)]
pub enum OpsError {
    Io(io::Error),
    /// The destination name already exists and an overwrite would be required.
    Conflict(PathBuf),
    /// A source path did not exist.
    NotFound(String),
    /// Something was expected to be a directory but wasn't.
    NotADirectory(String),
    /// Trashing failed for another reason.
    Trash(String),
}

impl std::fmt::Display for OpsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpsError::Io(err) => write!(f, "{err}"),
            OpsError::Conflict(path) => write!(f, "'{}' already exists", path.display()),
            OpsError::NotFound(name) => write!(f, "no such file or directory: {name}"),
            OpsError::NotADirectory(name) => write!(f, "not a directory: {name}"),
            OpsError::Trash(reason) => write!(f, "could not move to trash: {reason}"),
        }
    }
}

impl std::error::Error for OpsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            OpsError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for OpsError {
    fn from(err: io::Error) -> Self {
        OpsError::Io(err)
    }
}

/// Linux `EXDEV` errno, used to detect cross-filesystem moves that `rename(2)`
/// rejects. Constant avoids an otherwise unnecessary `libc` dependency.
const EXDEV: i32 = 18;

/// Linux `EINVAL` errno, returned when `RENAME_NOREPLACE` is unsupported.
const EINVAL: i32 = 22;

/// `rename(2)` with `RENAME_NOREPLACE`: atomically rename `from` to `dest`,
/// refusing to overwrite an existing `dest`. Returns `AlreadyExists` when
/// `dest` is taken, and `EINVAL` when the underlying filesystem does not
/// support the flag (callers fall back to a non-atomic check).
#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, dest: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // RENAME_NOREPLACE is a glibc-level syscall wrapper; call it directly so we
    // do not need libc as a dependency.
    const AT_FDCWD: i32 = -100;
    const RENAME_NOREPLACE: u32 = 1;

    let from_c = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let dest_c = CString::new(dest.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // Safety: the two paths are valid NUL-terminated C strings that outlive the
    // call, and the remaining arguments are plain integers.
    let rc = unsafe {
        libc_syscall::renameat2(
            AT_FDCWD,
            from_c.as_ptr(),
            AT_FDCWD,
            dest_c.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Non-Linux fallback: there is no `RENAME_NOREPLACE`, so report `EINVAL` and
/// let the caller use its conservative fallback path.
#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_from: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(EINVAL))
}

/// The minimal `renameat2` declaration, kept in one place so the unsafe FFI is
/// confined (and this module stays free of a `libc` dependency).
mod libc_syscall {
    use std::os::raw::{c_char, c_int};

    pub type Syscall = i64;

    unsafe extern "C" {
        /// `syscall(number, ...)` from `<unistd.h>`. Declared by hand because we
        /// deliberately avoid the `libc` crate.
        fn syscall(number: Syscall, ...) -> c_int;
    }

    /// `renameat2(AT_FDCWD, old, AT_FDCWD, new, RENAME_NOREPLACE)`.
    ///
    /// # Safety
    /// `old` and `new` must be valid NUL-terminated C strings.
    pub unsafe fn renameat2(
        olddirfd: c_int,
        oldpath: *const c_char,
        newdirfd: c_int,
        newpath: *const c_char,
        flags: u32,
    ) -> c_int {
        // __NR_renameat2 is 316 on all Linux architectures we target
        // (x86_64/aarch64). A wrong number would surface as ENOSYS, which the
        // caller treats as "unsupported, use the fallback".
        const SYS_RENAMEAT2: Syscall = 316;
        unsafe { syscall(SYS_RENAMEAT2, olddirfd, oldpath, newdirfd, newpath, flags) }
    }
}

fn is_exdev(err: &io::Error) -> bool {
    err.raw_os_error() == Some(EXDEV)
}

/// Where the destination of an operation lands in the trash.
pub trait TrashService: Send + Sync + std::fmt::Debug {
    fn trash(&self, path: &Path) -> Result<(), OpsError>;
}

/// Moves paths to the system trash via GIO. This is a small, thin wrapper so
/// tests can substitute a recording fake.
#[derive(Debug, Default, Clone, Copy)]
pub struct GioTrash;

impl TrashService for GioTrash {
    fn trash(&self, path: &Path) -> Result<(), OpsError> {
        let file = relm4::gtk::gio::File::for_path(path);
        file.trash(None::<&relm4::gtk::gio::Cancellable>)
            .map_err(|err| OpsError::Trash(format!("{}: {err}", path.display())))
    }
}

/// All file operations of the panel.
#[derive(Debug)]
pub struct FileOps {
    trash: Box<dyn TrashService>,
}

impl FileOps {
    /// FileOps backed by the platform trash (GIO).
    pub fn new() -> Self {
        Self {
            trash: Box::new(GioTrash),
        }
    }

    /// FileOps with an explicit trash backend (used in tests).
    pub fn with_trash(trash: Box<dyn TrashService>) -> Self {
        Self { trash }
    }

    /// Create a new, empty file named `name` inside `dir`.
    ///
    /// Fails with [`OpsError::Conflict`] when the name already exists, so this
    /// is safe against accidental overwrites.
    pub fn create_file(&self, dir: &Path, name: &str) -> Result<PathBuf, OpsError> {
        if name.trim().is_empty() {
            return Err(OpsError::NotFound("empty name".into()));
        }
        let dest = dir.join(name);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&dest)
            .map_err(|err| match err.kind() {
                io::ErrorKind::AlreadyExists => OpsError::Conflict(dest.clone()),
                _ => OpsError::Io(err),
            })?;
        Ok(dest)
    }

    /// Create a new, empty directory named `name` inside `dir`.
    pub fn create_dir(&self, dir: &Path, name: &str) -> Result<PathBuf, OpsError> {
        if name.trim().is_empty() {
            return Err(OpsError::NotFound("empty name".into()));
        }
        let dest = dir.join(name);
        fs::create_dir(&dest).map_err(|err| match err.kind() {
            io::ErrorKind::AlreadyExists => OpsError::Conflict(dest.clone()),
            _ => OpsError::Io(err),
        })?;
        Ok(dest)
    }

    /// Rename `from` to a new name inside its current directory.
    ///
    /// Uses `renameat2(..., RENAME_NOREPLACE)` where available so the
    /// "destination must not exist" check is atomic: the previous
    /// `dest.exists()` pre-check had a TOCTOU window in which a concurrently
    /// created file would be silently overwritten by `rename(2)`. On
    /// filesystems or kernels that reject `RENAME_NOREPLACE` (e.g. some network
    /// filesystems), it falls back to the check-then-rename sequence, which is
    /// still correct in the common single-user case.
    pub fn rename(&self, from: &Path, new_name: &str) -> Result<PathBuf, OpsError> {
        if new_name.trim().is_empty() {
            return Err(OpsError::NotFound("empty name".into()));
        }
        let dest = from.with_file_name(new_name);
        match rename_noreplace(from, &dest) {
            Ok(()) => Ok(dest),
            // The kernel rejected the flags (not supported here): fall back to
            // a conservative existence check plus a plain rename.
            Err(err) if err.raw_os_error() == Some(EINVAL) => {
                if dest.exists() {
                    return Err(OpsError::Conflict(dest.clone()));
                }
                self.move_path(from, &dest)?;
                Ok(dest)
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Err(OpsError::Conflict(dest.clone()))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Err(OpsError::NotFound(from.display().to_string()))
            }
            Err(err) => Err(OpsError::from(err)),
        }
    }

    /// Copy `from` into `dest_dir` using `from`'s file name, resolving name
    /// conflicts by appending ` (n)` before the extension.
    pub fn copy(&self, from: &Path, dest_dir: &Path) -> Result<PathBuf, OpsError> {
        if !from.exists() {
            return Err(OpsError::NotFound(from.display().to_string()));
        }
        let dest = self.unique_dest(dest_dir, from.file_name().unwrap_or_default());
        self.copy_path(from, &dest)?;
        Ok(dest)
    }

    /// Move `from` into `dest_dir` under its current file name (resolving
    /// conflicts the same way as [`Self::copy`]). Falls back to copy+delete
    /// when the move crosses a filesystem boundary.
    pub fn move_(&self, from: &Path, dest_dir: &Path) -> Result<PathBuf, OpsError> {
        if !from.exists() {
            return Err(OpsError::NotFound(from.display().to_string()));
        }
        let dest = self.unique_dest(dest_dir, from.file_name().unwrap_or_default());
        self.move_or_copy(from, &dest)?;
        Ok(dest)
    }

    /// Duplicate `from` in place (used by the Duplicate menu action).
    pub fn duplicate(&self, from: &Path) -> Result<PathBuf, OpsError> {
        let dir = from.parent().unwrap_or_else(|| Path::new("."));
        self.copy(from, dir)
    }

    /// Create a symlink to `from` next to it, named "Link to <name>" (with a
    /// unique suffix on collisions). The link is created with an absolute
    /// target so it keeps working from any directory.
    pub fn create_link(&self, from: &Path) -> Result<PathBuf, OpsError> {
        if !from.exists() {
            return Err(OpsError::NotFound(from.display().to_string()));
        }
        let dir = from.parent().unwrap_or_else(|| Path::new("."));
        let name = from.file_name().unwrap_or_default();
        let link_name = format!("Link to {}", name.to_string_lossy());
        let dest = self.unique_dest(dir, std::ffi::OsStr::new(&link_name));
        std::os::unix::fs::symlink(from, &dest).map_err(OpsError::from)?;
        Ok(dest)
    }

    /// Move each path to the trash. Stops at the first failure.
    pub fn trash(&self, paths: &[PathBuf]) -> Result<(), OpsError> {
        for path in paths {
            self.trash.trash(path)?;
        }
        Ok(())
    }

    /// Permanently delete each path — unlinks files, recursively removes
    /// directories. Bypasses the trash entirely; there is no undo. Stops at
    /// the first failure (paths already removed stay removed).
    pub fn delete_permanently(&self, paths: &[PathBuf]) -> Result<(), OpsError> {
        for path in paths {
            self.remove_path(path)?;
        }
        Ok(())
    }

    // -- internals -----------------------------------------------------------

    /// Move `from` onto `dest`, falling back to copy+remove across devices.
    fn move_or_copy(&self, from: &Path, dest: &Path) -> Result<(), OpsError> {
        self.rename_with_fallback(from, dest, |a, b| std::fs::rename(a, b))
    }

    /// `rename` with the EXDEV fallback separated out so tests can inject a
    /// rename that always fails, without touching a real cross-device setup.
    fn rename_with_fallback(
        &self,
        from: &Path,
        dest: &Path,
        rename: impl for<'a, 'b> Fn(&'a Path, &'b Path) -> io::Result<()>,
    ) -> Result<(), OpsError> {
        match rename(from, dest) {
            Err(err) if is_exdev(&err) => {
                self.copy_path(from, dest)?;
                self.remove_path(from)?;
                Ok(())
            }
            Err(err) => Err(OpsError::from(err)),
            Ok(()) => Ok(()),
        }
    }

    /// `fs::rename`, no cross-device fallback.
    fn move_path(&self, from: &Path, dest: &Path) -> Result<(), OpsError> {
        std::fs::rename(from, dest).map_err(OpsError::from)
    }

    /// Copy a file or directory tree to `dest`. Symlinks are re-created as
    /// symlinks rather than followed.
    fn copy_path(&self, from: &Path, dest: &Path) -> Result<(), OpsError> {
        let meta = fs::symlink_metadata(from)?;
        if meta.is_dir() {
            fs::create_dir_all(dest)?;
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                self.copy_path(&entry.path(), &dest.join(entry.file_name()))?;
            }
        } else if meta.file_type().is_symlink() {
            let target = fs::read_link(from)?;
            std::os::unix::fs::symlink(target, dest).map_err(OpsError::from)?;
        } else {
            fs::copy(from, dest)?;
        }
        Ok(())
    }

    /// Remove a single path (unlink file / remove empty dir). Directories with
    /// children are removed recursively.
    fn remove_path(&self, path: &Path) -> Result<(), OpsError> {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    /// Pick a non-existing file name: `name`, `name (1)`, `name (2)`, ...
    /// The counter is inserted before the final extension when one exists.
    fn unique_dest(&self, dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
        let text = name.to_string_lossy();
        let (stem, ext) = split_extension(&text);
        for n in 1.. {
            let candidate_text = format!("{stem} ({n}){ext}");
            let candidate = dir.join(&candidate_text);
            if !candidate.exists() {
                return candidate;
            }
        }
        unreachable!("the loop above never terminates without a free name")
    }
}

impl Default for FileOps {
    fn default() -> Self {
        Self::new()
    }
}

/// Split `name` into stem and extension (with leading dot). Dotfiles like
/// `.bashrc` have neither extension nor stem split.
fn split_extension(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            (stem.to_owned(), format!(".{ext}"))
        }
        _ => (name.to_owned(), String::new()),
    }
}

// Linux-only tool; symlinks are re-created as symlinks when copying.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records trashed paths instead of touching the real trash.
    #[derive(Debug, Clone, Default)]
    struct FakeTrash(Arc<Mutex<Vec<PathBuf>>>);

    impl TrashService for FakeTrash {
        fn trash(&self, path: &Path) -> Result<(), OpsError> {
            self.0.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }

    fn fops() -> (FileOps, FakeTrash) {
        let trash = FakeTrash::default();
        let ops = FileOps::with_trash(Box::new(trash.clone()));
        (ops, trash)
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn create_file_creates_and_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let p = ops.create_file(dir.path(), "hi.txt").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "");
        assert!(matches!(
            ops.create_file(dir.path(), "hi.txt"),
            Err(OpsError::Conflict(_))
        ));
    }

    #[test]
    fn create_dir_and_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let p = ops.create_dir(dir.path(), "sub").unwrap();
        assert!(p.is_dir());
        assert!(matches!(
            ops.create_dir(dir.path(), "sub"),
            Err(OpsError::Conflict(_))
        ));
    }

    #[test]
    fn create_with_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        assert!(ops.create_file(dir.path(), "  ").is_err());
        assert!(ops.create_dir(dir.path(), "\t").is_err());
    }

    #[test]
    fn rename_changes_name_and_rejects_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let from = write(dir.path(), "a.txt", "data");
        let to = ops.rename(&from, "b.txt").unwrap();
        assert_eq!(to.file_name().unwrap(), "b.txt");
        assert!(!from.exists());
        // Conflict with an existing name.
        write(dir.path(), "c.txt", "x");
        assert!(matches!(
            ops.rename(&from, "c.txt"),
            Err(OpsError::Conflict(_)) | Err(OpsError::NotFound(_))
        ));
    }

    #[test]
    fn rename_dir_works() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let from = ops.create_dir(dir.path(), "folder").unwrap();
        let dest = ops.rename(&from, "folder2").unwrap();
        assert!(dest.is_dir());
        assert!(!from.exists());
    }

    #[test]
    fn copy_file_into_dir_with_unique_names() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let from = write(src.path(), "photo.png", "bytes");

        let first = ops.copy(&from, dst.path()).unwrap();
        assert_eq!(first.file_name().unwrap(), "photo.png");
        assert_eq!(fs::read(&first).unwrap(), b"bytes");

        let second = ops.copy(&from, dst.path()).unwrap();
        assert_eq!(second.file_name().unwrap(), "photo (1).png");
        let third = ops.copy(&from, dst.path()).unwrap();
        assert_eq!(third.file_name().unwrap(), "photo (2).png");
    }

    #[test]
    fn copy_directory_tree_preserves_structure_and_symlinks() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        fs::create_dir(src.path().join("sub")).unwrap();
        write(src.path(), "root.txt", "r");
        write(&src.path().join("sub"), "child.txt", "c");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("root.txt", src.path().join("link")).unwrap();
        }

        let out = ops.copy(src.path(), dst.path()).unwrap();
        assert!(out.join("sub/child.txt").is_file());
        assert_eq!(fs::read_to_string(out.join("sub/child.txt")).unwrap(), "c");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(out.join("link")).unwrap(),
            Path::new("root.txt")
        );
    }

    #[test]
    fn move_resolves_conflict_and_falls_back_on_cross_device() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let ops = FileOps::new();

        // A name that already exists inside `dst` → unique suffix.
        let _occupied = write(dst.path(), "data.bin", "other");
        let from = write(src.path(), "data.bin", "payload");
        let moved = ops.move_(&from, dst.path()).unwrap();
        assert_eq!(moved.file_name().unwrap(), "data (1).bin");
        assert!(!from.exists());

        // A rename that answers EXDEV must fall back to copy + remove.
        let fallback_src = write(src.path(), "x.txt", "pay");
        let fallback_dst = dst.path().join("x.txt");
        ops.rename_with_fallback(&fallback_src, &fallback_dst, |_from, _to| {
            Err(io::Error::from_raw_os_error(EXDEV))
        })
        .unwrap();
        assert!(!fallback_src.exists());
        assert_eq!(fs::read_to_string(&fallback_dst).unwrap(), "pay");
    }

    #[test]
    fn duplicate_file() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let from = write(dir.path(), "note.md", "# hello");
        let dup = ops.duplicate(&from).unwrap();
        assert_eq!(dup.file_name().unwrap(), "note (1).md");
    }

    #[test]
    fn create_link_creates_symlink_next_to_source() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let from = write(dir.path(), "data.txt", "payload");

        let link = ops.create_link(&from).unwrap();
        assert_eq!(link.file_name().unwrap(), "Link to data.txt");
        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "created path is a symlink");
        assert_eq!(fs::read_link(&link).unwrap(), from);

        // Collision → unique name (suffix before the extension, like other ops).
        let second = ops.create_link(&from).unwrap();
        assert_eq!(second.file_name().unwrap(), "Link to data (1).txt");
    }

    #[test]
    fn trash_delegates_to_service() {
        let dir = tempfile::tempdir().unwrap();
        let (ops, trash) = fops();
        let a = write(dir.path(), "a.txt", "1");
        let b = write(dir.path(), "b.txt", "2");
        ops.trash(&[a.clone(), b.clone()]).unwrap();
        let trashed = trash.0.lock().unwrap();
        assert_eq!(*trashed, vec![a, b]);
    }

    #[test]
    fn missing_sources_report_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let ops = FileOps::new();
        let missing = dir.path().join("nope");
        assert!(matches!(
            ops.copy(&missing, dir.path()),
            Err(OpsError::NotFound(_))
        ));
        assert!(matches!(
            ops.move_(&missing, dir.path()),
            Err(OpsError::NotFound(_))
        ));
    }

    #[test]
    fn split_extension_handles_dotfiles() {
        assert_eq!(
            split_extension("photo.png"),
            ("photo".to_owned(), ".png".to_owned())
        );
        assert_eq!(
            split_extension(".bashrc"),
            (".bashrc".to_owned(), String::new())
        );
        assert_eq!(
            split_extension("noext"),
            ("noext".to_owned(), String::new())
        );
        assert_eq!(
            split_extension("a.tar.gz"),
            ("a.tar".to_owned(), ".gz".to_owned())
        );
    }
}
