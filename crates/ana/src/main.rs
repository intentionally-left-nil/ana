//! The `ana` binary: a thin shell over `ana::cli` and each command's
//! entry point (`ana::run_command`/`ana::exec`, `ana::sync_command`,
//! `ana::clean_command`). clap owns help text, parse errors, and their
//! exit codes; runtime failures print the error and exit 1.
//!
//! Builds the process-wide shared state exactly once here (not inside
//! `ana-solver` or `ana-installer`, since `ana-installer`'s downloads and
//! `ana-solver`'s repodata fetches need to share one retry policy): the
//! cache root (`rattler_cache::default_cache_dir()`, honoring
//! `$RATTLER_CACHE_DIR`), the `Downloader` (client + package/wheel
//! caches, rooted under that one shared location), and the solver (whose
//! `Gateway` gets the *same* client and whose repodata cache lives under
//! the same shared root's `repodata/` subdirectory).
//!

use std::path::Path;
use std::process::ExitCode;

use ana::cli::{self, Command};
use ana::{clean_command, exec, run_command, sync_command, EnsureOutcome, SyncOptions};
use ana_installer::Downloader;
use ana_lockfile::{PlatformStatus, SolveScope};
use ana_solver::RattlerSolver;
use rattler_conda_types::Platform;
use uv_normalize::GroupName;

struct Engine {
    runtime: tokio::runtime::Runtime,
    downloader: Downloader,
    solver: RattlerSolver,
}

impl Engine {
    fn build(cwd: &Path) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("could not start the async runtime: {err}"))?;

        let cache_root = rattler_cache::default_cache_dir()
            .map_err(|err| format!("could not determine the cache directory: {err}"))?;

        let downloader = Downloader::new(&cache_root)
            .map_err(|err| format!("could not prepare the download cache: {err}"))?;

        let repodata_cache_dir = cache_root.join(rattler_cache::REPODATA_CACHE_DIR);
        let solver = RattlerSolver::new(
            repodata_cache_dir,
            cwd.to_path_buf(),
            runtime.handle().clone(),
            downloader.client().clone(),
        );

        Ok(Self {
            runtime,
            downloader,
            solver,
        })
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(command) => command,
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

    match command {
        Command::Run {
            group,
            quiet,
            frozen,
            command,
        } => main_run(&cwd, group, quiet, frozen, command),
        Command::Sync {
            group,
            clean,
            frozen,
            subdir,
        } => main_sync(&cwd, group, clean, frozen, subdir),
        Command::Clean => main_clean(&cwd),
        Command::Config { action } => main_config(action),
    }
}

fn main_run(
    cwd: &Path,
    groups: Vec<GroupName>,
    quiet: bool,
    frozen: bool,
    command: Vec<String>,
) -> ExitCode {
    let config = match ana::config::resolve_config() {
        Ok(config) => config,
        Err(err) => {
            if !quiet {
                eprintln!("ana: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    let engine = match Engine::build(cwd) {
        Ok(engine) => engine,
        Err(message) => {
            if !quiet {
                eprintln!("ana: {message}");
            }
            return ExitCode::FAILURE;
        }
    };

    let outcome = match run_command(
        cwd,
        &SolveScope {
            groups: &groups,
            channels: &config.default_channels,
        },
        &command,
        frozen,
        &engine.solver,
        engine.runtime.handle(),
        &engine.downloader,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            if !quiet {
                eprintln!("ana: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    if !quiet {
        report_ensure(outcome.ensure);
        report_install(outcome.install.is_some());
    }

    // Logged *before* exec, since exec never returns at all on success
    // (Unix) or only returns here via `std::process::exit` (Windows) --
    // anything after this point only runs on the failure path.
    let err = exec(&outcome);
    if !quiet {
        eprintln!("ana: {err}");
    }
    ExitCode::FAILURE
}

fn main_sync(
    cwd: &Path,
    groups: Vec<GroupName>,
    clean: bool,
    frozen: bool,
    subdirs: Vec<Platform>,
) -> ExitCode {
    let config = match ana::config::resolve_config() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    let engine = match Engine::build(cwd) {
        Ok(engine) => engine,
        Err(message) => {
            eprintln!("ana: {message}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = match sync_command(
        cwd,
        &SyncOptions {
            clean,
            frozen,
            subdirs: &subdirs,
        },
        &SolveScope {
            groups: &groups,
            channels: &config.default_channels,
        },
        &engine.solver,
        engine.runtime.handle(),
        &engine.downloader,
    ) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    report_ensure(outcome.ensure);
    report_install(outcome.install.is_some());
    if let Some(report) = &outcome.subdirs {
        for (platform, status) in &report.platforms {
            match status {
                PlatformStatus::Valid => eprintln!("ana: {platform} is up to date"),
                PlatformStatus::Stale => {
                    eprintln!("ana: {platform} was stale and has been re-solved")
                }
            }
        }
    }
    ExitCode::SUCCESS
}

fn main_clean(cwd: &Path) -> ExitCode {
    let outcome = match clean_command(cwd) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    if outcome.removed.is_empty() {
        eprintln!("ana: nothing to clean");
    } else {
        for env in &outcome.removed {
            eprintln!("ana: removed {}", env.path.display());
        }
    }
    ExitCode::SUCCESS
}

fn main_config(action: cli::ConfigAction) -> ExitCode {
    let result = match action {
        cli::ConfigAction::Get { key } => ana::config::config_get(key).map(|text| {
            println!("{text}");
        }),
        cli::ConfigAction::Set { key, values } => ana::config::config_set(key, &values),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ana: {err}");
            ExitCode::FAILURE
        }
    }
}

fn report_ensure(ensure: EnsureOutcome) {
    match ensure {
        EnsureOutcome::Fresh => eprintln!("ana: lockfile is up to date"),
        EnsureOutcome::Resolved => eprintln!("ana: regenerated the lockfile"),
    }
}

fn report_install(installed: bool) {
    if installed {
        eprintln!("ana: environment installed");
    } else {
        eprintln!("ana: environment is up to date");
    }
}
