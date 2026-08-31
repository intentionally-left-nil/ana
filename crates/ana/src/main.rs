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
use ana::{clean_command, clean_global_command, exec, run_command, sync_command};
use ana::{EnsureOutcome, SyncOptions};
use ana_channels::ChannelPolicy;
use ana_environment::{EnvironmentRequest, RequirementInput};
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

/// Builds the one [`ana_channels::ChannelPolicy`] a whole invocation
/// solves against, from `config`'s `default_channels`/`allowed_channels`.
/// A malformed admin config -- a `file://` channel, a credentialed URL, a
/// misplaced `/*` -- surfaces here, once, attributed to whichever key it
/// came from, before `ana_environment::resolve` (or any network access)
/// ever runs.
fn build_channel_policy(config: &ana::config::ResolvedConfig) -> Result<ChannelPolicy, String> {
    ChannelPolicy::new(
        &config.default_channels,
        config.allowed_channels.as_deref().unwrap_or(&[]),
    )
    .map_err(|err| format!("invalid channel configuration: {err}"))
}

/// Where a CLI-declared (`-g`/`-i`) environment lives, with no project
/// root of its own. `ana_paths::global_cache_root`'s `None` (no
/// resolvable home directory) is surfaced as a real error rather than
/// papered over with the process's temp directory, which `ana clean
/// --global` would never find and the OS can evict mid-use.
fn global_cache_root() -> Result<std::path::PathBuf, String> {
    ana_paths::global_cache_root().ok_or_else(|| {
        "could not determine the cache directory (no resolvable home directory)".to_string()
    })
}

/// What [`startup`] hands back to `exec_in_environment`/`main_sync`:
/// the shared engine and channel policy every command needs, plus a
/// keyring diagnostic to print (if any) once the caller knows whether
/// to gate it behind `quiet`.
struct Startup {
    engine: Engine,
    channel_policy: ChannelPolicy,
    cache_root: std::path::PathBuf,
    /// Set only when `~/.anaconda/keyring` exists but couldn't be read
    /// or parsed -- never for the common case of a simply-missing file.
    /// See `ana_auth::build_middleware`'s own docs.
    keyring_diagnostic: Option<String>,
}

/// Runs `ana::config::resolve_config` and `ana_auth::build_middleware`
/// (discarding its middleware -- only the diagnostic matters here;
/// `Engine::build` builds its own middleware separately) concurrently:
/// both are independent, disk-only reads with no dependency on each
/// other. A panic in either thread (never expected in practice) is
/// surfaced as a plain error string rather than propagated as a panic
/// itself -- this call site has no `unwrap`/`expect` to reach for.
fn config_and_keyring_diagnostic() -> Result<(ana::config::ResolvedConfig, Option<String>), String>
{
    let (config_result, diagnostic_result) = std::thread::scope(|scope| {
        let config_handle = scope.spawn(ana::config::resolve_config);
        let diagnostic_handle = scope.spawn(|| ana_auth::build_middleware().diagnostic);
        (config_handle.join(), diagnostic_handle.join())
    });

    let config = match config_result {
        Ok(Ok(config)) => config,
        Ok(Err(err)) => return Err(err.to_string()),
        Err(_) => return Err("internal error: the config-loading thread panicked".to_string()),
    };
    let keyring_diagnostic = diagnostic_result.unwrap_or_else(|_| {
        Some("internal error: the keyring-loading thread panicked".to_string())
    });

    Ok((config, keyring_diagnostic))
}

/// `exec_in_environment`/`main_sync`'s shared setup sequence: `ana`'s
/// own `config.toml` read and a `~/.anaconda/keyring` diagnostic check are
/// independent, disk-only reads with no dependency on each other, so
/// they run concurrently ([`config_and_keyring_diagnostic`]) rather than
/// one after another purely because of call order. `Engine::build`
/// (which itself calls `ana_auth::build_middleware` again,
/// independently, to build the auth middleware `Downloader::build`
/// actually uses -- a second, cheap read of the same small file, traded
/// here for not having to thread an already-built middleware through
/// `Engine`'s own constructor) and `build_channel_policy`/
/// `global_cache_root` (no I/O) run after, using `config`'s result.
///
/// Deliberately does *not* also fan out the project file
/// (`ana_environment::resolve`): for `-g`/`-i` invocations it needs the
/// pypi-to-conda mapping `Engine::build` produces, so running it before
/// `Engine::build` exists would be a real, not just call-order,
/// dependency. `main_sync` never needs the mapping for its own project-
/// file resolve (no `-i`, never a CLI-declared input), but `main_run`
/// does whenever `-i`/`-g` supplies dependencies -- keeping one shared
/// helper meant picking a single, always-correct ordering rather than
/// branching this call on which command is asking.
fn startup(
    mapping_options: ana_pypi_conda_map::LoadOptions,
    on_blocking_mapping_refresh: impl FnOnce(),
) -> Result<Startup, String> {
    let (config, keyring_diagnostic) = config_and_keyring_diagnostic()?;

    let engine = Engine::build(
        &config.pypi_to_conda_uri,
        mapping_options,
        on_blocking_mapping_refresh,
    )?;
    let channel_policy = build_channel_policy(&config)?;
    let cache_root = global_cache_root()?;

    Ok(Startup {
        engine,
        channel_policy,
        cache_root,
        keyring_diagnostic,
    })
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
            global,
            include,
            quiet,
            frozen,
            allow_stale_mapping,
            primary,
            program,
            args,
        } => main_run(
            &cwd,
            group,
            global,
            include,
            quiet,
            frozen,
            allow_stale_mapping,
            primary,
            program,
            args,
        ),
        Command::Sync {
            group,
            clean,
            frozen,
            allow_stale_mapping,
            subdir,
            dry,
            format,
        } => main_sync(
            &cwd,
            group,
            clean,
            frozen,
            allow_stale_mapping,
            subdir,
            dry,
            format,
        ),
        Command::Clean { global } => main_clean(&cwd, global),
        Command::Login {
            quiet,
            allow_stale_mapping,
            args,
        } => main_login(&cwd, quiet, allow_stale_mapping, args),
        Command::Config { action } => main_config(action),
    }
}

#[allow(clippy::too_many_arguments)]
fn main_run(
    cwd: &Path,
    groups: Vec<GroupName>,
    global: bool,
    include: Vec<String>,
    quiet: bool,
    frozen: bool,
    allow_stale_mapping: bool,
    primary: String,
    program: Option<String>,
    args: Vec<String>,
) -> ExitCode {
    let invocation = match cli::resolve_run_invocation(global, primary, program, include, args) {
        Ok(invocation) => invocation,
        Err(err) => {
            if !quiet {
                eprintln!("ana: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    exec_in_environment(
        cwd,
        &groups,
        global,
        quiet,
        frozen,
        allow_stale_mapping,
        &invocation,
    )
}

/// `ana login`: a fixed `ana run -g anaconda-auth anaconda -- login`
/// invocation. Its ad hoc environment is keyed by `anaconda-auth` alone,
/// the same as any other `ana run -g anaconda-auth ...` -- so once
/// materialized here, it's reused (not re-solved or reinstalled) by
/// every later `ana login`, or any other `-g anaconda-auth` invocation.
fn main_login(cwd: &Path, quiet: bool, allow_stale_mapping: bool, args: Vec<String>) -> ExitCode {
    let mut exec_args = vec!["login".to_string()];
    exec_args.extend(args);

    let invocation = match cli::resolve_run_invocation(
        true,
        "anaconda-auth".to_string(),
        Some("anaconda".to_string()),
        Vec::new(),
        exec_args,
    ) {
        Ok(invocation) => invocation,
        Err(err) => {
            if !quiet {
                eprintln!("ana: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    exec_in_environment(
        cwd,
        &[],
        true,
        quiet,
        false,
        allow_stale_mapping,
        &invocation,
    )
}

/// Materializes whatever environment `invocation` targets -- the
/// project's (default, or `--group`-selected) under `global == false`,
/// a PEP 723 script's own block when `<primary>` names one, or, under
/// `global == true`, an ad hoc one keyed by `invocation.cli_deps`
/// alone -- brings it up to date, and execs `invocation.exec_command`
/// inside it (as `python <script>` for a script).
///
/// The "materialize an environment, then run something inside it as a
/// subshell" pipeline shared by every `ana` command that ends in a
/// subshell exec: builds the engine (mapping, solver, downloader) once,
/// resolves the environment, calls `run_command`, then `exec`s. Callers
/// only ever differ in how they arrive at `invocation` and whether
/// `global` is set -- `ana run` resolves it from its own CLI arguments;
/// `ana login` passes a fixed one. Never returns on success: `exec`
/// replaces this process on Unix, or exits directly on Windows; the
/// return value only ever reports a failure exit code.
fn exec_in_environment(
    cwd: &Path,
    groups: &[GroupName],
    global: bool,
    quiet: bool,
    frozen: bool,
    allow_stale_mapping: bool,
    invocation: &cli::RunInvocation,
) -> ExitCode {
    // Only a non-`-g` `<primary>` can ever be a PEP 723 script: under
    // `-g`, `<primary>` is already a requirement specifier, not a
    // program name. `invocation.exec_command[0]` is exactly the
    // original `<primary>` string in that case -- see
    // `resolve_run_invocation`'s docs.
    let script = if global {
        None
    } else {
        match ana::detect_script(cwd, &invocation.exec_command[0]) {
            Ok(script) => script,
            Err(err) => {
                if !quiet {
                    eprintln!("ana: {err}");
                }
                return ExitCode::FAILURE;
            }
        }
    };

    let Startup {
        engine,
        channel_policy,
        cache_root,
        keyring_diagnostic,
    } = match startup(
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
        Ok(startup) => startup,
        Err(message) => {
            if !quiet {
                eprintln!("ana: {message}");
            }
            return ExitCode::FAILURE;
        }
    };
    if !quiet {
        if let Some(diagnostic) = &keyring_diagnostic {
            eprintln!("ana: {diagnostic}");
        }
    }

    let input = match &script {
        Some((path, requirements)) => RequirementInput::Script { path, requirements },
        None if global => RequirementInput::CommandLine {
            dependencies: &invocation.cli_deps,
        },
        None => RequirementInput::ProjectDir { dir: cwd },
    };
    // Under `-g`, every CLI-declared dependency is already the
    // `CommandLine` input itself; without it (including for a script,
    // whose own declaration is `input` above), they're `extra`, layered
    // on top of the declaration.
    let extra: &[ana_dependency::Dependency] = if global { &[] } else { &invocation.cli_deps };
    let env = match ana_environment::resolve(&EnvironmentRequest {
        input,
        groups,
        extra,
        platform: Platform::current(),
        pypi_to_conda_map: &engine.mapping,
        global_cache_root: &cache_root,
    }) {
        Ok(env) => env,
        Err(err) => {
            if !quiet {
                eprintln!("ana: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    // A script execs `python <script> ARGS...`, its own dependencies
    // (plus `python` itself -- see `ana::detect_script`'s docs) having
    // already become `env`'s declaration above; every other mode execs
    // `invocation.exec_command` verbatim.
    let script_exec_command: Option<Vec<String>> = script.as_ref().map(|_| {
        let mut command = Vec::with_capacity(1 + invocation.exec_command.len());
        command.push("python".to_string());
        command.extend(invocation.exec_command.iter().cloned());
        command
    });
    let exec_command: &[String] = script_exec_command
        .as_deref()
        .unwrap_or(&invocation.exec_command);

    let outcome = match run_command(
        &env,
        &SolveScope {
            channels: &channel_policy,
            pypi_to_conda_map: &engine.mapping,
        },
        exec_command,
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

#[allow(clippy::too_many_arguments)]
fn main_sync(
    cwd: &Path,
    groups: Vec<GroupName>,
    clean: bool,
    frozen: bool,
    allow_stale_mapping: bool,
    subdirs: Vec<Platform>,
    dry: bool,
    format: ana::dry::Format,
) -> ExitCode {
    let Startup {
        engine,
        channel_policy,
        cache_root,
        keyring_diagnostic,
    } = match startup(
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping,
            force_refresh: false,
        },
        || eprintln!("ana: downloading conda name translations..."),
    ) {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("ana: {message}");
            return ExitCode::FAILURE;
        }
    };
    // `main_sync` has no `quiet` flag -- this always prints, consistent
    // with everything else it already reports unconditionally.
    if let Some(diagnostic) = &keyring_diagnostic {
        eprintln!("ana: {diagnostic}");
    }

    let env = match ana_environment::resolve(&EnvironmentRequest {
        input: RequirementInput::ProjectDir { dir: cwd },
        groups: &groups,
        extra: &[],
        platform: Platform::current(),
        pypi_to_conda_map: &engine.mapping,
        global_cache_root: &cache_root,
    }) {
        Ok(env) => env,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    if dry {
        let scope = SolveScope {
            channels: &channel_policy,
            pypi_to_conda_map: &engine.mapping,
        };
        let plan = match ana::dry::plan_sync(&env, &subdirs, &scope, &engine.solver) {
            Ok(plan) => plan,
            Err(err) => {
                eprintln!("ana: {err}");
                return ExitCode::FAILURE;
            }
        };
        let rendered = match ana::dry::render(&plan, &env.paths().lock_path, format) {
            Ok(rendered) => rendered,
            Err(err) => {
                eprintln!("ana: {err}");
                return ExitCode::FAILURE;
            }
        };
        print!("{rendered}");
        let _ = engine.mapping.finish();
        return ExitCode::SUCCESS;
    }

    let outcome = match sync_command(
        &env,
        &SyncOptions {
            clean,
            frozen,
            subdirs: &subdirs,
        },
        &SolveScope {
            channels: &channel_policy,
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

fn main_clean(cwd: &Path, global: bool) -> ExitCode {
    let outcome = if global {
        let cache_root = match global_cache_root() {
            Ok(cache_root) => cache_root,
            Err(message) => {
                eprintln!("ana: {message}");
                return ExitCode::FAILURE;
            }
        };
        clean_global_command(&cache_root)
    } else {
        clean_command(cwd)
    };
    let outcome = match outcome {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A compiled (or disk) config whose `default_channels` are concrete
    /// URLs -- not bare names -- still produces a [`ChannelPolicy`] that
    /// authorizes exactly those channels, the same way a commercial
    /// build's `compiled_config.toml` would.
    #[test]
    fn a_config_with_concrete_url_default_channels_produces_an_authorizing_policy() {
        let config = ana::config::ResolvedConfig {
            default_channels: vec!["https://repo.mycompany.com/conda".to_string()],
            allowed_channels: None,
            dry_solve_channels: None,
            pypi_to_conda_uri: url::Url::parse(ana_config::DEFAULT_PYPI_TO_CONDA_URI).unwrap(),
        };

        let policy = build_channel_policy(&config).unwrap();

        let authorized: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://repo.mycompany.com/conda")
                .unwrap()
                .into();
        assert!(policy.authorizes_channel(&authorized));

        let unauthorized: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://conda.anaconda.org/conda-forge/")
                .unwrap()
                .into();
        assert!(!policy.authorizes_channel(&unauthorized));
    }

    /// `ANA_CONFIG_PATH`/`ANA_KEYRING_PATH` are process-wide state --
    /// serialize this module's tests that touch them so they can't
    /// observe each other's mutations (matches `ana-config`'s own
    /// `path.rs` convention).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The common case for anyone who hasn't run `ana login`/`anaconda
    /// login`: no `~/.anaconda/keyring` at all. `startup`'s fan-out must
    /// still succeed, with no diagnostic to print -- the graceful-
    /// degradation path this plan adds must actually reach this call
    /// site, not just `ana-auth`'s own unit tests.
    #[test]
    fn config_and_keyring_diagnostic_is_silent_for_a_missing_keyring_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let keyring_dir = tempfile::tempdir().unwrap();
        std::env::set_var(
            "ANA_CONFIG_PATH",
            config_dir.path().join("does-not-exist.toml"),
        );
        std::env::set_var(
            "ANA_KEYRING_PATH",
            keyring_dir.path().join("does-not-exist"),
        );

        let result = config_and_keyring_diagnostic();

        std::env::remove_var("ANA_CONFIG_PATH");
        std::env::remove_var("ANA_KEYRING_PATH");

        let (_, keyring_diagnostic) = result.unwrap();
        assert_eq!(keyring_diagnostic, None);
    }

    /// A keyring file that exists but is corrupt (not the common
    /// missing-file case) still lets `startup`'s fan-out succeed
    /// overall -- private-channel auth being broken must not block work
    /// against public channels -- but surfaces a diagnostic to print.
    #[test]
    fn config_and_keyring_diagnostic_surfaces_a_diagnostic_for_a_corrupt_keyring_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let keyring_dir = tempfile::tempdir().unwrap();
        let keyring_path = keyring_dir.path().join("keyring");
        std::fs::write(&keyring_path, b"not valid json").unwrap();
        std::env::set_var(
            "ANA_CONFIG_PATH",
            config_dir.path().join("does-not-exist.toml"),
        );
        std::env::set_var("ANA_KEYRING_PATH", &keyring_path);

        let result = config_and_keyring_diagnostic();

        std::env::remove_var("ANA_CONFIG_PATH");
        std::env::remove_var("ANA_KEYRING_PATH");

        let (_, keyring_diagnostic) = result.unwrap();
        assert!(keyring_diagnostic.is_some());
    }

    /// A malformed `config.toml` is still a real, fatal error -- only
    /// its *timing* changed (concurrent with the keyring read, not
    /// sequential before it), never its success/failure semantics.
    /// Gated the same way `ana::config`'s own disk-mutating tests are:
    /// a `commercial-config` build ignores `ANA_CONFIG_PATH`/disk
    /// entirely (see `ana::config::raw_config`), so this scenario
    /// cannot occur there.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_and_keyring_diagnostic_still_fails_on_a_malformed_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let keyring_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, b"not valid toml [[[").unwrap();
        std::env::set_var("ANA_CONFIG_PATH", &config_path);
        std::env::set_var(
            "ANA_KEYRING_PATH",
            keyring_dir.path().join("does-not-exist"),
        );

        let result = config_and_keyring_diagnostic();

        std::env::remove_var("ANA_CONFIG_PATH");
        std::env::remove_var("ANA_KEYRING_PATH");

        assert!(result.is_err());
    }
}
