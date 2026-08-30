//! The `ana` binary: a thin shell over `ana::cli` and each command's
//! entry point. clap owns help text, parse errors, and their exit
//! codes; runtime failures print the error and exit 1.
//!
//! Builds the process-wide shared state exactly once here: the cache
//! root, the `Downloader`, the solver, and the pypi-to-conda mapping all
//! share one HTTP client and retry policy.

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
    /// The loaded pypi-to-conda mapping. Loading this is a hard failure
    /// for the whole command: a PEP 508 requirement can't be converted
    /// to a matchspec without it, so there is no identity-mapping
    /// fallback.
    mapping: ana_pypi_conda_map::MappingHandle,
}

impl Engine {
    fn build(
        cwd: &Path,
        pypi_to_conda_uri: &url::Url,
        mapping_options: ana_pypi_conda_map::LoadOptions,
        on_blocking_mapping_refresh: impl FnOnce(),
    ) -> Result<Self, String> {
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

        let mapping = ana_pypi_conda_map::load(
            runtime.handle(),
            downloader.client(),
            pypi_to_conda_uri.as_str(),
            mapping_options,
            on_blocking_mapping_refresh,
        )
        .map_err(|err| format!("could not load the pypi-to-conda name mapping: {err}"))?;

        Ok(Self {
            runtime,
            downloader,
            solver,
            mapping,
        })
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(command) => command,
        // Never returns: prints help/error and exits with clap's code.
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
            allow_stale_mapping,
            command,
        } => main_run(&cwd, group, quiet, frozen, allow_stale_mapping, command),
        Command::Sync {
            group,
            clean,
            frozen,
            allow_stale_mapping,
            subdir,
        } => main_sync(&cwd, group, clean, frozen, allow_stale_mapping, subdir),
        Command::Clean => main_clean(&cwd),
        Command::Config { action } => main_config(action),
    }
}

fn main_run(
    cwd: &Path,
    groups: Vec<GroupName>,
    quiet: bool,
    frozen: bool,
    allow_stale_mapping: bool,
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

    let engine = match Engine::build(
        cwd,
        &config.pypi_to_conda_uri,
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping,
            force_refresh: false,
        },
        || {
            if !quiet {
                eprintln!("ana: downloading conda name translations...");
            }
        },
    ) {
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
            default_channels: &config.default_channels,
            allowed_channels: config.allowed_channels.as_deref().unwrap_or(&[]),
            pypi_to_conda_map: &engine.mapping,
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

    // `engine` is intentionally dropped here without calling
    // `MappingHandle::finish`: joining a background refresh would block
    // the fast path it exists to keep fast, and `exec` never returns on
    // success (Unix) -- skipping `finish()` is always safe (see
    // `MappingHandle::finish`'s own docs).
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
    allow_stale_mapping: bool,
    subdirs: Vec<Platform>,
) -> ExitCode {
    let config = match ana::config::resolve_config() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    let engine = match Engine::build(
        cwd,
        &config.pypi_to_conda_uri,
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping,
            force_refresh: false,
        },
        || eprintln!("ana: downloading conda name translations..."),
    ) {
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
            default_channels: &config.default_channels,
            allowed_channels: config.allowed_channels.as_deref().unwrap_or(&[]),
            pypi_to_conda_map: &engine.mapping,
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

    // `ana sync` always returns normally (no `exec`), so it's worth
    // waiting for an in-flight background refresh here -- otherwise it
    // would be killed, unfinished, the moment this process exits. The
    // outcome is discarded: a failed opportunistic refresh isn't a
    // reason to fail an otherwise-successful `ana sync`.
    let _ = engine.mapping.finish();
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
