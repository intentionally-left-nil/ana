//! The per-environment advisory lock: an [`AdvisoryLock`] (shared with
//! `ana-pypi-conda-map` via `ana-fs-util`) plus this crate's acquisition
//! policy -- pixi's periodic "still waiting" notices while blocked
//! (`sync_algorithm.md`'s concurrency section). Atomic file replacement
//! also lives in `ana-fs-util` (`ana_fs_util::write_atomic`); both
//! mechanisms are the ones `investigations/lock_generation_algorithm.md`'s
//! "Concurrency and atomicity" section points at.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use ana_fs_util::AdvisoryLock;

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

/// A prepared (opened, not yet acquired) advisory lock on an environment.
/// Kept separate from acquisition so the acquired guard can borrow from
/// this value -- `fd_lock`'s guards are scoped to the `RwLock` they came
/// from, so both have to live as locals for the duration of the critical
/// section:
///
/// ```ignore
/// let mut lock = EnvironmentLock::open(&path)?;
/// let _guard = lock.acquire()?;
/// // ... critical section ...
/// ```
pub(crate) struct EnvironmentLock {
    inner: AdvisoryLock,
}

impl EnvironmentLock {
    /// Open (creating if necessary) the advisory lock file at `path`. See
    /// [`AdvisoryLock::open`].
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: AdvisoryLock::open(path)?,
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
        self.inner.write()
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
