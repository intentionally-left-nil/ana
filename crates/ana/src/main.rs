//! The `ana` binary: a thin shell over `ana::cli` and
//! `ana::run_command`. clap owns help text, parse errors, and their exit
//! codes; runtime failures print the error and exit 1.

use std::process::ExitCode;

use ana::cli::{self, Command};
use ana::{run_command, shell_join, EnsureOutcome, NoSolver};

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

    match run_command(&cwd, &groups, &command, &NoSolver) {
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
