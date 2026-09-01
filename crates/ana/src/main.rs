//! The `ana` binary: a thin shell over `ana::cli` and each command's
//! entry point. clap owns help text, parse errors, and their exit
//! codes; runtime failures print the error and exit 1.
//!
//! Builds the process-wide shared state exactly once here: the cache
//! root, the `Downloader`, the solver, and the pypi-to-conda mapping all
//! share one HTTP client and retry policy.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use ana::cli::{self, Command};
use ana::{clean_command, clean_global_command, exec, run_command, sync_command};
use ana::{EnsureOutcome, SyncOptions};
use ana_channels::ChannelPolicy;
use ana_environment::{Environment, EnvironmentRequest, RequirementInput};
use ana_installer::Downloader;
use ana_lockfile::{PlatformStatus, SolveScope};
use ana_solver::RattlerSolver;
use rattler_conda_types::Platform;
use uv_normalize::GroupName;

/// The channel search list [`main_kilo`]'s bootstrap solves against,
/// independent of `config.toml`: `"akulkarnizzz"` is where Kilo's own
/// conda package is published; `"defaults"` is where its one present
/// dependency (`ripgrep`) resolves from. Neither ever mixes with a
/// user's own project solves -- see [`build_fixed_channel_policy`].
const KILO_CHANNELS: &[&str] = &["akulkarnizzz", "defaults"];

/// The managed `kilo.json` provisioned into [`main_kilo`]'s Kilo config
/// directory: the remote MCP servers every `ana`-launched Kilo session
/// runs with, the per-agent permissions granting the `code` and `ask`
/// agents access to their tools, a global `bash` denylist blocking
/// every agent from invoking `uv`, `pip`, `conda`, or `pixi` directly --
/// those package managers bypass `ana`'s own dependency resolution and
/// lockfile, so agents must use `ana` instead -- and `skills_dir` (see
/// [`ensure_kilo_skill_files`]) as an extra skill-search path. Kilo
/// loads this as part of `KILO_CONFIG_DIR`, where it takes precedence
/// over the user's own global config.
fn kilo_config_json(skills_dir: &Path) -> String {
    let value = serde_json::json!({
        "mcp": {
            "terminal-space": {
                "type": "remote",
                "url": "https://repo.terminal.space/api/mcp",
                "enabled": true
            }
        },
        "permission": {
            "bash": {
                "uv *": "deny",
                "pip *": "deny",
                "conda *": "deny",
                "pixi *": "deny"
            }
        },
        "agent": {
            "code": {
                "permission": {
                    "terminal-space_*": "allow"
                }
            },
            "ask": {
                "permission": {
                    "terminal-space_*": "allow"
                }
            }
        },
        "skills": {
            "paths": [skills_dir.to_string_lossy()]
        }
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
}

/// `ana run <script>.py`'s exit code when [`main_kilo_script_assist`]'s
/// session returns but `script_path`'s re-checked
/// [`ana::DetectedScript`] is still
/// [`MissingMetadata`](ana::DetectedScript::MissingMetadata) -- distinct
/// from [`ExitCode::FAILURE`] so a caller scripting `ana run` can tell
/// "the user (or Kilo) declined to add metadata" apart from an ordinary
/// run failure.
const SCRIPT_ASSIST_DECLINED_EXIT_CODE: u8 = 10;

/// One skill provisioned by [`ensure_kilo_skill_files`]: `name` must
/// match its own `SKILL.md` frontmatter `name` field (Kilo's own naming
/// rule) and the directory it's written under.
struct Skill {
    name: &'static str,
    skill_md: &'static str,
}

/// The skills [`kilo_script_assist_prompt`] points its Kilo
/// session at: driving the ana CLI itself, parsing a script's own
/// imports into candidate dependencies, checking whether ana can
/// actually solve them, and proposing a channel/package policy change
/// when a dry-solve only succeeds after widening. Written
/// to disk by [`ensure_kilo_skill_files`] and referenced by absolute
/// path from [`kilo_config_json`]'s `skills.paths` -- so they load in
/// every Kilo session `ana` launches, not just a script-assist one.
const SKILLS: &[Skill] = &[
    Skill {
        name: "ana-cli",
        skill_md: include_str!("skills/ana-cli/SKILL.md"),
    },
    Skill {
        name: "python-script-requirements",
        skill_md: include_str!("skills/python-script-requirements/SKILL.md"),
    },
    Skill {
        name: "ana-dependency-check",
        skill_md: include_str!("skills/ana-dependency-check/SKILL.md"),
    },
    Skill {
        name: "terminal-space-policy",
        skill_md: include_str!("skills/terminal-space-policy/SKILL.md"),
    },
];

/// Writes each of [`SKILLS`] to `dir/<name>/SKILL.md` whenever its
/// current content differs, mirroring [`ensure_kilo_config_file`]'s own
/// self-healing idempotence: a stale or hand-edited copy is overwritten
/// on the next launch, and an already-current one is left untouched.
fn ensure_kilo_skill_files(dir: &Path) -> Result<(), String> {
    for skill in SKILLS {
        let skill_dir = dir.join(skill.name);
        std::fs::create_dir_all(&skill_dir)
            .map_err(|err| format!("could not create {}: {err}", skill_dir.display()))?;
        let path = skill_dir.join("SKILL.md");
        if std::fs::read_to_string(&path).ok().as_deref() == Some(skill.skill_md) {
            continue;
        }
        std::fs::write(&path, skill.skill_md)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    }
    Ok(())
}

/// The message [`main_kilo_script_assist`] sends to `kilo run` for
/// `script_path`, a `.py` file `ana::detect_script` found to have no PEP
/// 723 metadata block. Spells out the full step sequence rather than
/// just naming the skills, so the session's very first turn already has
/// a concrete plan to follow.
///
/// The final step tells the session to announce its own completion and
/// ask for a manual `Ctrl-C` -- interactive `kilo run` has no way to
/// exit itself once its turn ends (it's a persistent chat session by
/// design), so without an explicit instruction the user is left staring
/// at an idle prompt with no indication `ana` is waiting on them to
/// close it.
fn kilo_script_assist_prompt(script_path: &Path) -> String {
    format!(
        "`ana run` was asked to run {path}, a Python script with no PEP 723 \
         inline metadata (`# /// script ... # ///`) declaring its \
         dependencies. Follow this sequence:\n\
         \n\
         1. Load the `python-script-requirements` skill and use it to read \
         {path} and determine the candidate third-party dependencies its \
         imports actually need.\n\
         2. Load the `ana-dependency-check` skill and use it to check \
         whether those candidate dependencies can actually be solved by \
         ana, without writing anything to {path} or anywhere else yet. \
         (The `ana-cli` skill covers ana's command surface if you need \
         it.)\n\
         3. If that check reports the solve only succeeds after widening \
         to extra channels, load the `terminal-space-policy` skill and \
         follow it.\n\
         4. If the dependencies do not solve at all, stop and report why \
         -- do not edit {path}.\n\
         5. If they do solve, explicitly ask me for permission before \
         changing anything, showing the exact PEP 723 block you intend to \
         add (a `# /// script` ... `# ///` comment block near the top of \
         the file, after any shebang line, with a `dependencies = [...]` \
         array and, if relevant, a `requires-python` field).\n\
         6. Only once I say yes, add that block to {path}. Do not run the \
         script yourself -- `ana` will run it once this session ends.\n\
         7. Whichever of the above you stopped at, end your final message \
         by clearly stating that the task is complete (or why you stopped) \
         and that I should now press Ctrl-C to exit this session -- this \
         session cannot exit itself, and `ana` is waiting for it to close \
         before it can continue.\n\
         \n\
         If at any point a dependency looks unfamiliar, obscure, or \
         otherwise not confidently a real, known package, ask me about it \
         by name before treating it as safe to solve for or install -- \
         never assume a package is legitimate or popular just because the \
         script imports it.",
        path = script_path.display(),
    )
}

/// `ana sync --dry`'s exit code when solving only succeeded after
/// widening to `dry_solve_channels` -- distinct from [`ExitCode::SUCCESS`]
/// because the printed plan is *not* what a real `ana sync` would produce
/// until `dry_solve_channels`' channel(s) are promoted into
/// `allowed_channels`.
const DRY_WIDENED_CHANNELS_EXIT_CODE: u8 = 9;

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
        keyring_path: Option<&Path>,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| format!("could not start the async runtime: {err}"))?;

        let cache_root = rattler_cache::default_cache_dir()
            .map_err(|err| format!("could not determine the cache directory: {err}"))?;

        let downloader = Downloader::new(&cache_root, keyring_path)
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

/// The policy `ana sync --dry` retries with if solving with
/// [`build_channel_policy`]'s own policy fails: `default_channels ∪
/// dry_solve_channels` searched, still checked against
/// `allowed_channels`. `None` when `config` has no `dry_solve_channels`
/// configured -- there is nothing to widen to, so `--dry` behaves exactly
/// as it does today.
fn build_dry_fallback_channel_policy(
    config: &ana::config::ResolvedConfig,
) -> Result<Option<ChannelPolicy>, String> {
    let dry_solve_channels = config.dry_solve_channels.as_deref().unwrap_or(&[]);
    if dry_solve_channels.is_empty() {
        return Ok(None);
    }
    let mut widened = config.default_channels.clone();
    widened.extend(dry_solve_channels.iter().cloned());
    ChannelPolicy::new(&widened, config.allowed_channels.as_deref().unwrap_or(&[]))
        .map(Some)
        .map_err(|err| format!("invalid channel configuration: {err}"))
}

/// Builds a [`ChannelPolicy`] fixed to exactly `channels`, ignoring
/// `config.toml`'s own `default_channels`/`allowed_channels` entirely --
/// used only by [`main_kilo`]'s bootstrap, which names a fixed vendor
/// package on a fixed channel and must neither inherit a user's own
/// channel configuration nor be blocked by it.
fn build_fixed_channel_policy(channels: &[&str]) -> Result<ChannelPolicy, String> {
    let channels: Vec<String> = channels.iter().map(|channel| channel.to_string()).collect();
    ChannelPolicy::new(&channels, &[])
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
    /// The policy `ana sync --dry` retries with if solving with
    /// `channel_policy` fails -- see [`build_dry_fallback_channel_policy`].
    /// Always `None` when `channel_override` was `Some` (`main_kilo`'s
    /// bootstrap never runs `--dry`) or when the caller didn't ask
    /// [`startup`] to build it at all (see `startup`'s `want_dry_fallback`).
    dry_fallback_channel_policy: Option<ChannelPolicy>,
    cache_root: std::path::PathBuf,
    /// Set only when `~/.anaconda/keyring` exists but couldn't be read
    /// or parsed -- never for the common case of a simply-missing file.
    /// See `ana_auth::build_middleware`'s own docs.
    keyring_diagnostic: Option<String>,
    /// Channels whose packages must run under a nono sandbox (see
    /// `ana::sandbox`). `None` when the caller passed
    /// `bypass_sandbox: true`.
    sandboxed_channels: Option<Vec<String>>,
    /// The nono profile a sandboxed run is applied under -- ana's own
    /// built-in default unless `config.toml` sets `sandbox_policy`.
    sandbox_policy: String,
}

/// Runs `ana::config::resolve_config` and `ana_auth::build_middleware`
/// (discarding its middleware -- only the diagnostic matters here)
/// concurrently: both are independent, disk-only reads. A panic in
/// either thread is surfaced as a plain error string rather than
/// propagated as a panic itself.
fn config_and_keyring_diagnostic(
    config_path: Option<&Path>,
    keyring_path: Option<&Path>,
) -> Result<(ana::config::ResolvedConfig, Option<String>), String> {
    let (config_result, diagnostic_result) = std::thread::scope(|scope| {
        let config_handle = scope.spawn(move || ana::config::resolve_config(config_path));
        let diagnostic_handle =
            scope.spawn(move || ana_auth::build_middleware(keyring_path).diagnostic);
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

/// `exec_in_environment`/`main_sync`'s shared setup sequence: resolves
/// the `config.toml`/`~/.anaconda/keyring` paths (the one place the
/// process environment is consulted for them), reads both concurrently
/// ([`config_and_keyring_diagnostic`]), then builds the engine and
/// channel policy from `config`'s result.
///
/// `channel_override`, when `Some`, replaces `config`'s own
/// `default_channels`/`allowed_channels` for this call's
/// [`ChannelPolicy`] entirely (see [`build_fixed_channel_policy`]).
///
/// `bypass_sandbox`, when `true`, drops `config`'s own
/// `sandboxed_channels`/`sandbox_policy` for this call: the invocation
/// runs ana's own tooling, not project code.
///
/// `want_dry_fallback` gates building
/// [`Startup::dry_fallback_channel_policy`] at all: only `ana sync
/// --dry` ever reads it, so every other caller skips both the extra
/// `ChannelPolicy::new` work and the risk of a malformed
/// `dry_solve_channels` entry failing a command that would never have
/// used it anyway.
fn startup(
    mapping_options: ana_pypi_conda_map::LoadOptions,
    on_blocking_mapping_refresh: impl FnOnce(),
    channel_override: Option<&[&str]>,
    bypass_sandbox: bool,
    want_dry_fallback: bool,
) -> Result<Startup, String> {
    let config_path = ana_config::config_path();
    let keyring_path = ana_auth::keyring_path();
    let (config, keyring_diagnostic) =
        config_and_keyring_diagnostic(config_path.as_deref(), keyring_path.as_deref())?;

    let engine = Engine::build(
        &config.pypi_to_conda_uri,
        mapping_options,
        on_blocking_mapping_refresh,
        keyring_path.as_deref(),
    )?;
    let channel_policy = match channel_override {
        Some(channels) => build_fixed_channel_policy(channels)?,
        None => build_channel_policy(&config)?,
    };
    let dry_fallback_channel_policy = match channel_override {
        Some(_) => None,
        None if want_dry_fallback => build_dry_fallback_channel_policy(&config)?,
        None => None,
    };
    let cache_root = global_cache_root()?;
    let (sandboxed_channels, sandbox_policy) = if bypass_sandbox {
        (None, String::new())
    } else {
        (config.sandboxed_channels, config.sandbox_policy)
    };

    Ok(Startup {
        engine,
        channel_policy,
        dry_fallback_channel_policy,
        cache_root,
        keyring_diagnostic,
        sandboxed_channels,
        sandbox_policy,
    })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(err) => {
            eprintln!("ana: could not determine the current directory: {err}");
            return ExitCode::FAILURE;
        }
    };

    // A bare `ana` invocation (zero args), or a bare `ana -- ARGS...`
    // (no subcommand, just a leading literal `--` naming Kilo's own
    // argument list): neither ever reaches `cli::parse` at all, so
    // `--help`/`--version`/an unknown flag are unaffected -- each
    // supplies at least one arg that isn't a leading literal `--`, and
    // is handled by clap as usual. Both shapes are checked here, before
    // parsing, rather than added as a `Command` variant: zero args is
    // the one shape clap itself cannot single out (an `Option<Command>`
    // subcommand can't be told apart from a present-but-unmatched one
    // at the type level), and a leading `--` has no subcommand of its
    // own for clap to attach a trailing-`ARGS` field to.
    if let Some(kilo_args) = cli::kilo_passthrough_args(&args) {
        return main_kilo(&cwd, kilo_args);
    }

    let command = match cli::parse(&args) {
        Ok(command) => command,
        // Never returns: prints help/error and exits with clap's code.
        Err(err) => err.exit(),
    };

    match command {
        Command::Run {
            group,
            global,
            include,
            quiet,
            frozen,
            allow_stale_mapping,
            manifest,
            primary,
            program,
            args,
            agent,
        } => main_run(
            &cwd,
            group,
            global,
            include,
            quiet,
            frozen,
            allow_stale_mapping,
            manifest,
            primary,
            program,
            args,
            agent,
        ),
        Command::Sync {
            group,
            clean,
            frozen,
            allow_stale_mapping,
            manifest,
            subdir,
            dry,
            format,
        } => main_sync(
            &cwd,
            group,
            clean,
            frozen,
            allow_stale_mapping,
            manifest,
            subdir,
            dry,
            format,
        ),
        Command::Clean { global } => main_clean(&cwd, global),
        Command::Info {
            group,
            allow_stale_mapping,
            manifest,
            format,
        } => main_info(&cwd, group, allow_stale_mapping, manifest, format),
        Command::Search {
            channel,
            subdir,
            format,
            builds,
            show_subdir,
            deps,
            allow_stale_mapping,
            spec,
        } => main_search(
            channel,
            subdir,
            format,
            builds,
            show_subdir,
            deps,
            allow_stale_mapping,
            spec,
        ),
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
    manifest: cli::ManifestArgs,
    primary: String,
    program: Option<String>,
    args: Vec<String>,
    script_assist: ana::ScriptAssistMode,
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
        &manifest,
        &invocation,
        None,
        &[],
        false,
        script_assist,
    )
}

/// `ana login`: a fixed `ana run -g anaconda-auth anaconda -- login`
/// invocation. Its ad hoc environment is keyed by `anaconda-auth` alone,
/// the same as any other `ana run -g anaconda-auth ...` -- so once
/// materialized here, it's reused (not re-solved or reinstalled) by
/// every later `ana login`, or any other `-g anaconda-auth` invocation.
/// `bypass_sandbox: true`: an interactive OAuth flow must not run inside
/// a nono sandbox.
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
        &cli::ManifestArgs::default(),
        &invocation,
        None,
        &[],
        true,
        ana::ScriptAssistMode::Off,
    )
}

/// Bare `ana` -- no subcommand, and (since it takes at least one arg to
/// spell) never `--help`/`--version` either -- drops the user into
/// Kilo's own interactive agent harness. A fixed
/// `ana run -g akulkarnizzz::kilo` invocation: the same "materialize an
/// ad hoc global environment, then exec into it" pattern [`main_login`]
/// uses for `anaconda-auth`. Its ad hoc environment is keyed by
/// `akulkarnizzz::kilo` alone, so once materialized here it's reused
/// (not re-solved or reinstalled) by every later bare `ana` invocation.
///
/// `args` -- from [`cli::kilo_passthrough_args`] -- becomes Kilo's own
/// argument list: empty for a bare `ana`, or whatever followed a
/// leading literal `--` (`ana -- mcp auth abc` execs `kilo mcp auth
/// abc`).
///
/// Unlike `main_login`'s `anaconda-auth` (already reachable via the
/// user's own configured `default_channels`), Kilo's package lives on
/// its own `akulkarnizzz` channel, which no user config authorizes by
/// default -- so this bypasses `config.toml` entirely via
/// [`exec_in_environment`]'s `channel_override`, rather than requiring
/// every user to first `ana config set allowed_channels akulkarnizzz`.
///
/// The `kilo` process itself is launched config-isolated: [`kilo_config_dir`]
/// and [`kilo_env_vars`] point it at a `KILO_CONFIG_DIR`/`KILO_DB` `ana`
/// fully owns, reusing the user's real Kilo auth (if any) by value via
/// `KILO_AUTH_CONTENT` rather than by sharing a file path.
fn main_kilo(cwd: &Path, args: Vec<String>) -> ExitCode {
    let invocation = match cli::resolve_run_invocation(
        true,
        "akulkarnizzz::kilo".to_string(),
        None,
        Vec::new(),
        args,
    ) {
        Ok(invocation) => invocation,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    let config_dir = match kilo_config_dir() {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("ana: {message}");
            return ExitCode::FAILURE;
        }
    };
    let extra_env = kilo_env_vars(&config_dir, real_kilo_auth_content().as_deref());

    exec_in_environment(
        cwd,
        &[],
        true,
        false,
        false,
        false,
        &cli::ManifestArgs::default(),
        &invocation,
        Some(KILO_CHANNELS),
        &extra_env,
        true,
        ana::ScriptAssistMode::Off,
    )
}

/// Every way [`detect_script_or_assist`] can fail to produce a script
/// for its caller to run.
enum ScriptOrAssistError {
    /// Detection itself failed (a malformed PEP 723 block), or
    /// [`main_kilo_script_assist`] could not even launch its Kilo
    /// session -- either way, `ana run` cannot proceed at all.
    Fatal(String),
    /// [`main_kilo_script_assist`]'s session returned, but `candidate`
    /// still has no PEP 723 metadata: the user (or Kilo) declined, or
    /// simply didn't get to it. Distinct from `Fatal` so the caller can
    /// exit with [`SCRIPT_ASSIST_DECLINED_EXIT_CODE`] instead of
    /// [`ExitCode::FAILURE`].
    Declined(PathBuf),
}

/// Wraps [`ana::detect_script`] for `ana run`'s own use: a
/// [`ana::DetectedScript::MissingMetadata`] result is not returned to
/// the caller directly when `mode` isn't
/// [`Off`](ana::ScriptAssistMode::Off) -- instead,
/// [`main_kilo_script_assist`] runs first, and `candidate` is
/// re-checked afterward, so the caller only ever sees a script that
/// either already had metadata or was just given some. Under `Off`,
/// behaves exactly as it did before this feature existed: missing
/// metadata is reported as [`Ok(None)`], the same as
/// [`NotAScript`](ana::DetectedScript::NotAScript).
fn detect_script_or_assist(
    cwd: &Path,
    candidate: &str,
    quiet: bool,
    mode: ana::ScriptAssistMode,
) -> Result<Option<(PathBuf, ana_requirements::RequirementSet)>, ScriptOrAssistError> {
    let to_fatal = |err: ana_pep723::Pep723Error| ScriptOrAssistError::Fatal(err.to_string());

    match ana::detect_script(cwd, candidate).map_err(to_fatal)? {
        ana::DetectedScript::Found(path, requirements) => Ok(Some((path, requirements))),
        ana::DetectedScript::NotAScript => Ok(None),
        ana::DetectedScript::MissingMetadata(_) if mode == ana::ScriptAssistMode::Off => Ok(None),
        ana::DetectedScript::MissingMetadata(path) => {
            if !quiet {
                let how = match mode {
                    ana::ScriptAssistMode::Headless => " headlessly (--agent headless)",
                    ana::ScriptAssistMode::Interactive | ana::ScriptAssistMode::Off => "",
                };
                eprintln!(
                    "ana: {} has no PEP 723 metadata; asking Kilo for help adding it{how}...",
                    path.display()
                );
            }
            main_kilo_script_assist(&path, mode).map_err(ScriptOrAssistError::Fatal)?;

            match ana::detect_script(cwd, candidate).map_err(to_fatal)? {
                ana::DetectedScript::Found(path, requirements) => Ok(Some((path, requirements))),
                ana::DetectedScript::NotAScript | ana::DetectedScript::MissingMetadata(_) => {
                    Err(ScriptOrAssistError::Declined(path))
                }
            }
        }
    }
}

/// The extra `kilo run` flags [`main_kilo_script_assist`] adds on top of
/// `--agent code`, chosen by `mode`. `kilo run`'s own default (neither
/// flag) is a one-shot streaming mode that cannot prompt for approval
/// at all and auto-rejects any permission request it receives -- fine
/// for nothing this feature needs, so every non-`Off` mode picks one of
/// the other two explicitly:
///
/// - [`Interactive`](ana::ScriptAssistMode::Interactive): `--interactive`
///   (`-i`), `kilo run`'s own direct interactive split-footer mode, so a
///   live user actually sees and can answer permission prompts and the
///   skill's own confirmation questions.
/// - [`Headless`](ana::ScriptAssistMode::Headless): `--auto`, so Kilo
///   auto-approves whatever isn't explicitly denied instead of hanging
///   (or auto-rejecting) waiting for input that will never arrive.
///
/// Never called under [`Off`](ana::ScriptAssistMode::Off) -- see
/// [`detect_script_or_assist`].
fn script_assist_kilo_flags(mode: ana::ScriptAssistMode) -> Vec<String> {
    match mode {
        ana::ScriptAssistMode::Interactive => vec!["--interactive".to_string()],
        ana::ScriptAssistMode::Headless => vec!["--auto".to_string()],
        ana::ScriptAssistMode::Off => vec![],
    }
}

/// Launches a Kilo session (see [`kilo_script_assist_prompt`]) to help
/// add PEP 723 metadata to `script_path`, a `.py` file
/// [`ana::detect_script`] found to have none. Spawns (rather than
/// execs, unlike [`main_kilo`] itself) the same ad hoc
/// `akulkarnizzz::kilo` environment, waits for the session to exit,
/// then returns control to [`detect_script_or_assist`], which
/// re-checks `script_path` for a metadata block that may now be there.
/// Any exit status from the session itself -- the user quitting, the
/// agent declining, a crash -- is treated the same way here: the
/// caller decides what happened from the file, not from this
/// function's own success.
///
/// Runs the `code` agent (full tool access, including `edit`), not
/// `ask` (which has none at all and so could never write the metadata
/// even after asking permission). `mode` picks the rest of the flags
/// via [`script_assist_kilo_flags`]; never called at all under `Off`
/// (see [`detect_script_or_assist`]).
///
/// Spawns the child inheriting this process's own current directory
/// (never changed, so it's still the project root `ana run` itself was
/// invoked from) -- no explicit `cwd` is threaded through here.
fn main_kilo_script_assist(script_path: &Path, mode: ana::ScriptAssistMode) -> Result<(), String> {
    let prompt = kilo_script_assist_prompt(script_path);
    let mut args = vec![
        "run".to_string(),
        prompt,
        "--agent".to_string(),
        "code".to_string(),
    ];
    args.extend(script_assist_kilo_flags(mode));
    let invocation = cli::resolve_run_invocation(
        true,
        "akulkarnizzz::kilo".to_string(),
        None,
        Vec::new(),
        args,
    )
    .map_err(|err| err.to_string())?;

    let config_dir = kilo_config_dir()?;
    let extra_env = kilo_env_vars(&config_dir, real_kilo_auth_content().as_deref());

    let Startup {
        engine,
        channel_policy,
        cache_root,
        ..
    } = startup(
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping: false,
            force_refresh: false,
        },
        || {},
        Some(KILO_CHANNELS),
        true,
        false,
    )?;

    let env = ana_environment::resolve(&EnvironmentRequest {
        input: RequirementInput::CommandLine {
            dependencies: &invocation.cli_deps,
        },
        groups: &[],
        extra: &[],
        platform: Platform::current(),
        pypi_to_conda_map: &engine.mapping,
        global_cache_root: &cache_root,
    })
    .map_err(|err| err.to_string())?;

    let outcome = run_command(
        &env,
        &SolveScope {
            channels: &channel_policy,
            pypi_to_conda_map: &engine.mapping,
        },
        &invocation.exec_command,
        false,
        &engine.solver,
        engine.runtime.handle(),
        &engine.downloader,
    )
    .map_err(|err| err.to_string())?;

    ana::spawn_and_wait(&outcome, &extra_env)
        .map(|_status| ())
        .map_err(|err| err.to_string())
}

/// The [`RequirementInput`] an explicit `--manifest`/`--manifest-type`
/// pair selects, rooted at `root`; `None` when neither (or only one) is
/// set, leaving the caller to fall back to its own default (a project
/// directory, a script, or an ad hoc declaration). Shared by `run` and
/// `sync` so their manifest-selection logic can't drift apart.
fn manifest_input<'a>(
    manifest: &'a cli::ManifestArgs,
    root: &'a Path,
) -> Option<RequirementInput<'a>> {
    match (&manifest.manifest, manifest.manifest_type) {
        (Some(path), Some(kind)) => Some(RequirementInput::ExplicitFile { path, kind, root }),
        _ => None,
    }
}

/// [`main_kilo`]'s own Kilo config directory -- [`ana_paths::kilo_config_dir`],
/// created if it doesn't already exist so `KILO_CONFIG_DIR` always names
/// a real directory, never a dangling path, provisioned with
/// [`ensure_kilo_skill_files`]'s skills and [`kilo_config_json`]'s
/// `kilo.json` (which points `skills.paths` at them).
fn kilo_config_dir() -> Result<PathBuf, String> {
    let dir = ana_paths::kilo_config_dir().ok_or_else(|| {
        "could not determine ana's Kilo config directory (no resolvable home directory)".to_string()
    })?;
    ensure_kilo_config_dir(&dir)?;
    let skills_dir = dir.join("skills");
    ensure_kilo_skill_files(&skills_dir)?;
    ensure_kilo_config_file(&dir, &skills_dir)?;
    Ok(dir)
}

/// Creates `dir` if needed and restricts it to the owner alone
/// ([`restrict_to_owner`]): it holds `KILO_DB`, a session/credential
/// database, so it must never be left group/world-readable by the
/// process's umask.
fn ensure_kilo_config_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    restrict_to_owner(dir).map_err(|err| format!("could not secure {}: {err}", dir.display()))?;
    Ok(())
}

/// Restricts `dir`'s permissions to the owner alone (`0700`) on Unix,
/// applied unconditionally (not just on first creation) so a directory
/// left over from before this restriction existed is self-healed on the
/// next `ana kilo` invocation. No-op on platforms without POSIX
/// permission bits.
#[cfg(unix)]
fn restrict_to_owner(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Restricts `dir`'s permissions to the owner alone. Windows ACLs
/// already default a user's own profile-relative directories (such as
/// this one, under `%APPDATA%`) to that user alone, so there is nothing
/// further to do here.
#[cfg(not(unix))]
fn restrict_to_owner(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Writes [`kilo_config_json`]`(skills_dir)` to `dir/kilo.json` whenever
/// its current content differs, so a stale or hand-edited file
/// self-heals on the next launch without rewriting an already
/// up-to-date one.
fn ensure_kilo_config_file(dir: &Path, skills_dir: &Path) -> Result<(), String> {
    let path = dir.join("kilo.json");
    let content = kilo_config_json(skills_dir);
    if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
        return Ok(());
    }
    std::fs::write(&path, &content)
        .map_err(|err| format!("could not write {}: {err}", path.display()))
}

/// The environment variables [`main_kilo`] adds to the `kilo` child
/// process, on top of `PATH`: `KILO_CONFIG_DIR` and `KILO_DB` always
/// point inside `config_dir`, isolating the subprocess's config and its
/// own session/credential database from the user's real Kilo install.
/// Kilo treats a later-loaded config source as higher precedence than
/// the user's own global config, so `KILO_CONFIG_DIR`'s settings always
/// win on any conflicting key without `ana` needing to hide that global
/// config from Kilo at all.
///
/// `KILO_AUTH_CONTENT` is included only when the user already has a
/// real Kilo auth store on disk (`auth_content`, from
/// [`real_kilo_auth_content`]) -- it hands Kilo's auth store that
/// store's contents by value, so the isolated subprocess authenticates
/// against the same AI gateway without `ana` ever writing to, or
/// pointing Kilo directly at, the user's real credential file.
fn kilo_env_vars(config_dir: &Path, auth_content: Option<&str>) -> Vec<(&'static str, OsString)> {
    let mut vars = vec![
        ("KILO_CONFIG_DIR", config_dir.as_os_str().to_owned()),
        ("KILO_DB", config_dir.join("kilo.db").into_os_string()),
    ];
    if let Some(content) = auth_content {
        vars.push(("KILO_AUTH_CONTENT", content.into()));
    }
    vars
}

/// The real Kilo auth store's contents, if one exists on disk --
/// `${XDG_DATA_HOME:-$HOME/.local/share}/kilo/auth.json`, matching
/// Kilo's own (OS-independent) data-directory resolution. `None` for
/// any reason at all (no resolvable home directory, no such file, or
/// an unreadable one) -- a missing file is the ordinary case for anyone
/// who has never run Kilo's own login flow, not a failure worth
/// reporting.
fn real_kilo_auth_content() -> Option<String> {
    let data_dir = match std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => ana_paths::home_dir()?.join(".local").join("share"),
    };
    real_kilo_auth_content_under(&data_dir)
}

/// `data_dir/kilo/auth.json`'s contents, or `None` for any reason at
/// all.
fn real_kilo_auth_content_under(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join("kilo").join("auth.json")).ok()
}

/// Materializes whatever environment `invocation` targets -- `manifest`
/// (an explicit `--manifest`/`--manifest-type` file) when set, else the
/// project's (default, or `--group`-selected) under `global == false`,
/// a PEP 723 script's own block when `<primary>` names one, or, under
/// `global == true`, an ad hoc one keyed by `invocation.cli_deps`
/// alone -- brings it up to date, and execs `invocation.exec_command`
/// inside it (as `python <script>` for a script, independent of which
/// declaration governs its dependencies: an explicit manifest overrides
/// a script's own inline block without changing how it's executed).
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
///
/// `channel_override` and `bypass_sandbox` are forwarded to [`startup`]
/// verbatim; `extra_env` is forwarded to [`exec`] verbatim -- it must
/// reach the child process actually being exec'd into, not the ad hoc
/// environment it runs in. `script_assist` is forwarded to
/// [`detect_script_or_assist`] verbatim; irrelevant whenever `global`
/// is `true`, since script detection never runs at all in that case --
/// every caller with no script of its own to detect (`main_login`,
/// `main_kilo`) passes [`ana::ScriptAssistMode::Off`].
#[allow(clippy::too_many_arguments)]
fn exec_in_environment(
    cwd: &Path,
    groups: &[GroupName],
    global: bool,
    quiet: bool,
    frozen: bool,
    allow_stale_mapping: bool,
    manifest: &cli::ManifestArgs,
    invocation: &cli::RunInvocation,
    channel_override: Option<&[&str]>,
    extra_env: &[(&str, OsString)],
    bypass_sandbox: bool,
    script_assist: ana::ScriptAssistMode,
) -> ExitCode {
    // Only a non-`-g` `<primary>` can ever be a PEP 723 script: under
    // `-g`, `<primary>` is already a requirement specifier, not a
    // program name. `invocation.exec_command[0]` is exactly the
    // original `<primary>` string in that case -- see
    // `resolve_run_invocation`'s docs. A `.py` file with no metadata is
    // not an immediate failure here -- see `detect_script_or_assist`.
    let script = if global {
        None
    } else {
        match detect_script_or_assist(cwd, &invocation.exec_command[0], quiet, script_assist) {
            Ok(script) => script,
            Err(ScriptOrAssistError::Fatal(err)) => {
                if !quiet {
                    eprintln!("ana: {err}");
                }
                return ExitCode::FAILURE;
            }
            Err(ScriptOrAssistError::Declined(path)) => {
                if !quiet {
                    eprintln!(
                        "ana: {} still has no PEP 723 metadata; not running it",
                        path.display()
                    );
                }
                return ExitCode::from(SCRIPT_ASSIST_DECLINED_EXIT_CODE);
            }
        }
    };

    let Startup {
        engine,
        channel_policy,
        cache_root,
        keyring_diagnostic,
        sandboxed_channels,
        sandbox_policy,
        ..
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
        channel_override,
        bypass_sandbox,
        false,
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

    // `--manifest` overrides whatever declares an environment's
    // dependencies by default -- a script's own inline PEP 723 block
    // included -- so it's checked first. `script_exec_command` below
    // still decides independently whether to exec `python <script>`:
    // an explicit manifest changes *which* declaration governs the
    // solve, not whether `<primary>` is a script.
    let input = manifest_input(manifest, cwd).unwrap_or_else(|| match &script {
        Some((path, requirements)) => RequirementInput::Script { path, requirements },
        None if global => RequirementInput::CommandLine {
            dependencies: &invocation.cli_deps,
        },
        None => RequirementInput::ProjectDir { dir: cwd },
    });
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
                report_solve_error(&err, &env, &engine.mapping);
            }
            return ExitCode::FAILURE;
        }
    };

    if !quiet {
        report_ensure(outcome.ensure);
        report_install(outcome.install.is_some());
    }

    let needs_sandbox = match &sandboxed_channels {
        Some(channels) if !channels.is_empty() => {
            match ana::sandbox::packages_require_sandbox(channels, &outcome.packages) {
                Ok(needs) => needs,
                Err(err) => {
                    if !quiet {
                        eprintln!("ana: {err}");
                    }
                    return ExitCode::FAILURE;
                }
            }
        }
        _ => false,
    };

    if !quiet {
        report_exec(&outcome.command, needs_sandbox);
    }

    if needs_sandbox {
        return exec_sandboxed(cwd, &engine, &cache_root, &sandbox_policy, &outcome, quiet);
    }

    // `engine` is intentionally dropped here without calling
    // `MappingHandle::finish`: joining a background refresh would block
    // the fast path it exists to keep fast, and `exec` never returns on
    // success (Unix) -- skipping `finish()` is always safe (see
    // `MappingHandle::finish`'s own docs).
    let err = exec(&outcome, extra_env);
    if !quiet {
        eprintln!("ana: {err}");
    }
    ExitCode::FAILURE
}

/// Why [`bootstrap_nono`] couldn't produce a nono environment.
enum NonoBootstrapError {
    /// The nono package couldn't be solved for this platform at all.
    Unavailable,
    Failed(String),
}

/// Materializes (or reuses) the ad hoc environment `conda-forge::nono` is
/// installed into, bypassing whatever channel policy governs the
/// environment actually being sandboxed -- nono is ana's own tooling, not
/// something a project's `allowed_channels` has any say over. Returns the
/// environment's own prefix, from which nono's binary is resolved via
/// `PATH` (see `ana::sandbox::env_bin_dirs`).
fn bootstrap_nono(
    engine: &Engine,
    cache_root: &Path,
) -> Result<std::path::PathBuf, NonoBootstrapError> {
    let spec = format!(
        "{}::{}",
        ana::sandbox::NONO_CHANNEL,
        ana::sandbox::NONO_PACKAGE
    );
    let invocation = cli::resolve_run_invocation(true, spec, None, Vec::new(), Vec::new())
        .map_err(|err| NonoBootstrapError::Failed(err.to_string()))?;
    let policy = build_fixed_channel_policy(&[ana::sandbox::NONO_CHANNEL])
        .map_err(NonoBootstrapError::Failed)?;
    let env = ana_environment::resolve(&EnvironmentRequest {
        input: RequirementInput::CommandLine {
            dependencies: &invocation.cli_deps,
        },
        groups: &[],
        extra: &[],
        platform: Platform::current(),
        pypi_to_conda_map: &engine.mapping,
        global_cache_root: cache_root,
    })
    .map_err(|err| NonoBootstrapError::Failed(err.to_string()))?;
    let outcome = run_command(
        &env,
        &SolveScope {
            channels: &policy,
            pypi_to_conda_map: &engine.mapping,
        },
        &invocation.exec_command,
        false,
        &engine.solver,
        engine.runtime.handle(),
        &engine.downloader,
    )
    .map_err(|err| {
        if nono_is_unsolvable(&err) {
            NonoBootstrapError::Unavailable
        } else {
            NonoBootstrapError::Failed(err.to_string())
        }
    })?;
    Ok(outcome.env_path)
}

/// Whether `err` is the solver reporting the nono package simply isn't
/// published for the current platform (Windows has no nono build).
fn nono_is_unsolvable(err: &ana::Error) -> bool {
    let ana::Error::Lockfile(ana_lockfile::Error::Solve { source, .. }) = err else {
        return false;
    };
    source
        .downcast_ref::<ana_solver::Error>()
        .is_some_and(ana_solver::Error::is_unsolvable)
}

/// Runs `outcome.command` inside `outcome.env_path`, wrapped in a nono
/// sandbox: bootstraps nono ([`bootstrap_nono`]), translates
/// `sandbox_policy` into `nono run` arguments and environment variables,
/// pre-creates every directory those variables point at, then execs
/// `nono` with `PATH` covering both nono's own environment and the
/// sandboxed one. Never returns on success, the same as [`exec`].
fn exec_sandboxed(
    cwd: &Path,
    engine: &Engine,
    cache_root: &Path,
    sandbox_policy: &str,
    outcome: &ana::RunOutcome,
    quiet: bool,
) -> ExitCode {
    let nono_env_path = match bootstrap_nono(engine, cache_root) {
        Ok(path) => path,
        Err(NonoBootstrapError::Unavailable) => {
            if !quiet {
                if cfg!(windows) {
                    eprintln!("ana: sandboxing is not available on Windows");
                } else {
                    eprintln!(
                        "ana: sandboxing is not available on this platform: \
                         nono is not published for {}",
                        Platform::current()
                    );
                }
            }
            return ExitCode::FAILURE;
        }
        Err(NonoBootstrapError::Failed(message)) => {
            if !quiet {
                eprintln!("ana: could not prepare the nono sandbox: {message}");
            }
            return ExitCode::FAILURE;
        }
    };

    let translated = match ana::sandbox::translate_policy(sandbox_policy, &outcome.env_path, cwd) {
        Ok(translated) => translated,
        Err(err) => {
            if !quiet {
                eprintln!("ana: {err}");
            }
            return ExitCode::FAILURE;
        }
    };

    // Some libraries (Jupyter among them) probe for a directory rather
    // than creating it lazily, so a `set_vars` target that doesn't exist
    // yet can make an otherwise-working tool fail inside the sandbox.
    let dirs: std::collections::BTreeSet<std::path::PathBuf> = translated
        .env
        .values()
        .map(std::path::PathBuf::from)
        .collect();
    for dir in &dirs {
        if let Err(err) = std::fs::create_dir_all(dir) {
            if !quiet {
                eprintln!("ana: could not create {}: {err}", dir.display());
            }
            return ExitCode::FAILURE;
        }
    }

    let nono_command = ana::sandbox::nono_argv(&translated.args, cwd, &outcome.command);
    let mut path_dirs = ana::sandbox::env_bin_dirs(&nono_env_path);
    path_dirs.extend(ana::sandbox::env_bin_dirs(&outcome.env_path));
    if let Some(existing) = std::env::var_os("PATH") {
        path_dirs.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(&path_dirs)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default());

    let err = ana::exec_program_with_clean_env(
        "nono",
        &nono_command,
        &path,
        &translated.env,
        &outcome.command,
    );
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
    manifest: cli::ManifestArgs,
    subdirs: Vec<Platform>,
    dry: bool,
    format: ana::dry::Format,
) -> ExitCode {
    let Startup {
        engine,
        channel_policy,
        dry_fallback_channel_policy,
        cache_root,
        keyring_diagnostic,
        ..
    } = match startup(
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping,
            force_refresh: false,
        },
        || eprintln!("ana: downloading conda name translations..."),
        None,
        false,
        dry,
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

    let input = manifest_input(&manifest, cwd).unwrap_or(RequirementInput::ProjectDir { dir: cwd });
    let env = match ana_environment::resolve(&EnvironmentRequest {
        input,
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
        let fallback_scope = dry_fallback_channel_policy
            .as_ref()
            .map(|policy| SolveScope {
                channels: policy,
                pypi_to_conda_map: &engine.mapping,
            });
        let (plan, exit_on_success) = match ana::dry::plan_sync_with_fallback(
            &env,
            &subdirs,
            &scope,
            fallback_scope.as_ref(),
            &engine.solver,
        ) {
            Ok(ana::dry::DryOutcome::Direct(plan)) => (plan, ExitCode::SUCCESS),
            Ok(ana::dry::DryOutcome::Widened(plan)) => {
                eprintln!(
                    "ana: this plan only solved after also searching dry_solve_channels; \
                     a real `ana sync` would still fail until those channels are promoted \
                     into allowed_channels -- exiting {DRY_WIDENED_CHANNELS_EXIT_CODE}\n\
                     ana: to fix this, load the `terminal-space-policy` skill: it authors a \
                     policy draft admitting the packages above, and walks you through \
                     promoting it, binding it to a channel, and authorizing that channel."
                );
                (plan, ExitCode::from(DRY_WIDENED_CHANNELS_EXIT_CODE))
            }
            Err(err) => {
                report_solve_error(&err, &env, &engine.mapping);
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
        return exit_on_success;
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
            report_solve_error(&err, &env, &engine.mapping);
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

/// `ana search`'s exit code when every channel answered but none had a
/// match -- distinct from success, so `ana search foo && ...` works.
const SEARCH_NO_MATCHES_EXIT_CODE: u8 = 1;

/// `ana search`'s exit code when the query never completed: an
/// unparseable spec, an unauthorized `--channel`, or any channel
/// unreachable -- "not found" can't be concluded from any of those.
const SEARCH_QUERY_FAILED_EXIT_CODE: u8 = 2;

#[allow(clippy::too_many_arguments)]
fn main_search(
    channel_args: Vec<String>,
    subdirs: Vec<Platform>,
    format: ana::search::SearchFormat,
    builds: bool,
    show_subdir: bool,
    deps: bool,
    allow_stale_mapping: bool,
    spec: String,
) -> ExitCode {
    let Startup {
        engine,
        channel_policy,
        keyring_diagnostic,
        ..
    } = match startup(
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping,
            force_refresh: false,
        },
        || eprintln!("ana: downloading conda name translations..."),
        None,
        false,
        false,
    ) {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("ana: {message}");
            return ExitCode::from(SEARCH_QUERY_FAILED_EXIT_CODE);
        }
    };
    if let Some(diagnostic) = &keyring_diagnostic {
        eprintln!("ana: {diagnostic}");
    }

    let spec = match ana::search::resolve_spec(&spec, &engine.mapping) {
        Ok(spec) => spec,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::from(SEARCH_QUERY_FAILED_EXIT_CODE);
        }
    };
    if let ana::search::NameMapping::Mapped(pypi_name) = &spec.mapping {
        eprintln!(
            "ana: '{pypi_name}' maps to conda package '{}'",
            spec.conda_name
        );
    }

    let platforms = if subdirs.is_empty() {
        vec![Platform::current()]
    } else {
        subdirs
    };

    let channels =
        match ana::search::resolve_channels(&channel_policy, &channel_args, &spec, &platforms) {
            Ok(channels) => channels,
            Err(err) => {
                eprintln!("ana: {err}");
                return ExitCode::from(SEARCH_QUERY_FAILED_EXIT_CODE);
            }
        };

    let report = ana::search::search(&spec, &channels, &platforms, &engine.solver);

    let rendered = match ana::search::render(
        &report,
        format,
        ana::search::DisplayOptions {
            builds,
            subdir: show_subdir,
            deps,
        },
    ) {
        Ok(rendered) => rendered,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::from(SEARCH_QUERY_FAILED_EXIT_CODE);
        }
    };
    print!("{rendered}");

    // Like `main_sync`: search returns normally, so an in-flight
    // background mapping refresh is waited on rather than killed
    // mid-rename by process exit.
    let _ = engine.mapping.finish();

    if report.any_matches() {
        ExitCode::SUCCESS
    } else if report.all_channels_failed() {
        eprintln!(
            "ana: could not search any channel for '{}'",
            report.conda_name
        );
        ExitCode::from(SEARCH_QUERY_FAILED_EXIT_CODE)
    } else if report.any_channel_failed() {
        eprintln!(
            "ana: '{}' was not found on the channels that answered, \
             but some channels could not be searched",
            report.conda_name
        );
        ExitCode::from(SEARCH_QUERY_FAILED_EXIT_CODE)
    } else {
        eprintln!(
            "ana: '{}' was not found on any searched channel",
            report.conda_name
        );
        if matches!(report.mapping, ana::search::NameMapping::Unmapped) {
            eprintln!(
                "ana: no pypi-to-conda mapping entry for '{}'; searched as-is",
                report.input
            );
        }
        ExitCode::from(SEARCH_NO_MATCHES_EXIT_CODE)
    }
}

fn main_info(
    cwd: &Path,
    groups: Vec<GroupName>,
    allow_stale_mapping: bool,
    manifest: cli::ManifestArgs,
    format: ana::info::Format,
) -> ExitCode {
    let Startup {
        engine,
        channel_policy,
        cache_root,
        keyring_diagnostic,
        sandboxed_channels,
        ..
    } = match startup(
        ana_pypi_conda_map::LoadOptions {
            allow_stale_mapping,
            force_refresh: false,
        },
        || eprintln!("ana: downloading conda name translations..."),
        None,
        false,
        false,
    ) {
        Ok(startup) => startup,
        Err(message) => {
            eprintln!("ana: {message}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(diagnostic) = &keyring_diagnostic {
        eprintln!("ana: {diagnostic}");
    }

    let input = manifest_input(&manifest, cwd).unwrap_or(RequirementInput::ProjectDir { dir: cwd });
    let env = match ana_environment::resolve(&EnvironmentRequest {
        input,
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

    let report = match ana::info::gather(
        &env,
        Platform::current(),
        &SolveScope {
            channels: &channel_policy,
            pypi_to_conda_map: &engine.mapping,
        },
        &engine.solver,
        sandboxed_channels.as_deref().unwrap_or(&[]),
    ) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };

    let rendered = match ana::info::render(&report, format) {
        Ok(rendered) => rendered,
        Err(err) => {
            eprintln!("ana: {err}");
            return ExitCode::FAILURE;
        }
    };
    print!("{rendered}");

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
    let config_path = ana_config::config_path();
    let result = match action {
        cli::ConfigAction::Get { key } => {
            ana::config::config_get(key, config_path.as_deref()).map(|text| {
                println!("{text}");
            })
        }
        cli::ConfigAction::Set { key, values } => {
            ana::config::config_set(key, &values, config_path.as_deref())
        }
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

/// Echoes the exact command about to be exec'd -- the derived program
/// included, so `ana run -g python -- python -c ...` shows up as
/// `python python -c ...` before it runs. A sandboxed run says so,
/// every time.
fn report_exec(command: &[String], sandboxed: bool) {
    let rendered = ana::shell_join(command);
    if sandboxed {
        eprintln!("ana: running {rendered} -- inside a nono sandbox");
    } else {
        eprintln!("ana: running {rendered}");
    }
}

/// Prints `err`, enriched when it is a solve failure carrying structure
/// main can add context to: a repodata fetch failure is called out as
/// an unreachable searched channel (not an unsatisfiable solve), and an
/// unsolvable solve that `ana-solver` classified lists each requirement
/// that has no candidates at all, annotated with its pypi-to-conda
/// mapping decision (`env`'s selected requirements carry the original
/// PEP 508 names).
fn report_solve_error(
    err: &ana::Error,
    env: &Environment,
    mapping: &ana_pypi_conda_map::MappingHandle,
) {
    match diagnose_solve_error(err, env, mapping) {
        Some(message) => eprintln!("ana: {message}"),
        None => eprintln!("ana: {err}"),
    }
}

fn diagnose_solve_error(
    err: &ana::Error,
    env: &Environment,
    mapping: &ana_pypi_conda_map::MappingHandle,
) -> Option<String> {
    let ana::Error::Lockfile(ana_lockfile::Error::Solve { source, .. }) = err else {
        return None;
    };
    match source.downcast_ref::<ana_solver::Error>()? {
        ana_solver::Error::Gateway(gateway) => Some(format!(
            "a searched channel could not be reached: {gateway}"
        )),
        ana_solver::Error::Unsolvable {
            missing, channels, ..
        } => Some(diagnose_unsolvable(
            missing,
            channels,
            &env.select(),
            mapping,
        )),
        _ => None,
    }
}

fn diagnose_unsolvable(
    missing: &[ana_solver::MissingSpec],
    channels: &[String],
    selected: &[ana_dependency::SelectedRequirement<'_>],
    mapping: &ana_pypi_conda_map::MappingHandle,
) -> String {
    let mut lines = Vec::with_capacity(missing.len());
    for spec in missing {
        match mapping_note(&spec.name, selected, mapping) {
            Some(note) => lines.push(format!("  {} -- {note}", spec.spec)),
            None => lines.push(format!("  {}", spec.spec)),
        }
    }
    format!(
        "no candidates were found for these requirements on any searched channel ({}):\n{}",
        channels.join(", "),
        lines.join("\n")
    )
}

/// Why the conda package `name` was looked up at all, when `selected`
/// says: the PEP 508 requirement it came from, and what the
/// pypi-to-conda mapping did to its name. `None` for a matchspec-sourced
/// name -- the spec text already says exactly what it is.
fn mapping_note(
    name: &str,
    selected: &[ana_dependency::SelectedRequirement<'_>],
    mapping: &ana_pypi_conda_map::MappingHandle,
) -> Option<String> {
    for requirement in selected {
        let ana_dependency::Dependency::Pep508(requirement) = requirement.dependency else {
            continue;
        };
        let pypi_name = requirement.name.as_str();
        let Ok(mapped) = mapping.get(pypi_name) else {
            continue;
        };
        let Ok(conda_name) = rattler_conda_types::PackageName::from_str(mapped) else {
            continue;
        };
        if conda_name.as_normalized() != name {
            continue;
        }
        return Some(if mapped == pypi_name {
            format!("no pypi-to-conda mapping entry for `{pypi_name}`; the name was searched as-is")
        } else {
            format!("the pypi-to-conda mapping renames `{pypi_name}` to `{mapped}`")
        });
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// `manifest_input` selects an explicit manifest only when both
    /// `--manifest` and `--manifest-type` are set -- the same priority
    /// `exec_in_environment` and `main_sync` give it over a script's own
    /// PEP 723 block or project-directory auto-detection.
    #[test]
    fn manifest_input_is_explicit_file_only_when_both_manifest_fields_are_set() {
        let root = std::path::Path::new("/project");
        let path = std::path::PathBuf::from("/project/deps/requirements-dev.txt");

        let both = cli::ManifestArgs {
            manifest: Some(path.clone()),
            manifest_type: Some(ana_environment::ManifestKind::RequirementsTxt),
        };
        assert!(matches!(
            manifest_input(&both, root),
            Some(RequirementInput::ExplicitFile {
                kind: ana_environment::ManifestKind::RequirementsTxt,
                ..
            })
        ));

        assert!(manifest_input(&cli::ManifestArgs::default(), root).is_none());
    }

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
            sandboxed_channels: None,
            sandbox_policy: String::new(),
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

    /// No `dry_solve_channels` configured at all -- the common case --
    /// means there is nothing to widen `--dry` to.
    #[test]
    fn dry_fallback_channel_policy_is_none_when_dry_solve_channels_is_unset() {
        let config = ana::config::ResolvedConfig {
            default_channels: vec!["defaults".to_string()],
            allowed_channels: None,
            dry_solve_channels: None,
            pypi_to_conda_uri: url::Url::parse(ana_config::DEFAULT_PYPI_TO_CONDA_URI).unwrap(),
            sandboxed_channels: None,
            sandbox_policy: String::new(),
        };

        assert!(build_dry_fallback_channel_policy(&config)
            .unwrap()
            .is_none());
    }

    /// `dry_solve_channels = []` is the same as unset: an empty list is
    /// nothing to widen to, not an authorization of every channel.
    #[test]
    fn dry_fallback_channel_policy_is_none_when_dry_solve_channels_is_empty() {
        let config = ana::config::ResolvedConfig {
            default_channels: vec!["defaults".to_string()],
            allowed_channels: None,
            dry_solve_channels: Some(Vec::new()),
            pypi_to_conda_uri: url::Url::parse(ana_config::DEFAULT_PYPI_TO_CONDA_URI).unwrap(),
            sandboxed_channels: None,
            sandbox_policy: String::new(),
        };

        assert!(build_dry_fallback_channel_policy(&config)
            .unwrap()
            .is_none());
    }

    /// A configured `dry_solve_channels` produces a policy authorizing
    /// both it and `default_channels` -- the fallback widens, it never
    /// replaces.
    #[test]
    fn dry_fallback_channel_policy_authorizes_defaults_and_dry_channels() {
        let config = ana::config::ResolvedConfig {
            default_channels: vec!["conda-forge".to_string()],
            allowed_channels: None,
            dry_solve_channels: Some(vec!["bioconda".to_string()]),
            pypi_to_conda_uri: url::Url::parse(ana_config::DEFAULT_PYPI_TO_CONDA_URI).unwrap(),
            sandboxed_channels: None,
            sandbox_policy: String::new(),
        };

        let policy = build_dry_fallback_channel_policy(&config).unwrap().unwrap();

        let conda_forge: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://conda.anaconda.org/conda-forge/")
                .unwrap()
                .into();
        let bioconda: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://conda.anaconda.org/bioconda/")
                .unwrap()
                .into();
        assert!(policy.authorizes_channel(&conda_forge));
        assert!(policy.authorizes_channel(&bioconda));
    }

    /// [`build_fixed_channel_policy`] authorizes exactly the channels it
    /// is given, regardless of any `config.toml` -- proven here against
    /// [`KILO_CHANNELS`] itself: `main_kilo`'s bootstrap must be able to
    /// fetch both its own package (`akulkarnizzz`) and its dependency's
    /// (`"defaults"`, which expands to `main`), but nothing else.
    #[test]
    fn kilo_channels_authorizes_its_own_channel_and_defaults_but_nothing_else() {
        let policy = build_fixed_channel_policy(KILO_CHANNELS).unwrap();

        let kilo_channel: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://conda.anaconda.org/akulkarnizzz/")
                .unwrap()
                .into();
        assert!(policy.authorizes_channel(&kilo_channel));

        let main_channel: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://repo.anaconda.com/pkgs/main/")
                .unwrap()
                .into();
        assert!(policy.authorizes_channel(&main_channel));

        let unauthorized: rattler_conda_types::ChannelUrl =
            url::Url::parse("https://conda.anaconda.org/conda-forge/")
                .unwrap()
                .into();
        assert!(!policy.authorizes_channel(&unauthorized));
    }

    /// The common case for anyone who hasn't run `ana login`/`anaconda
    /// login`: no `~/.anaconda/keyring` at all, and no diagnostic.
    #[test]
    fn config_and_keyring_diagnostic_is_silent_for_a_missing_keyring_file() {
        let config_dir = tempfile::tempdir().unwrap();
        let keyring_dir = tempfile::tempdir().unwrap();

        let (_, keyring_diagnostic) = config_and_keyring_diagnostic(
            Some(&config_dir.path().join("does-not-exist.toml")),
            Some(&keyring_dir.path().join("does-not-exist")),
        )
        .unwrap();

        assert_eq!(keyring_diagnostic, None);
    }

    /// A keyring file that exists but is corrupt (not the common
    /// missing-file case) still lets `startup`'s fan-out succeed
    /// overall -- private-channel auth being broken must not block work
    /// against public channels -- but surfaces a diagnostic to print.
    #[test]
    fn config_and_keyring_diagnostic_surfaces_a_diagnostic_for_a_corrupt_keyring_file() {
        let config_dir = tempfile::tempdir().unwrap();
        let keyring_dir = tempfile::tempdir().unwrap();
        let keyring_path = keyring_dir.path().join("keyring");
        std::fs::write(&keyring_path, b"not valid json").unwrap();

        let (_, keyring_diagnostic) = config_and_keyring_diagnostic(
            Some(&config_dir.path().join("does-not-exist.toml")),
            Some(&keyring_path),
        )
        .unwrap();

        assert!(keyring_diagnostic.is_some());
    }

    /// A malformed `config.toml` is still a real, fatal error -- only
    /// its *timing* changed (concurrent with the keyring read, not
    /// sequential before it), never its success/failure semantics.
    /// Gated the same way `ana::config`'s own disk-mutating tests are:
    /// a `commercial-config` build ignores disk entirely (see
    /// `ana::config::raw_config`), so this scenario cannot occur there.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_and_keyring_diagnostic_still_fails_on_a_malformed_config() {
        let config_dir = tempfile::tempdir().unwrap();
        let keyring_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(&config_path, b"not valid toml [[[").unwrap();

        let result = config_and_keyring_diagnostic(
            Some(&config_path),
            Some(&keyring_dir.path().join("does-not-exist")),
        );

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn kilo_config_dir_is_restricted_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("kilo");

        ensure_kilo_config_dir(&dir).unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// A directory left over from before this restriction existed (or
    /// created under a permissive umask) is self-healed to owner-only on
    /// the next call, not just at first creation.
    #[cfg(unix)]
    #[test]
    fn kilo_config_dir_tightens_pre_existing_permissive_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("kilo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_kilo_config_dir(&dir).unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    /// The managed config names the terminal-space remote MCP server and
    /// grants the `code` and `ask` agents access to its tools.
    #[test]
    fn kilo_config_json_provisions_the_terminal_space_mcp_server() {
        let skills_dir = tempfile::tempdir().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&kilo_config_json(skills_dir.path())).unwrap();

        assert_eq!(
            parsed["mcp"]["terminal-space"]["url"],
            "https://repo.terminal.space/api/mcp"
        );
        assert_eq!(parsed["mcp"]["terminal-space"]["type"], "remote");
        assert_eq!(
            parsed["agent"]["code"]["permission"]["terminal-space_*"],
            "allow"
        );
        assert_eq!(
            parsed["agent"]["ask"]["permission"]["terminal-space_*"],
            "allow"
        );
    }

    /// The managed config denies every agent from running `uv`, `pip`,
    /// `conda`, or `pixi` directly, since those bypass `ana`'s own
    /// dependency resolution and lockfile.
    #[test]
    fn kilo_config_json_denies_direct_package_manager_invocations() {
        let skills_dir = tempfile::tempdir().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&kilo_config_json(skills_dir.path())).unwrap();

        for tool in ["uv", "pip", "conda", "pixi"] {
            assert_eq!(
                parsed["permission"]["bash"][format!("{tool} *")],
                "deny",
                "expected {tool} to be denied"
            );
        }
    }

    /// `skills.paths` names exactly the directory passed in, so Kilo
    /// discovers [`SKILLS`] there regardless of the OS-specific config
    /// directory it's actually nested under.
    #[test]
    fn kilo_config_json_names_the_given_skills_dir() {
        let skills_dir = tempfile::tempdir().unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&kilo_config_json(skills_dir.path())).unwrap();

        assert_eq!(
            parsed["skills"]["paths"],
            serde_json::json!([skills_dir.path().to_string_lossy()])
        );
    }

    #[test]
    fn kilo_config_file_is_written_into_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");

        ensure_kilo_config_file(dir.path(), &skills_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("kilo.json")).unwrap(),
            kilo_config_json(&skills_dir)
        );
    }

    /// A stale or hand-edited `kilo.json` is rewritten with the managed
    /// content on the next call.
    #[test]
    fn kilo_config_file_overwrites_drifted_content() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::write(dir.path().join("kilo.json"), "{}").unwrap();

        ensure_kilo_config_file(dir.path(), &skills_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("kilo.json")).unwrap(),
            kilo_config_json(&skills_dir)
        );
    }

    /// An already up-to-date file is left untouched -- a read-only file
    /// makes any write attempt fail the call.
    #[cfg(unix)]
    #[test]
    fn kilo_config_file_leaves_up_to_date_content_untouched() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let path = dir.path().join("kilo.json");
        std::fs::write(&path, kilo_config_json(&skills_dir)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

        ensure_kilo_config_file(dir.path(), &skills_dir).unwrap();
    }

    /// Every skill in [`SKILLS`] lands at `skills_dir/<name>/SKILL.md`,
    /// and its frontmatter's own `name:` field matches -- Kilo's own
    /// naming rule requires the two to agree.
    #[test]
    fn ensure_kilo_skill_files_writes_every_skill_with_a_matching_name() {
        let dir = tempfile::tempdir().unwrap();

        ensure_kilo_skill_files(dir.path()).unwrap();

        for skill in SKILLS {
            let content = std::fs::read_to_string(dir.path().join(skill.name).join("SKILL.md"))
                .unwrap_or_else(|err| panic!("{}: {err}", skill.name));
            assert!(
                content
                    .lines()
                    .any(|line| line == format!("name: {}", skill.name)),
                "{}'s SKILL.md frontmatter must declare `name: {}`",
                skill.name,
                skill.name
            );
        }
    }

    /// A stale or hand-edited `SKILL.md` self-heals on the next call,
    /// mirroring [`ensure_kilo_config_file`]'s own idempotence.
    #[test]
    fn ensure_kilo_skill_files_overwrites_drifted_content() {
        let dir = tempfile::tempdir().unwrap();
        let first_skill_dir = dir.path().join(SKILLS[0].name);
        std::fs::create_dir_all(&first_skill_dir).unwrap();
        std::fs::write(first_skill_dir.join("SKILL.md"), "stale").unwrap();

        ensure_kilo_skill_files(dir.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(first_skill_dir.join("SKILL.md")).unwrap(),
            SKILLS[0].skill_md
        );
    }

    /// [`kilo_script_assist_prompt`] names the script by its absolute
    /// path and points at every skill in [`SKILLS`], so the session's
    /// first turn already knows what to load and for which file.
    #[test]
    fn kilo_script_assist_prompt_names_the_script_and_every_skill() {
        let path = Path::new("/tmp/example/hello.py");
        let prompt = kilo_script_assist_prompt(path);

        assert!(prompt.contains("/tmp/example/hello.py"));
        for skill in SKILLS {
            assert!(
                prompt.contains(skill.name),
                "prompt must mention the `{}` skill",
                skill.name
            );
        }
    }

    /// Interactive `kilo run` has no way to exit itself once its turn
    /// ends, so the prompt must tell the session to instruct the user
    /// to press `Ctrl-C` -- otherwise `ana` is left waiting on a
    /// session that will sit idle forever with no indication why.
    #[test]
    fn kilo_script_assist_prompt_tells_the_session_to_ask_for_ctrl_c() {
        let prompt = kilo_script_assist_prompt(Path::new("/tmp/example/hello.py"));

        assert!(prompt.contains("Ctrl-C"));
    }

    /// `Interactive` must pass `--interactive`, not rely on `kilo run`'s
    /// own default -- its default is a one-shot streaming mode that
    /// can't prompt for approval at all and auto-rejects every
    /// permission request, which would silently defeat the entire
    /// point of running interactively.
    #[test]
    fn script_assist_kilo_flags_interactive_passes_the_interactive_flag() {
        assert_eq!(
            script_assist_kilo_flags(ana::ScriptAssistMode::Interactive),
            vec!["--interactive".to_string()]
        );
    }

    #[test]
    fn script_assist_kilo_flags_headless_passes_auto() {
        assert_eq!(
            script_assist_kilo_flags(ana::ScriptAssistMode::Headless),
            vec!["--auto".to_string()]
        );
    }

    /// Never actually reached (see `detect_script_or_assist`), but must
    /// not accidentally pass a flag that would change behavior if it
    /// somehow were.
    #[test]
    fn script_assist_kilo_flags_off_passes_nothing() {
        assert_eq!(
            script_assist_kilo_flags(ana::ScriptAssistMode::Off),
            Vec::<String>::new()
        );
    }

    /// [`kilo_env_vars`] always sets `KILO_CONFIG_DIR` to `config_dir`
    /// itself and `KILO_DB` to a `kilo.db` file inside it, regardless of
    /// whether a real Kilo auth store exists to also share.
    #[test]
    fn kilo_env_vars_always_points_config_dir_and_db_inside_the_given_directory() {
        let config_dir = tempfile::tempdir().unwrap();

        let vars = kilo_env_vars(config_dir.path(), None);

        assert_eq!(
            vars.iter().find(|(key, _)| *key == "KILO_CONFIG_DIR"),
            Some(&("KILO_CONFIG_DIR", config_dir.path().as_os_str().to_owned()))
        );
        assert_eq!(
            vars.iter().find(|(key, _)| *key == "KILO_DB"),
            Some(&(
                "KILO_DB",
                config_dir.path().join("kilo.db").into_os_string()
            ))
        );
    }

    /// No `auth.json` under the data dir: `KILO_AUTH_CONTENT` is omitted
    /// entirely rather than passed as an empty placeholder.
    #[test]
    fn kilo_env_vars_omits_auth_content_without_an_existing_auth_file() {
        let data_dir = tempfile::tempdir().unwrap();
        let config_dir = tempfile::tempdir().unwrap();

        let auth_content = real_kilo_auth_content_under(data_dir.path());
        let vars = kilo_env_vars(config_dir.path(), auth_content.as_deref());

        assert_eq!(auth_content, None);
        assert!(!vars.iter().any(|(key, _)| *key == "KILO_AUTH_CONTENT"));
    }

    /// When the user already has a real Kilo auth store, its contents
    /// are handed to the child process verbatim via `KILO_AUTH_CONTENT`
    /// -- byte-for-byte, since Kilo (not `ana`) owns that format.
    #[test]
    fn kilo_env_vars_shares_an_existing_auth_files_contents_verbatim() {
        let data_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data_dir.path().join("kilo")).unwrap();
        std::fs::write(
            data_dir.path().join("kilo").join("auth.json"),
            r#"{"token":"secret"}"#,
        )
        .unwrap();
        let config_dir = tempfile::tempdir().unwrap();

        let auth_content = real_kilo_auth_content_under(data_dir.path());
        let vars = kilo_env_vars(config_dir.path(), auth_content.as_deref());

        assert_eq!(
            vars.iter().find(|(key, _)| *key == "KILO_AUTH_CONTENT"),
            Some(&("KILO_AUTH_CONTENT", OsString::from(r#"{"token":"secret"}"#)))
        );
    }

    fn mapping(entries: &[(&str, &str)]) -> ana_pypi_conda_map::MappingHandle {
        ana_pypi_conda_map::MappingHandle::from_map(
            entries
                .iter()
                .map(|(pypi, conda)| (pypi.to_string(), conda.to_string()))
                .collect(),
        )
    }

    fn deps(specs: &[&str]) -> Vec<ana_dependency::Dependency> {
        specs
            .iter()
            .map(|spec| ana_dependency::parse_specifier(spec).unwrap())
            .collect()
    }

    fn selected(
        deps: &[ana_dependency::Dependency],
    ) -> Vec<ana_dependency::SelectedRequirement<'_>> {
        deps.iter()
            .map(|dependency| ana_dependency::SelectedRequirement {
                dependency,
                source: "runtime".to_string(),
            })
            .collect()
    }

    #[test]
    fn diagnose_unsolvable_annotates_each_specs_mapping_decision() {
        let deps = deps(&["mirascope", "duckdb", "::numpy"]);
        let selected = selected(&deps);
        let missing = vec![
            ana_solver::MissingSpec {
                name: "mirascope".to_string(),
                spec: "mirascope *".to_string(),
            },
            ana_solver::MissingSpec {
                name: "python-duckdb".to_string(),
                spec: "python-duckdb *".to_string(),
            },
            ana_solver::MissingSpec {
                name: "numpy".to_string(),
                spec: "numpy *".to_string(),
            },
        ];
        let channels = vec!["https://repo.anaconda.com/pkgs/main/".to_string()];

        let message = diagnose_unsolvable(
            &missing,
            &channels,
            &selected,
            &mapping(&[("duckdb", "python-duckdb")]),
        );

        assert!(
            message.contains(
                "mirascope * -- no pypi-to-conda mapping entry for `mirascope`; the name was searched as-is"
            ),
            "{message}"
        );
        assert!(
            message.contains(
                "python-duckdb * -- the pypi-to-conda mapping renames `duckdb` to `python-duckdb`"
            ),
            "{message}"
        );
        // A matchspec-sourced name gets no mapping note.
        assert!(message.contains("  numpy *"), "{message}");
        assert!(!message.contains("numpy * --"), "{message}");
        assert!(
            message.contains("https://repo.anaconda.com/pkgs/main/"),
            "{message}"
        );
    }

    #[test]
    fn diagnose_solve_error_routes_a_classified_unsolvable() {
        let deps = deps(&["mirascope"]);
        let selected = selected(&deps);
        let err = ana::Error::Lockfile(ana_lockfile::Error::Solve {
            platform: Platform::Linux64,
            source: Box::new(ana_solver::Error::Unsolvable {
                missing: vec![ana_solver::MissingSpec {
                    name: "mirascope".to_string(),
                    spec: "mirascope *".to_string(),
                }],
                channels: vec!["https://repo.anaconda.com/pkgs/main/".to_string()],
                source: rattler_solve::SolveError::Unsolvable(vec![]),
            }),
        });

        // `diagnose_solve_error` takes an `Environment`, so exercise the
        // downcast routing through the pieces it delegates to instead of
        // building a whole project on disk.
        let ana::Error::Lockfile(ana_lockfile::Error::Solve { source, .. }) = &err else {
            panic!("expected a solve error");
        };
        let Some(ana_solver::Error::Unsolvable {
            missing, channels, ..
        }) = source.downcast_ref::<ana_solver::Error>()
        else {
            panic!("expected the classified unsolvable variant");
        };
        let message = diagnose_unsolvable(missing, channels, &selected, &mapping(&[]));
        assert!(
            message.contains("no pypi-to-conda mapping entry for `mirascope`"),
            "{message}"
        );
    }

    #[test]
    fn diagnose_solve_error_passes_other_errors_through() {
        let err = ana::Error::NoConfigDir;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\ndependencies = []\n",
        )
        .unwrap();
        let env = ana_environment::resolve(&EnvironmentRequest {
            input: RequirementInput::ProjectDir { dir: dir.path() },
            groups: &[],
            extra: &[],
            platform: Platform::current(),
            pypi_to_conda_map: &mapping(&[]),
            global_cache_root: dir.path(),
        })
        .unwrap();

        assert!(diagnose_solve_error(&err, &env, &mapping(&[])).is_none());
    }
}
