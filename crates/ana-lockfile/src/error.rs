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
    /// Reading a file that must be readable failed (`ana.lock`, or
    /// opening/creating the environment's advisory lock file).
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },

    /// Writing `ana.lock` (committed or the local `<env_path>/ana.lock`)
    /// failed. Callers swallow this for the env lock's post-install
    /// `{ dirty: false, ... }` write (best-effort; a lost write there only
    /// costs one extra dirty-wipe next run), but propagate it for the
    /// pre-install `dirty = true` write, since without that landing a
    /// crash mid-install is indistinguishable from "never started."
    #[error("failed to write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },

    /// Acquiring the environment's advisory lock failed at the OS level (an
    /// unwritable locks directory, say). Contention is *not* an error --
    /// acquisition blocks, with periodic "still waiting" notices.
    #[error("failed to acquire advisory lock {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },

    /// `ana.lock` exists but is not parseable TOML (a botched merge
    /// conflict resolution, a hand-edit typo, ...). Never silently
    /// regenerated; the user must repair or delete the file explicitly.
    #[error("{path} exists but could not be parsed ({reason}); repair or delete it and re-run")]
    CorruptLock { path: PathBuf, reason: String },

    /// Recursively removing `env_path` -- because the env lock says
    /// `dirty = true`, so a previous reconcile may have left a
    /// half-installed prefix -- failed.
    #[error("failed to remove the environment directory {path}: {source}")]
    DeleteEnv { path: PathBuf, source: io::Error },

    /// A requirement could not be converted to a matchspec for the
    /// target platform (including an unsupported target platform
    /// itself).
    #[error(transparent)]
    Convert(#[from] ana_matchspec_convert::Error),

    /// The solver itself failed (network, unsatisfiable requirements, ...).
    #[error("solve failed for {platform}: {source}")]
    Solve {
        platform: Platform,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// `check` was called with `fix: true` but no solver to fix with.
    #[error("cannot fix stale lock sections without a solver")]
    FixWithoutSolver,

    /// `ensure_current_platform_locked` was called with `frozen: true` and
    /// `platform`'s section was missing or out of date with the project's
    /// declaration -- the whole point of `--frozen` is to fail instead of
    /// writing to `ana.lock`, so no solve is even attempted.
    #[error(
        "ana.lock is out of date for {platform} and --frozen was given \
         (run without --frozen to update the lock, or run `ana lock` first)"
    )]
    Frozen { platform: Platform },

    /// A channel/url override (a project's own `conda-channels`/
    /// `# ana-channels:`, or an individual package's `channel::`/url
    /// override) failed `ana_channels`' own validation: a channel not
    /// present in `default_channels ∪ allowed_channels`
    /// ([`ana_channels::Error::ChannelNotAllowed`]), an entry that
    /// didn't parse as a channel name or URL
    /// ([`ana_channels::Error::InvalidChannel`]), or one that resolved
    /// to an unsupported local filesystem path
    /// ([`ana_channels::Error::LocalChannelNotSupported`]).
    #[error(transparent)]
    Channels(#[from] ana_channels::Error),
}
