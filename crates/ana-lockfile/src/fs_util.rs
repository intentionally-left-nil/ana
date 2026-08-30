//! The per-environment advisory lock: an [`AdvisoryLock`] (shared with
//! `ana-pypi-conda-map` via `ana-fs-util`) plus this crate's acquisition
//! policy of periodic "still waiting" notices while blocked. Atomic file
//! replacement also lives in `ana-fs-util` (`ana_fs_util::write_atomic`).
//!
//! [`EnvironmentLock`] and [`EnvironmentLockGuard`] are `pub` so a caller
//! can hold the lock across more than this crate's own entry points
//! without a second, independent lock file. [`crate::acquire_environment_lock`]
//! is the intended entry point; `open`/`acquire` are split so the guard
//! can borrow from a value ([`EnvironmentLock`]) the caller keeps alive
//! for the guard's whole lifetime -- `fd_lock`'s guards are scoped to the
//! `RwLock` they came from, so both have to live as locals for the
//! duration of the critical section:
//!
//! ```ignore
//! let mut lock = ana_lockfile::acquire_environment_lock(&paths)?;
//! let guard = lock.acquire()?;
//! // ... critical section, guard held throughout ...
//! ```

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use ana_fs_util::AdvisoryLock;

/// How long lock acquisition waits before the first "still waiting"
/// notice, and the interval between subsequent notices.
const WAIT_NOTICE_AFTER: Duration = Duration::from_secs(5);
const WAIT_NOTICE_INTERVAL: Duration = Duration::from_secs(10);

/// How long to sleep between `try_write` attempts while another process
/// holds the lock. Short enough to notice a release promptly, long enough
/// to not spin.
const WAIT_POLL: Duration = Duration::from_millis(200);

/// A prepared (opened, not yet acquired) advisory lock on an environment.
/// Kept separate from acquisition so the acquired guard can borrow from
/// this value across the whole critical section.
pub struct EnvironmentLock {
    inner: AdvisoryLock,
}

/// Proof that an [`EnvironmentLock`] is held, for the whole lifetime of
/// the borrow.
pub struct EnvironmentLockGuard<'a>(#[allow(dead_code)] fd_lock::RwLockWriteGuard<'a, fs::File>);

impl EnvironmentLock {
    /// Open (creating if necessary) the advisory lock file at `path`. See
    /// [`AdvisoryLock::open`].
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: AdvisoryLock::open(path)?,
        })
    }

    /// Acquire the lock exclusively, blocking until any other process
    /// holding it releases. Prints a periodic "still waiting" notice to
    /// stderr while blocked.
    ///
    /// The loop below only detects acquisition and immediately releases
    /// it; the returned guard comes from the final blocking `write()`
    /// call, which is the real synchronization point. If another process
    /// slips into the gap, this call simply blocks until it releases.
    pub fn acquire(&mut self) -> io::Result<EnvironmentLockGuard<'_>> {
        // Formatted up front: the notice below can't borrow
        // `self.inner.path()` while `try_write`'s mutable borrow is live.
        let path = self.inner.path().display().to_string();
        let started = Instant::now();
        let mut last_notice = started;
        loop {
            match self.inner.try_write() {
                Ok(guard) => {
                    drop(guard);
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    if started.elapsed() >= WAIT_NOTICE_AFTER
                        && now.duration_since(last_notice) >= WAIT_NOTICE_INTERVAL
                    {
                        eprintln!("ana: still waiting on another process holding {path}");
                        last_notice = now;
                    }
                    std::thread::sleep(WAIT_POLL);
                }
                Err(err) => return Err(err),
            }
        }
        self.inner.write().map(EnvironmentLockGuard)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn environment_lock_acquires_when_uncontended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locks/test.lock");
        let mut lock = EnvironmentLock::open(&path).unwrap();
        let _guard = lock.acquire().unwrap();
        assert!(path.exists(), "the lock file is created, parents included");
    }
}
