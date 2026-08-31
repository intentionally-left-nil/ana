//! The `ana` command-line interface.
//!
//! The library/binary split lets the whole flow be tested with a fake
//! [`Solver`] and a temp cache/prefix. `main.rs` is a thin shell over
//! [`cli::parse`] and each command's entry point; file-location logic
//! lives in `ana-paths`, not here.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod clean;
pub mod cli;
pub mod config;
mod run;
pub mod script;
mod sync;

pub use ana_lockfile::EnsureOutcome;
pub use clean::{clean_command, clean_global_command, CleanOutcome};
pub use run::{exec, run_command, NoSolver, RunOutcome};
pub use script::detect_script;
pub use sync::{sync_command, SyncOptions, SyncOutcome};

/// Every way a CLI invocation can fail after its arguments have parsed
/// (parse failures are clap's own errors -- see [`cli`]).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Resolving the invocation to an environment failed: no project file,
    /// an unknown `--group`, or a malformed `pyproject.toml`/
    /// `requirements.txt`. `ana` must be run from the project root; there
    /// is no walk-up discovery.
    #[error(transparent)]
    Environment(#[from] ana_environment::Error),

    /// The lockfile algorithm itself failed.
    #[error(transparent)]
    Lockfile(#[from] ana_lockfile::Error),

    /// Acquiring the environment's advisory lock failed with an I/O
    /// error, not contention (contention just blocks, per
    /// [`ana_lockfile::EnvironmentLock::acquire`]'s own docs).
    #[error("could not acquire the environment's advisory lock at {path}")]
    Lock {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// `ensure_current_platform_locked` reported success but `ana.lock`
    /// has no section for `platform` afterwards -- a contract violation
    /// on `ana-lockfile`'s part.
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
    /// started. On Unix this is the *only* way [`exec`] returns at all --
    /// success never returns, by construction.
    #[error("could not run `{}`: {source}", run::shell_join(command))]
    Exec {
        command: Vec<String>,
        #[source]
        source: std::io::Error,
    },

    /// Removing an environment's directory failed.
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

    /// `ana config set` was invoked in a `commercial-config` build, where
    /// the disk copy of `config.toml` is never touched.
    #[error("Not available on commercial builds")]
    ConfigSetDisabled,

    /// `ana config set <key> <values...>` was given the wrong number of
    /// values for `key`.
    #[error("`{key}` takes {expected}")]
    ConfigSetArity {
        key: ana_config::Key,
        expected: &'static str,
    },

    /// `ana config set` needs `config.toml`'s path but
    /// `ana_config::config_path` couldn't determine one.
    #[error("could not determine ana's config directory (no home directory?)")]
    NoConfigDir,

    /// A `commercial-config` build's baked-in `pypi_to_conda_uri` failed
    /// to re-parse at runtime -- `build.rs` validates it before compiling
    /// it in, so this signals a `build.rs` bug, not bad user input.
    #[error("build.rs baked in an invalid `{field}`: {source}")]
    InvalidCompiledConfig {
        field: &'static str,
        source: url::ParseError,
    },
}
