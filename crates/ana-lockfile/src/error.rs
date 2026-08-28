//! The crate's error type. Everything that can fail across the algorithm
//! funnels into [`Error`]; the two deliberate design constraints are (a) a
//! missing/corrupt *env lock* (`<env_path>/ana.lock`) is *never* an error
//! -- it is local, gitignored state, and any doubt about its content is
//! handled by treating it as absent (see `crate::env_lock`), and (b) a
//! *missing* committed `ana.lock` is not an error either -- it is a
//! regeneration trigger. A *corrupt* committed `ana.lock`, by contrast, is
//! [`Error::CorruptLock`]: the file is committed and shared, so silently
//! regenerating it would destroy every other platform's section.

use std::io;
use std::path::PathBuf;

use rattler_conda_types::Platform;

/// Every way the lock-generation algorithm can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading a file that must be readable failed (`pyproject.toml`, or
    /// opening/creating the environment's advisory lock file).
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },

    /// Writing `ana.lock` (committed or the local `<env_path>/ana.lock`)
    /// failed. The env lock's post-install `{ dirty: false, ... }` write is
    /// deliberately not represented here in practice: callers swallow that
    /// particular failure (best-effort), since a lost write there only
    /// ever costs one extra dirty-wipe on the next invocation. The
    /// pre-install `dirty = true` write, by contrast, is expected to
    /// propagate this variant -- without it landing, a crash during the
    /// install that follows is indistinguishable from "never started."
    #[error("failed to write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },

    /// Acquiring the environment's advisory lock failed at the OS level (an
    /// unwritable locks directory, say). Contention is *not* an error --
    /// acquisition blocks, with periodic "still waiting" notices.
    #[error("failed to acquire advisory lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },

    /// `ana.lock` exists but is not parseable TOML (a botched merge
    /// conflict resolution, a hand-edit typo, ...). Never silently
    /// regenerated: wholesale replacement would destroy every other
    /// platform's committed section, so the user must repair or delete
    /// the file explicitly.
    #[error("{path} exists but could not be parsed ({reason}); repair or delete it and re-run")]
    CorruptLock { path: PathBuf, reason: String },

    /// Recursively removing `env_path` -- because the env lock says
    /// `dirty = true`, so a previous reconcile may have left a
    /// half-installed prefix -- failed.
    #[error("failed to remove the environment directory {path}: {source}")]
    DeleteEnv { path: PathBuf, source: io::Error },

    /// `pyproject.toml` failed `ana_pyproject`'s own validation.
    #[error("{0}")]
    Pyproject(#[from] ana_pyproject::PyprojectError),

    /// A `--group` name that doesn't exist in `pyproject.toml`'s
    /// `[dependency-groups]`.
    #[error("dependency group `{0}` is not defined in pyproject.toml")]
    UnknownGroup(String),

    /// The target platform has no marker-environment mapping -- only the
    /// six installable subdirs are supported.
    #[error("{0}")]
    UnsupportedPlatform(#[from] ana_marker_matchspec::UnsupportedPlatform),

    /// One or more requirements could not be converted to matchspecs for
    /// the target platform. Every failure is listed, not just the first --
    /// same aggregate-once-shape-is-valid policy as `ana-pyproject`.
    #[error("failed to convert requirements to matchspecs:\n{0}")]
    Conversion(String),

    /// The solver itself failed (network, unsatisfiable requirements, ...).
    /// The inner error is boxed because the real solver crate isn't in the
    /// workspace yet.
    #[error("solve failed for {platform}: {source}")]
    Solve {
        platform: Platform,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// `check` was called with `fix: true` but no solver to fix with.
    #[error("cannot fix stale lock sections without a solver")]
    FixWithoutSolver,
}
