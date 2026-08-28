//! The `ana` command-line interface.
//!
//! A scaffold with exactly one real command: `ana run [--group <name>]...
//! <command>...`. The command resolves its environment's paths via
//! `ana-paths`, brings the environment's `ana.lock` up to date via
//! `ana-lockfile`'s default mode, materializes the environment for real --
//! via `ana-installer`'s `reconcile`, but only when the target package set
//! actually differs from what the env lock says is already installed --
//! and then actually runs the command inside it ([`run_command`] returns
//! the exec plan; [`exec`] is what replaces this process image with it).
//! The real solver behind the [`Solver`] seam is `ana-solver`'s
//! `RattlerSolver` (wired in by `main.rs`); [`NoSolver`] remains as a
//! solver-free stand-in for tests and for any caller that only cares
//! about the offline paths (a fresh lock section never consults the
//! solver at all).
//!
//! The library/binary split exists so the whole flow is testable with a
//! fake [`Solver`] and a temp cache/prefix; `main.rs` is a thin shell over
//! [`cli::parse`], [`run_command`], and [`exec`]. Everything that knows
//! *where files live* is in `ana-paths`, not here -- the CLI only
//! composes.
//!
//! Command lines and `pyproject.toml` are untrusted input, so this crate
//! never `unwrap`/`expect`s its way past a failure outside of tests --
//! same lint-enforced rule as the rest of the workspace.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod cli;
mod run;

pub use ana_lockfile::EnsureOutcome;
pub use run::{exec, run_command, NoSolver, RunOutcome};

/// Every way a CLI invocation can fail after its arguments have parsed
/// (parse failures are clap's own errors, which print usage and exit 2 on
/// their own -- see [`cli`]).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No `pyproject.toml` in the working directory. There is no walk-up
    /// discovery: `ana` must be run from the project root.
    #[error("could not find pyproject.toml in the current directory (ana must be run from the project root)")]
    NoProjectRoot,

    /// The lockfile algorithm itself failed.
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
}
