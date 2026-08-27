//! The `ana` command-line interface.
//!
//! A scaffold with exactly one real command: `ana run [--group <name>]...
//! <command>...`. The command resolves its environment's paths via
//! `ana-paths` (`investigations/env_storage.md`'s discovery procedure),
//! brings the environment's `ana.lock` up to date via `ana-lockfile`'s
//! default mode
//! ([`run_command`]), and prints the command that *would* have been run
//! inside the environment -- environment creation and activation are
//! future work (`investigations/sync_algorithm.md`), as is the real solver
//! behind the [`Solver`] seam, so [`NoSolver`] stands in with an explicit
//! error for the moment a regeneration is actually needed.
//!
//! The library/binary split exists so the whole flow is testable with a
//! fake [`Solver`]; `main.rs` is a thin shell over [`cli::parse`] and
//! [`run_command`]. Everything that knows *where files live* is in
//! `ana-paths`, not here -- the CLI only composes.
//!
//! Command lines and `pyproject.toml` are untrusted input, so this crate
//! never `unwrap`/`expect`s its way past a failure outside of tests --
//! same lint-enforced rule as the rest of the workspace.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod cli;
mod run;

pub use ana_lockfile::EnsureOutcome;
pub use run::{run_command, shell_join, NoSolver, RunOutcome};

/// Every way a CLI invocation can fail after its arguments have parsed
/// (parse failures are clap's own errors, which print usage and exit 2 on
/// their own -- see [`cli`]).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No `pyproject.toml` in the working directory. There is no walk-up
    /// discovery: `ana` must be run from the project root (see
    /// `env_storage.md`'s amendment history).
    #[error("could not find pyproject.toml in the current directory (ana must be run from the project root)")]
    NoProjectRoot,

    /// The lockfile algorithm itself failed.
    #[error(transparent)]
    Lockfile(#[from] ana_lockfile::Error),
}
