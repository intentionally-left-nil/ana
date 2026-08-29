//! The `ana` command-line interface.
//!
//! The library/binary split exists so the whole flow is testable with a
//! fake [`Solver`] and a temp cache/prefix; `main.rs` is a thin shell over
//! [`cli::parse`] and each command's entry point. Everything that knows
//! *where files live* is in `ana-paths`, not here -- the CLI only
//! composes.
//!
//! Command lines and `pyproject.toml` are untrusted input, so this crate
//! never `unwrap`/`expect`s its way past a failure outside of tests --
//! same lint-enforced rule as the rest of the workspace.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod clean;
pub mod cli;
pub mod config;
mod run;
mod sync;

pub use ana_lockfile::EnsureOutcome;
pub use clean::{clean_command, CleanOutcome};
pub use run::{exec, run_command, NoSolver, RunOutcome};
pub use sync::{sync_command, SyncOptions, SyncOutcome};

/// Every way a CLI invocation can fail after its arguments have parsed
/// (parse failures are clap's own errors, which print usage and exit 2 on
/// their own -- see [`cli`]).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The lockfile algorithm itself failed -- including
    /// `ana_lockfile::Error::NoProjectFile`, when neither
    /// `pyproject.toml` nor `requirements.txt` exists in the working
    /// directory (`ana` must be run from the project root; there is no
    /// walk-up discovery for either file).
    #[error(transparent)]
    Lockfile(#[from] ana_lockfile::Error),

    /// Acquiring the environment's advisory lock failed -- an I/O error
    /// on the lock file itself (permissions, filesystem gone), not
    /// contention (contention just blocks, per
    /// [`ana_lockfile::EnvironmentLock::acquire`]'s own docs).
    #[error("could not acquire the environment's advisory lock at {path}")]
    Lock {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `ensure_current_platform_locked` reported success but `ana.lock`
    /// has no section for `platform` afterwards -- a contract violation
    /// on `ana-lockfile`'s part, not a user-facing input error; surfaced
    /// distinctly so it can never be confused with "no lock file at
    /// all," which is handled upstream, before a solve is even
    /// attempted.
    #[error(
        "ana.lock has no section for {platform} even after ensuring it's current -- this is a bug"
    )]
    MissingPlatformSection {
        platform: rattler_conda_types::Platform,
    },

    /// [`ana_installer::reconcile`] failed (download, extraction, hash
    /// mismatch, linking, ...).
    #[error(transparent)]
    Install(#[from] ana_installer::Error),

    /// [`exec`]'s underlying `std::process::Command` could not even be
    /// started (`command[0]` not found, not executable, ...). On Unix
    /// this is the *only* way [`exec`] returns at all -- success never
    /// returns, by construction.
    #[error("could not run `{}`: {source}", run::shell_join(command))]
    Exec {
        command: Vec<String>,
        #[source]
        source: std::io::Error,
    },

    /// Removing an environment's directory failed -- `ana clean`
    /// removing `.env`/`.ana/<hash>`, or `ana sync --clean`'s pre-emptive
    /// wipe of `env_path` before the rest of `sync` runs.
    #[error("failed to remove {path}: {source}")]
    DeleteEnv {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `ana clean` could not list `.ana/` to discover which group
    /// environments exist.
    #[error("failed to read directory {path}: {source}")]
    ReadDir {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    /// `ana-config` failed to read, parse, or validate `config.toml` (or,
    /// in a `commercial-config` build, the compiled-in config).
    #[error(transparent)]
    Config(#[from] ana_config::ConfigError),

    /// `ana config set` was invoked in a `commercial-config` build --
    /// centrally managed configuration is the entire point of that
    /// build, so the disk copy of `config.toml` is never touched.
    #[error("Not available on commercial builds")]
    ConfigSetDisabled,

    /// `ana config set <key> <values...>` was given the wrong number of
    /// values for `key` (`pypi_to_conda_uri` takes exactly one; a channel
    /// list takes any number, but at least one -- `set` can never clear a
    /// key back to unset).
    #[error("`{key}` takes {expected}")]
    ConfigSetArity {
        key: ana_config::Key,
        expected: &'static str,
    },

    /// `ana config set` needs `config.toml`'s path but
    /// `ana_config::config_path` couldn't determine one (no resolvable
    /// home/config directory on this system).
    #[error("could not determine ana's config directory (no home directory?)")]
    NoConfigDir,

    /// A `commercial-config` build's baked-in `pypi_to_conda_uri` failed
    /// to re-parse at runtime -- `build.rs` validated it with
    /// `ana_config::parse_str` before compiling it in, so this signals a
    /// `build.rs` bug, not bad user input.
    #[error("build.rs baked in an invalid `{field}`: {source}")]
    InvalidCompiledConfig {
        field: &'static str,
        source: url::ParseError,
    },
}
