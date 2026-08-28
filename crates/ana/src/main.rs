//! The `ana` binary: a thin shell over `ana::cli`, `ana::run_command`,
//! and `ana::exec`. clap owns help text, parse errors, and their exit
//! codes; runtime failures print the error and exit 1.
//!
//! Builds the process-wide shared state exactly once here (not inside
//! `ana-solver` or `ana-installer` -- both used to be able to build their
//! own runtime/client independently, but now that `ana-installer`'s
//! downloads and `ana-solver`'s repodata fetches need to share one retry
//! policy per
//! `investigations/package_download_and_install.md`'s recommendation 1,
//! one `tokio::runtime::Runtime` and one `ana_installer::Downloader` are
//! built here and passed down): the cache root
//! (`rattler_cache::default_cache_dir()`, honoring `$RATTLER_CACHE_DIR`,
//! per `investigations/package_download_and_install.md`'s "Cache
//! location" decision), the `Downloader` (client + package/wheel caches,
//! rooted under that one shared location), and the solver (whose
//! `Gateway` gets the *same* client and whose repodata cache lives under
//! the same shared root's `repodata/` subdirectory).

use std::process::ExitCode;

use ana::cli::{self, Command};
use ana::{exec, run_command, EnsureOutcome};
use ana_installer::Downloader;
use ana_solver::RattlerSolver;

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

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("ana: could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    // The one shared cache root every rattler-based tool on the machine
    // already uses -- see the module docs' "Cache location" reference.
    let cache_root = match rattler_cache::default_cache_dir() {
        Ok(root) => root,
        Err(err) => {
            eprintln!("ana: could not determine the cache directory: {err}");
            return ExitCode::FAILURE;
        }
    };

    let downloader = match Downloader::new(&cache_root) {
        Ok(downloader) => downloader,
        Err(err) => {
            eprintln!("ana: could not prepare the download cache: {err}");
            return ExitCode::FAILURE;
        }
    };

    // `ana-solver`'s repodata cache nests under the same shared root
    // (`REPODATA_CACHE_DIR`), and its `Gateway` uses the *same* client as
    // `downloader`'s installs -- one client, one retry policy, for both
    // repodata and package-artifact fetches.
    let repodata_cache_dir = cache_root.join(rattler_cache::REPODATA_CACHE_DIR);
    let solver = RattlerSolver::new(
        repodata_cache_dir,
        cwd.clone(),
        runtime.handle().clone(),
        downloader.client().clone(),
    );

    let outcome = match run_command(
        &cwd,
        &groups,
        &command,
        &solver,
        runtime.handle(),
        &downloader,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    match outcome.ensure {
        EnsureOutcome::Fresh => eprintln!("ana: lockfile is up to date"),
        EnsureOutcome::Resolved => eprintln!("ana: regenerated the lockfile"),
    }
    match outcome.install {
        None => eprintln!("ana: environment is up to date"),
        Some(_) => eprintln!("ana: environment installed"),
    }

    // Logged *before* exec, since exec never returns at all on success
    // (Unix) or only returns here via `std::process::exit` (Windows) --
    // anything after this point in `main` only runs on the failure path.
    let err = exec(&outcome);
    eprintln!("ana: {err}");
    ExitCode::FAILURE
}
