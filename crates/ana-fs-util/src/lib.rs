//! Filesystem primitives shared by ana crates: durable atomic file
//! replacement and advisory lock files.
//!
//! These two mechanisms are designed to be used together for committed or
//! cached state files that concurrent `ana` processes may read and write
//! (`ana.lock`, the PyPI→conda mapping cache, ...):
//!
//! - [`write_atomic`]: tempfile-in-the-same-directory + `rename()`, with
//!   fsyncs on both sides of the rename, so a reader never observes a
//!   partial file and a crash leaves the old *or* the new complete file.
//! - [`AdvisoryLock`]: an `fd-lock` advisory lock (flock/LockFileEx) on a
//!   dedicated lock file -- never on the data file itself, which is
//!   replaced by rename on every write, so a lock on its old inode
//!   wouldn't block a renamer.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use fd_lock::{RwLock, RwLockWriteGuard};

/// Atomically replace `path` with `contents`: write to a tempfile in the
/// same directory, then `rename()` over the target -- `rename` is only
/// atomic within one filesystem. The tempfile is fsynced *before* the
/// rename and the directory *after* it, so a crash leaves either the old
/// or the new complete file, never a partial one.
///
/// An existing target keeps its current permissions (the tempfile is
/// created 0600 on Unix, so without this the target would silently
/// become owner-only on every rewrite); a new target gets 0644.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    match fs::metadata(path) {
        Ok(metadata) => tmp.as_file().set_permissions(metadata.permissions())?,
        Err(_) => set_new_file_permissions(tmp.as_file())?,
    }
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|persist_err| persist_err.error)?;
    sync_dir(dir)?;
    Ok(())
}

/// Permissions for a freshly created target of [`write_atomic`].
#[cfg(unix)]
fn set_new_file_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o644))
}

/// Permissions for a freshly created target of [`write_atomic`].
#[cfg(not(unix))]
fn set_new_file_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

/// fsync a directory after a rename into it, so the new directory entry
/// itself is durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// fsync a directory after a rename into it. Windows has no user-space
/// directory fsync; the rename is already as durable as the OS makes it.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

pub fn remove_dir_all_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// A prepared (opened, not yet acquired) advisory lock on a dedicated lock
/// file. Kept separate from acquisition so the acquired guard can borrow
/// from this value -- `fd_lock`'s guards are scoped to the `RwLock` they
/// came from, so both have to live as locals for the duration of the
/// critical section:
///
/// ```ignore
/// let mut lock = AdvisoryLock::open(&path)?;
/// let _guard = lock.write()?;
/// // ... critical section ...
/// ```
///
/// Acquisition policy is the caller's: [`AdvisoryLock::write`] blocks;
/// [`AdvisoryLock::try_write`] lets a caller build its own retry loop.
pub struct AdvisoryLock {
    path: PathBuf,
    lock: RwLock<fs::File>,
}

impl AdvisoryLock {
    /// Open (creating if necessary) the advisory lock file at `path`,
    /// creating its parent directories as needed. Never truncates: the
    /// file's content is meaningless, it exists purely as an
    /// flock/LockFileEx handle. It must also never be deleted or
    /// replaced -- two processes holding flocks on different inodes of the
    /// same path are *not* mutually excluded.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            lock: RwLock::new(file),
        })
    }

    /// The lock file's path (for diagnostics and error messages).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire the lock exclusively, blocking until any other process
    /// holding it releases.
    pub fn write(&mut self) -> io::Result<RwLockWriteGuard<'_, fs::File>> {
        self.lock.write()
    }

    /// Try to acquire the lock exclusively without blocking;
    /// [`io::ErrorKind::WouldBlock`] means another process holds it.
    pub fn try_write(&mut self) -> io::Result<RwLockWriteGuard<'_, fs::File>> {
        self.lock.try_write()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn write_atomic_creates_parent_dirs_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }

    #[test]
    fn write_atomic_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        write_atomic(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        write_atomic(&path, b"new").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_gives_new_files_default_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");

        write_atomic(&path, b"new").unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn advisory_lock_acquires_when_uncontended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks/test.lock");
        let mut lock = AdvisoryLock::open(&path).unwrap();
        assert_eq!(lock.path(), path);
        let _guard = lock.write().unwrap();
        assert!(path.exists(), "the lock file is created, parents included");
    }

    #[test]
    fn remove_dir_all_if_exists_removes_an_existing_tree() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("env");
        fs::create_dir_all(target.join("nested")).unwrap();
        fs::write(target.join("nested/file"), b"data").unwrap();

        remove_dir_all_if_exists(&target).unwrap();

        assert!(!target.exists());
    }

    #[test]
    fn remove_dir_all_if_exists_is_a_noop_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("never-existed");

        remove_dir_all_if_exists(&target).unwrap();

        assert!(!target.exists());
    }
}
