//! Filesystem primitives shared by the algorithm: atomic file replacement
//! and the per-bucket advisory lock.
//!
//! Both are the same mechanisms `ana-pypi-conda-map` already uses for its
//! cache (`investigations/lock_generation_algorithm.md`'s "Concurrency and
//! atomicity" section points at that crate as the established pattern):
//! tempfile-in-the-same-directory + `rename()` for writes, and an `fd-lock`
//! advisory lock on a dedicated `.lock` file -- never on the data file
//! itself, which is replaced by rename on every write, so a lock on its old
//! inode wouldn't block a renamer.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fd_lock::RwLock;

/// Name of the advisory lock file, inside the bucket directory (the
/// directory containing `ana.lock`).
const LOCK_FILE_NAME: &str = ".lock";

/// How long lock acquisition waits before the first "still waiting"
/// notice, and the interval between subsequent notices. Ported from pixi's
/// equivalent behavior (`sync_algorithm.md`'s concurrency section), not
/// tuned.
const WAIT_NOTICE_AFTER: Duration = Duration::from_secs(5);
const WAIT_NOTICE_INTERVAL: Duration = Duration::from_secs(10);

/// How long to sleep between `try_write` attempts while another process
/// holds the lock. Short enough to notice a release promptly, long enough
/// to not spin.
const WAIT_POLL: Duration = Duration::from_millis(200);

/// Atomically replace `path` with `contents`: write to a tempfile in the
/// same directory, then `rename()` over the target. Same-directory matters
/// -- `rename` is only atomic within one filesystem. A crash between the
/// two steps leaves either the old complete file or no file, never a
/// partial one.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "target path has no parent directory",
        )
    })?;
    fs::create_dir_all(dir)?;

    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents)?;
    tmp.persist(path).map_err(|persist_err| persist_err.error)?;
    Ok(())
}

/// The path of the advisory lock file for the bucket rooted at
/// `bucket_dir`.
pub(crate) fn lock_file_path(bucket_dir: &Path) -> PathBuf {
    bucket_dir.join(LOCK_FILE_NAME)
}

/// A prepared (opened, not yet acquired) advisory lock on a bucket. Kept
/// separate from acquisition so the acquired guard can borrow from this
/// value -- `fd_lock`'s guards are scoped to the `RwLock` they came from,
/// so both have to live as locals for the duration of the critical
/// section:
///
/// ```ignore
/// let mut lock = BucketLock::open(dir)?;
/// let _guard = lock.acquire()?;
/// // ... critical section ...
/// ```
pub(crate) struct BucketLock {
    path: PathBuf,
    lock: RwLock<fs::File>,
}

impl BucketLock {
    /// Open (creating if necessary) the bucket's lock file. Never
    /// truncates: the file's content is meaningless, it exists purely as an
    /// flock/LockFileEx handle.
    pub(crate) fn open(bucket_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(bucket_dir)?;
        let path = lock_file_path(bucket_dir);
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        Ok(Self {
            path,
            lock: RwLock::new(file),
        })
    }

    /// Acquire the lock exclusively, blocking until any other process
    /// holding it releases. Prints a periodic "still waiting" notice to
    /// stderr while blocked (pixi's behavior, ported per the
    /// investigation), so a wedged holder is diagnosable instead of looking
    /// like a hang.
    ///
    /// Structured as poll-with-notices followed by a single blocking
    /// acquire: returning a `try_write` guard out of a retry loop is the
    /// classic case stable borrowck rejects (the guard's lifetime escapes
    /// the loop), so the loop below only ever *detects* acquisition and
    /// immediately releases it; the returned guard comes from the one
    /// `write()` call after it. That final call is the real
    /// synchronization point -- if another process slips into the gap, it
    /// simply blocks until they release -- so the drop-and-reacquire
    /// window is a scheduling detail, not a correctness one.
    pub(crate) fn acquire(&mut self) -> io::Result<fd_lock::RwLockWriteGuard<'_, fs::File>> {
        let started = Instant::now();
        let mut last_notice = started;
        loop {
            match self.lock.try_write() {
                Ok(guard) => {
                    drop(guard);
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    if started.elapsed() >= WAIT_NOTICE_AFTER
                        && now.duration_since(last_notice) >= WAIT_NOTICE_INTERVAL
                    {
                        eprintln!(
                            "ana: still waiting on another process holding {}",
                            self.path.display()
                        );
                        last_notice = now;
                    }
                    std::thread::sleep(WAIT_POLL);
                }
                Err(err) => return Err(err),
            }
        }
        self.lock.write()
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

    #[test]
    fn bucket_lock_acquires_when_uncontended() {
        let dir = tempfile::tempdir().unwrap();
        let mut lock = BucketLock::open(dir.path()).unwrap();
        let _guard = lock.acquire().unwrap();
        assert!(dir.path().join(".lock").exists());
    }
}
