//! The `ana` binary: a thin shell over `ana::cli` and
//! `ana::run_command`. clap owns help text, parse errors, and their exit
//! codes; runtime failures print the error and exit 1.

use std::process::ExitCode;

use ana::cli::{self, Command};
use ana::{run_command, shell_join, EnsureOutcome};
use ana_solver::RattlerSolver;

/// The repodata cache directory [`RattlerSolver`] fetches channel repodata
/// into, nested under the same per-OS cache root
/// [`ana_pypi_conda_map::cache_dir`] already resolves for its own,
/// unrelated cache file -- one shared root, one subdirectory per
/// consumer, rather than this crate re-deriving its own `ProjectDirs`
/// triple (and risking the two silently drifting apart). Falls back to
/// `.ana-cache` in the current directory on a platform where that can't
/// determine a cache root at all (rather than failing the whole
/// invocation over a cache location).
fn repodata_cache_dir() -> std::path::PathBuf {
    ana_pypi_conda_map::cache_dir()
        .map(|dir| dir.join("repodata"))
        .unwrap_or_else(|| std::path::PathBuf::from(".ana-cache/repodata"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (groups, command) = match cli::parse(&args) {
        Ok(Command::Run { group, command }) => (group, command),
        // Prints help or the parse error (to stdout/stderr respectively)
        // and exits with clap's code for it. Never returns.
        Err(err) => err.exit(),
    };

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            eprintln!("ana: could not determine the current directory: {err}");
            return ExitCode::FAILURE;
        }
    };

    let solver = match RattlerSolver::new(repodata_cache_dir(), cwd.clone()) {
        Ok(solver) => solver,
        Err(err) => {
            eprintln!("ana: could not start the solver: {err}");
            return ExitCode::FAILURE;
        }
    };

    match run_command(&cwd, &groups, &command, &solver) {
        Ok(outcome) => {
            match outcome.ensure {
                EnsureOutcome::Fresh | EnsureOutcome::CacheRefreshed => {
                    eprintln!("ana: lockfile is up to date")
                }
                EnsureOutcome::Resolved => eprintln!("ana: regenerated the lockfile"),
            }
            println!("would run: {}", shell_join(&outcome.command));
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("ana: {err}");
            ExitCode::FAILURE
        }
    }
}
