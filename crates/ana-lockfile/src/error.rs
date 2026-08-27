//! The crate's error type. Everything that can fail in the three
//! algorithm modes funnels into [`Error`]; the two deliberate design
//! constraints are (a) a missing/corrupt cache file is *never* an error
//! (it is a stage-1 miss, handled in `cache.rs` by returning `None`), and
//! (b) a *missing* `ana.lock` is not an error either -- it is a
//! regeneration trigger. A *corrupt* `ana.lock`, by contrast, is
//! [`Error::CorruptLock`]: the file is committed and shared, so silently
//! regenerating it would destroy every other platform's section.

use std::io;
use std::path::PathBuf;

use rattler_conda_types::Platform;

/// Every way the lock-generation algorithm can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading a file that must be readable failed (`pyproject.toml`, or
    /// opening/creating the bucket's advisory lock file).
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },

    /// Writing `ana.lock` itself failed. Cache-file writes are deliberately
    /// *not* represented here: they are swallowed (best-effort, same as
    /// `ana-pypi-conda-map`'s cache persistence), since a lost cache write
    /// only ever costs a stage-1 miss on the next invocation.
    #[error("failed to write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },

    /// Acquiring the bucket's advisory lock failed at the OS level (an
    /// unwritable bucket directory, say). Contention is *not* an error --
    /// acquisition blocks, with periodic "still waiting" notices.
    #[error("failed to acquire bucket lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },

    /// `ana.lock` exists but is not parseable TOML (a botched merge
    /// conflict resolution, a hand-edit typo, ...). Never silently
    /// regenerated: wholesale replacement would destroy every other
    /// platform's committed section, so the user must repair or delete
    /// the file explicitly.
    #[error("{path} exists but could not be parsed ({reason}); repair or delete it and re-run")]
    CorruptLock { path: PathBuf, reason: String },

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
    /// workspace yet -- see the investigation's open TODOs.
    #[error("solve failed for {platform}: {source}")]
    Solve {
        platform: Platform,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// `check` was called with `fix: true` but no solver to fix with.
    #[error("cannot fix stale lock sections without a solver")]
    FixWithoutSolver,
}
