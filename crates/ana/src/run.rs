//! The `ana run` flow: [`run_command`] brings an already-resolved
//! environment's lock up to date and materializes it, [`exec`] runs the
//! command inside it. [`NoSolver`] is a solver-free `Solver` stand-in
//! for tests; `ana-solver`'s `RattlerSolver` is the real implementation.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ana_environment::Environment;
use ana_installer::{Downloader, ReconcileMode};
use ana_lockfile::{
    acquire_environment_lock, ensure_current_platform_locked, read_lock_section, EnsureOutcome,
    EnvLock, SolveRequest, SolveScope, Solver,
};
use rattler::install::{InstallationResultRecord, Transaction};
use rattler_conda_types::{Platform, RepoDataRecord};

use crate::Error;

/// What a successful [`run_command`] did, and everything [`exec`] needs
/// to actually run the command.
#[derive(Debug)]
pub struct RunOutcome {
    /// What bringing `ana.lock`'s section up to date did, for the caller
    /// to report.
    pub ensure: EnsureOutcome,
    /// The reconcile's resulting [`Transaction`], if one ran at all --
    /// `None` means the section's packages already matched the env
    /// lock's.
    pub install: Option<Box<Transaction<InstallationResultRecord, RepoDataRecord>>>,
    /// The environment's prefix -- [`exec`] resolves the command's `PATH`
    /// from this.
    pub env_path: PathBuf,
    /// The command to run inside the environment, verbatim.
    pub command: Vec<String>,
    /// The current platform's now-current locked packages -- what's
    /// actually installed at `env_path`.
    pub packages: Vec<RepoDataRecord>,
}

/// `ana run [--group <name>]... [--frozen] <command>...`, given `env`
/// (already resolved by the caller -- see `ana_environment::resolve`).
///
/// Brings `ana.lock`'s section for the current platform up to date, then
/// -- only if needed -- reconciles the environment against it, all under
/// one continuously held advisory lock, released before [`exec`] is ever
/// called. `frozen` fails a stale (or missing) lock section instead of
/// solving and writing it.
pub fn run_command(
    env: &Environment,
    scope: &SolveScope<'_>,
    command: &[String],
    frozen: bool,
    solver: &dyn Solver,
    runtime: &tokio::runtime::Handle,
    downloader: &Downloader,
) -> Result<RunOutcome, Error> {
    let paths = env.paths();
    let platform = Platform::current();

    let mut lock = acquire_environment_lock(paths)?;
    let guard = lock.acquire().map_err(|source| Error::Lock {
        path: paths.advisory_lock_path(),
        source,
    })?;

    // With `frozen`, a stale section errors here instead of being solved
    // and spliced in.
    let ensure = ensure_current_platform_locked(&guard, env, platform, scope, solver, frozen)?;

    let mut section = read_lock_section(&paths.lock_path, platform)?
        .ok_or(Error::MissingPlatformSection { platform })?;
    section.canonicalize();

    // Read fresh here (rather than threaded through `ensure`'s return
    // value) so this always reflects reality even after a dirty wipe.
    let env_lock_path = paths.env_lock_path();
    let env_lock = EnvLock::read(&env_lock_path, platform);
    let mut previous = env_lock.section.unwrap_or_default();
    previous.canonicalize();

    let install = if section.packages == previous.packages {
        None
    } else {
        // Mark dirty *before* the real install starts, and propagate a
        // write failure: without it landing, a crash mid-install is
        // indistinguishable from "never started."
        EnvLock::write(&env_lock_path, platform, true, None)?;

        let desired = section.packages.clone();
        let transaction = runtime.block_on(ana_installer::reconcile(
            &guard,
            downloader,
            paths,
            platform,
            desired,
            ReconcileMode::Inexact,
        ))?;

        // Best-effort: a lost write here only costs one extra
        // dirty-triggered wipe on the next invocation, not correctness.
        let _ = EnvLock::write(&env_lock_path, platform, false, Some(&section));

        Some(transaction)
    };

    Ok(RunOutcome {
        ensure,
        install,
        env_path: paths.env_path.clone(),
        command: command.to_vec(),
        packages: section.packages,
    })
}

/// Actually run `outcome.command` inside `outcome.env_path`: prepend the
/// environment's executable directory (directories, on Windows) to
/// `PATH`, apply `extra_env` on top of that, then either `exec` (Unix --
/// replaces this process image, preserving signal/exit-code behavior) or
/// spawn+wait+[`std::process::exit`] (Windows, which has no `exec`
/// syscall equivalent). No activation script is run -- a `PATH`-prepend
/// is the minimum needed to make `ana run python ...` find the installed
/// interpreter.
///
/// Never returns on success, on any platform -- the return type exists
/// only for the failure path (`command[0]` couldn't even be started).
pub fn exec(outcome: &RunOutcome, extra_env: &[(&str, OsString)]) -> Error {
    let path = prepend_env_path(&outcome.env_path);
    let Some((program, args)) = outcome.command.split_first() else {
        // `cli::parse`'s `required = true` on the command already
        // prevents this in the real binary; handled rather than
        // indexed-panicked for any future caller that doesn't go
        // through the CLI.
        return Error::Exec {
            command: outcome.command.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty command"),
        };
    };
    exec_replacing_with(program, args, &outcome.command, |command| {
        command.env("PATH", &path);
        for (key, value) in extra_env {
            command.env(key, value);
        }
    })
}

/// The environment variables a normal program invocation still needs to
/// find its shell, locale, and home directory, repopulated by
/// [`exec_program_with_clean_env`].
#[cfg(unix)]
const IMPORTANT_ENV_VARS: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TERM",
    "SHELL",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
];

/// Windows counterpart to [`IMPORTANT_ENV_VARS`].
#[cfg(not(unix))]
const IMPORTANT_ENV_VARS: &[&str] = &[
    "SystemRoot",
    "SystemDrive",
    "USERPROFILE",
    "USERNAME",
    "APPDATA",
    "LOCALAPPDATA",
    "ComSpec",
    "PATHEXT",
];

/// Exec `program` with `args` in a minimal environment: every inherited
/// variable is dropped ([`std::process::Command::env_clear`]) and
/// replaced by `PATH` (`path`), [`IMPORTANT_ENV_VARS`], and `extra_env`.
/// A sandboxed run's `nono` invocation goes through here so the
/// sandboxed child never sees secrets sitting in the parent shell's
/// environment. `command` is only used to name the failing command in
/// the returned [`Error::Exec`].
pub fn exec_program_with_clean_env(
    program: &str,
    args: &[String],
    path: &OsString,
    extra_env: &BTreeMap<String, String>,
    command: &[String],
) -> Error {
    exec_replacing_with(program, args, command, |cmd| {
        cmd.env_clear();
        cmd.env("PATH", path);
        for name in IMPORTANT_ENV_VARS {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
    })
}

/// Builds `program`/`args` into a [`std::process::Command`], applies
/// `configure_env`, then execs it: `CommandExt::exec` (Unix -- replaces
/// this process image, preserving signal/exit-code behavior) or
/// spawn+wait+[`std::process::exit`] (Windows, which has no `exec`
/// syscall equivalent). Never returns on success, on any platform.
fn exec_replacing_with(
    program: &str,
    args: &[String],
    command: &[String],
    configure_env: impl FnOnce(&mut std::process::Command),
) -> Error {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    configure_env(&mut cmd);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `CommandExt::exec` only ever returns on failure -- success
        // replaces this process image and never comes back here at all.
        let source = cmd.exec();
        Error::Exec {
            command: command.to_vec(),
            source,
        }
    }

    #[cfg(not(unix))]
    {
        // Windows has no `exec` syscall equivalent: spawn, wait, and exit
        // this process with the child's own exit code instead.
        match cmd.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(source) => Error::Exec {
                command: command.to_vec(),
                source,
            },
        }
    }
}

/// `env_path`'s executable directory (directories, on Windows), prepended
/// to the current process's own `PATH`.
fn prepend_env_path(env_path: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> = crate::sandbox::env_bin_dirs(env_path);
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// A solver-free [`Solver`] stand-in for tests: any invocation that
/// actually needs a solve fails explicitly, rather than silently.
pub struct NoSolver;

impl Solver for NoSolver {
    fn solve(
        &self,
        _request: SolveRequest,
    ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
        Err(Box::new(SolveNotImplemented))
    }
}

/// [`NoSolver`]'s error.
#[derive(Debug, thiserror::Error)]
#[error(
    "regenerating the lock requires a solver, and no solver is wired into ana yet \
     (this invocation used NoSolver -- see ana-solver for the real implementation)"
)]
struct SolveNotImplemented;

/// Render a command the way a user could paste it back into a shell:
/// arguments joined with spaces, any argument containing shell-significant
/// characters single-quoted. Display-only, nothing here is executed.
pub(crate) fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote `arg` for [`shell_join`] if it isn't already a bare-safe word.
fn shell_quote(arg: &str) -> String {
    let bare = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._/:=@+%".contains(c));
    if bare {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};

    use ana_channels::ChannelPolicy;
    use ana_environment::{EnvironmentRequest, RequirementInput};
    use ana_lockfile::SolveRequest;
    use ana_pypi_conda_map::MappingHandle;
    use async_trait::async_trait;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{NoArchType, PackageName, PackageRecord, Version};
    use reqwest_middleware::{Middleware, Next};
    use uv_normalize::GroupName;

    use super::*;

    /// An empty mapping table, for tests that don't exercise name
    /// mapping.
    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    /// The channel every test in this module uses by default, unless it
    /// deliberately exercises a custom one. Not a real host: it has to
    /// at least look like one, though -- `ana_lockfile::channels`
    /// categorically rejects any channel resolving to a `file://` base
    /// url.
    const FIXTURE_ORIGIN: &str = "https://ana-test-fixture.internal/fixtures";

    fn test_channels() -> Vec<String> {
        vec![FIXTURE_ORIGIN.to_string()]
    }

    /// The fixture record's fetch URL, in the conventional
    /// `<channel>/<subdir>/<filename>` layout.
    fn fixture_url() -> String {
        format!("{FIXTURE_ORIGIN}/noarch/{FIXTURE_FILE_NAME}")
    }

    /// Answers any request for `fixture_url()` from the local fixture
    /// archive, so a fully offline test can still exercise
    /// `Downloader`'s real client/retry/`Installer` wiring end to end.
    struct FixtureMiddleware;

    #[async_trait]
    impl Middleware for FixtureMiddleware {
        async fn handle(
            &self,
            req: reqwest::Request,
            extensions: &mut http::Extensions,
            next: Next<'_>,
        ) -> reqwest_middleware::Result<reqwest::Response> {
            if req.url().as_str() == fixture_url() {
                let body = fs::read(fixture_path()).unwrap();
                let response = http::Response::builder().status(200).body(body).unwrap();
                Ok(reqwest::Response::from(response))
            } else {
                next.run(req, extensions).await
            }
        }
    }

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
dependencies = ["requests"]

[dependency-groups]
dev = ["ruff"]
"#;

    /// The same tiny, real, BSD-3-Clause fixture archive
    /// `ana-installer`'s own tests use (see that crate's
    /// `tests/fixtures/README.md` for provenance) -- copied here rather
    /// than referenced across crates, so this test module has no path
    /// coupling to another crate's `CARGO_MANIFEST_DIR`.
    const FIXTURE_FILE_NAME: &str = "empty-0.1.0-h4616a5c_0.conda";
    const FIXTURE_SHA256: &str = "af8000ad3ad6af83b294b0e700f7c6f17fa85c6b9db08207813f47af8a94d52c";
    const FIXTURE_SIZE: u64 = 1538;

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/packages")
            .join(FIXTURE_FILE_NAME)
    }

    fn hex_bytes(hex: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    /// The record every test's solver returns, regardless of which spec
    /// was requested -- `run.rs`'s tests only need to prove the
    /// lock/ensure/reconcile/exec pipeline works end to end.
    fn fixture_record() -> RepoDataRecord {
        let mut package_record = PackageRecord::new(
            PackageName::new_unchecked("empty"),
            Version::from_str("0.1.0").unwrap(),
            "h4616a5c_0".to_string(),
        );
        package_record.subdir = "noarch".to_string();
        package_record.noarch = NoArchType::generic();
        package_record.sha256 = Some(hex_bytes(FIXTURE_SHA256).into());
        package_record.size = Some(FIXTURE_SIZE);
        let identifier = DistArchiveIdentifier::try_from_filename(FIXTURE_FILE_NAME).unwrap();
        let url = url::Url::parse(&fixture_url()).unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url,
            channel: None,
        }
    }

    /// Resolves every spec to [`fixture_record`], so a real `reconcile`
    /// call has something genuinely installable.
    struct FakeSolver;

    impl Solver for FakeSolver {
        fn solve(
            &self,
            _request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![fixture_record()])
        }
    }

    /// Records the `channels` every `solve` call was made with, as their
    /// canonical base-url strings (in order).
    struct ChannelRecordingSolver {
        seen: Mutex<Vec<Vec<String>>>,
    }

    impl ChannelRecordingSolver {
        fn new() -> Self {
            Self {
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Solver for ChannelRecordingSolver {
        fn solve(
            &self,
            request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            self.seen.lock().unwrap().push(
                request
                    .channels
                    .iter()
                    .map(|channel| channel.base_url.as_str().to_string())
                    .collect(),
            );
            Ok(vec![fixture_record()])
        }
    }

    /// A fresh runtime + downloader per test, rooted at its own temp
    /// cache dir -- never shares cache state with another test or with
    /// a real `~/.cache/rattler`.
    struct Env {
        _cache: tempfile::TempDir,
        cache_root: tempfile::TempDir,
        runtime: tokio::runtime::Runtime,
        downloader: Downloader,
    }

    impl Env {
        fn new() -> Self {
            let cache = tempfile::tempdir().unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let downloader =
                Downloader::for_testing(cache.path(), None, Some(Arc::new(FixtureMiddleware)))
                    .unwrap();
            Self {
                _cache: cache,
                cache_root: tempfile::tempdir().unwrap(),
                runtime,
                downloader,
            }
        }

        fn run(
            &self,
            dir: &Path,
            groups: &[GroupName],
            command: &[String],
            solver: &dyn Solver,
        ) -> Result<RunOutcome, Error> {
            self.run_with(dir, groups, command, false, solver)
        }

        fn run_with(
            &self,
            dir: &Path,
            groups: &[GroupName],
            command: &[String],
            frozen: bool,
            solver: &dyn Solver,
        ) -> Result<RunOutcome, Error> {
            self.run_with_channels(dir, groups, command, frozen, &test_channels(), solver)
        }

        fn run_with_channels(
            &self,
            dir: &Path,
            groups: &[GroupName],
            command: &[String],
            frozen: bool,
            channels: &[String],
            solver: &dyn Solver,
        ) -> Result<RunOutcome, Error> {
            let map = no_mapping();
            let env = ana_environment::resolve(&EnvironmentRequest {
                input: RequirementInput::ProjectDir { dir },
                groups,
                extra: &[],
                platform: Platform::current(),
                pypi_to_conda_map: &map,
                global_cache_root: self.cache_root.path(),
            })?;
            let policy = ChannelPolicy::new(channels, &[]).unwrap();
            let scope = SolveScope {
                channels: &policy,
                pypi_to_conda_map: &map,
            };
            run_command(
                &env,
                &scope,
                command,
                frozen,
                solver,
                self.runtime.handle(),
                &self.downloader,
            )
        }
    }

    fn project_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), PYPROJECT).unwrap();
        dir
    }

    #[test]
    fn custom_channels_are_passed_through_to_the_solver() {
        let dir = project_root();
        let env = Env::new();
        let solver = ChannelRecordingSolver::new();
        let custom_channels = vec!["conda-forge".to_string()];

        env.run_with_channels(
            dir.path(),
            &[],
            &["true".to_string()],
            false,
            &custom_channels,
            &solver,
        )
        .unwrap();

        assert_eq!(
            solver.seen.lock().unwrap().as_slice(),
            [vec!["https://conda.anaconda.org/conda-forge/".to_string()]],
            "run_command must solve with whatever channel list its caller passes"
        );
    }

    #[test]
    fn fresh_lock_resolves_and_installs_for_real() {
        let dir = project_root();
        let env = Env::new();
        let command = vec!["python".to_string(), "--version".to_string()];

        let first = env.run(dir.path(), &[], &command, &FakeSolver).unwrap();
        assert_eq!(first.ensure, EnsureOutcome::Resolved);
        assert!(
            first.install.is_some(),
            "a fresh lock must trigger a real install"
        );
        assert_eq!(first.command, command);
        assert!(dir.path().join("ana.lock").exists());
        assert!(first
            .env_path
            .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
            .exists());
        assert_eq!(first.packages, vec![fixture_record()]);

        // Second run hits both short-circuits: no re-solve and no
        // re-install.
        let second = env.run(dir.path(), &[], &command, &FakeSolver).unwrap();
        assert_eq!(second.ensure, EnsureOutcome::Fresh);
        assert!(
            second.install.is_none(),
            "nothing changed since the first install, so reconcile must not even be called"
        );
    }

    /// End-to-end proof that a hand-edited `ana.lock` pointing a locked
    /// package's `url` at an unauthorized location is discarded and
    /// re-solved rather than trusted: after a genuine solve/install,
    /// the installed package's `url` is swapped to a different,
    /// `file://` location this test controls (which
    /// `ana_lockfile::channels` never authorizes) -- `run_command` must
    /// refuse to reconcile from it.
    #[test]
    fn hand_edited_lock_pointing_at_an_unauthorized_location_is_discarded_and_re_solved() {
        let dir = project_root();
        let env = Env::new();
        let command = vec!["true".to_string()];

        let first = env.run(dir.path(), &[], &command, &FakeSolver).unwrap();
        assert_eq!(first.ensure, EnsureOutcome::Resolved);
        assert!(first.install.is_some());

        // A byte-identical copy at a `file://` path standing in for an
        // attacker's own host: `ana_lockfile::channels` categorically
        // disallows the `file://` scheme, regardless of origin.
        let attacker_file = dir
            .path()
            .join("attacker-hosted-empty-0.1.0-h4616a5c_0.conda");
        fs::copy(fixture_path(), &attacker_file).unwrap();
        let original_url = fixture_url();
        let attacker_url = url::Url::from_file_path(&attacker_file)
            .unwrap()
            .to_string();

        let lock_path = dir.path().join("ana.lock");
        let lock_text = fs::read_to_string(&lock_path).unwrap();
        assert!(
            lock_text.contains(&original_url),
            "sanity check: the fixture's own url is in the freshly written lock"
        );
        let edited = lock_text.replace(&original_url, &attacker_url);
        assert_ne!(
            edited, lock_text,
            "the substitution must actually change something"
        );
        fs::write(&lock_path, edited).unwrap();

        let second = env.run(dir.path(), &[], &command, &FakeSolver).unwrap();

        assert_eq!(
            second.ensure,
            EnsureOutcome::Resolved,
            "a locked package whose url was swapped to an unauthorized location \
             must never be trusted as Fresh -- it must be discarded and re-solved"
        );
        // The re-solve reproduces the same, legitimate record as the
        // first run, which is already installed.
        assert!(
            second.install.is_none(),
            "the re-solved section already matches what's installed: {:?}",
            second.install
        );
        let lock_text_after = fs::read_to_string(&lock_path).unwrap();
        assert!(
            lock_text_after.contains(&original_url),
            "the re-solve must land the legitimate url back in ana.lock: {lock_text_after}"
        );
        assert!(
            !lock_text_after.contains(&attacker_url),
            "the attacker's url must not survive: {lock_text_after}"
        );
    }

    /// Same scenario, under `--frozen`: since a frozen run may never
    /// re-solve, the tampered lock must be rejected outright rather
    /// than silently self-healed.
    #[test]
    fn hand_edited_lock_pointing_at_an_unauthorized_location_is_rejected_under_frozen() {
        let dir = project_root();
        let env = Env::new();
        let command = vec!["true".to_string()];

        env.run(dir.path(), &[], &command, &FakeSolver).unwrap();

        let attacker_file = dir
            .path()
            .join("attacker-hosted-empty-0.1.0-h4616a5c_0.conda");
        fs::copy(fixture_path(), &attacker_file).unwrap();
        let original_url = fixture_url();
        let attacker_url = url::Url::from_file_path(&attacker_file)
            .unwrap()
            .to_string();

        let lock_path = dir.path().join("ana.lock");
        let lock_text = fs::read_to_string(&lock_path).unwrap();
        fs::write(&lock_path, lock_text.replace(&original_url, &attacker_url)).unwrap();

        let result = env.run_with(dir.path(), &[], &command, true, &FakeSolver);

        assert!(
            matches!(
                result,
                Err(Error::Lockfile(ana_lockfile::Error::Channels(
                    ana_channels::Error::ChannelNotAllowed(_)
                )))
            ),
            "{result:?}"
        );
    }

    #[test]
    fn group_selection_uses_hashed_paths() {
        let dir = project_root();
        let env = Env::new();
        let groups = vec![GroupName::from_str("dev").unwrap()];
        let outcome = env
            .run(dir.path(), &groups, &["ruff".to_string()], &FakeSolver)
            .unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Resolved);
        assert!(dir.path().join(".ana/ef260e9a/ana.lock").exists());
        assert!(!dir.path().join("ana.lock").exists());
        assert_eq!(
            outcome.env_path,
            dir.path().join(".ana/ef260e9a/env"),
            "the group environment's own env_path, not the default one"
        );
    }

    #[test]
    fn no_solver_errors_only_when_a_solve_is_needed() {
        let dir = project_root();
        let env = Env::new();
        let command = vec!["python".to_string()];
        let err = env.run(dir.path(), &[], &command, &NoSolver).unwrap_err();
        assert!(err.to_string().contains("no solver is wired into ana yet"));

        env.run(dir.path(), &[], &command, &FakeSolver).unwrap();
        let outcome = env.run(dir.path(), &[], &command, &NoSolver).unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Fresh);
        assert!(outcome.install.is_none());
    }

    #[test]
    fn missing_project_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::new();
        assert!(matches!(
            env.run(dir.path(), &[], &["true".to_string()], &FakeSolver),
            Err(Error::Environment(
                ana_environment::Error::NoProjectFile { .. }
            ))
        ));
    }

    #[test]
    fn unknown_group_is_an_error() {
        let dir = project_root();
        let env = Env::new();
        let groups = vec![GroupName::from_str("nope").unwrap()];
        assert!(matches!(
            env.run(dir.path(), &groups, &["true".to_string()], &FakeSolver),
            Err(Error::Environment(ana_environment::Error::Groups(
                ana_requirements::Error::UnknownGroup(name)
            ))) if name == "nope"
        ));
    }

    #[test]
    fn frozen_stale_lock_is_an_error() {
        let dir = project_root();
        let env = Env::new();
        let command = vec!["true".to_string()];

        let err = env
            .run_with(dir.path(), &[], &command, true, &FakeSolver)
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Lockfile(ana_lockfile::Error::Frozen { .. })
        ));
        assert!(!dir.path().join("ana.lock").exists());
    }

    #[test]
    fn frozen_fresh_lock_still_runs() {
        let dir = project_root();
        let env = Env::new();
        let command = vec!["true".to_string()];

        env.run(dir.path(), &[], &command, &FakeSolver).unwrap();

        let outcome = env
            .run_with(dir.path(), &[], &command, true, &FakeSolver)
            .unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Fresh);
    }

    #[test]
    fn shell_join_quotes_only_when_needed() {
        assert_eq!(shell_join(&["python".to_string()]), "python");
        assert_eq!(
            shell_join(&[
                "python".to_string(),
                "-c".to_string(),
                "print(\"hi\")".to_string(),
            ]),
            "python -c 'print(\"hi\")'"
        );
        assert_eq!(
            shell_join(&["echo".to_string(), "it's".to_string()]),
            "echo 'it'\\''s'"
        );
        assert_eq!(shell_join(&[String::new()]), "''");
    }

    /// `run_command` must not consult the solver on a stage-1 hit even
    /// when the lock exists but the environment was never materialized
    /// (the scaffold's steady state).
    #[test]
    fn fresh_check_never_touches_solver() {
        struct CountingSolver(Mutex<u32>);
        impl Solver for CountingSolver {
            fn solve(
                &self,
                _request: SolveRequest,
            ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
                *self.0.lock().unwrap() += 1;
                Ok(vec![fixture_record()])
            }
        }

        let dir = project_root();
        let env = Env::new();
        let solver = CountingSolver(Mutex::new(0));
        let command = vec!["true".to_string()];
        env.run(dir.path(), &[], &command, &solver).unwrap();
        env.run(dir.path(), &[], &command, &solver).unwrap();
        assert_eq!(*solver.0.lock().unwrap(), 1);
    }

    /// Runs `body` in a fresh child copy of this test binary (the
    /// current test re-run alone, marked by `ANA_EXEC_TEST_CHILD`):
    /// `CommandExt::exec` with a modified environment races lock-disciplined
    /// `std::env` readers on other threads (an upstream std soundness bug,
    /// <https://github.com/rust-lang/rust/issues/156951>), which would
    /// crash the whole suite in-process.
    #[cfg(unix)]
    fn exec_in_child_process(test_name: &str, body: impl FnOnce()) {
        if std::env::var_os("ANA_EXEC_TEST_CHILD").is_some() {
            body();
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
            .env("ANA_EXEC_TEST_CHILD", "1")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "child run of {test_name} failed: {status}"
        );
    }

    /// Windows counterpart: no `environ` swap exists there, so the body
    /// runs in-process.
    #[cfg(not(unix))]
    fn exec_in_child_process(_test_name: &str, body: impl FnOnce()) {
        body();
    }

    /// [`exec`]'s only testable path: a command that can't even be
    /// started. A real, found command would replace (Unix) or wait out
    /// (Windows) the test process itself, so this is deliberately the
    /// one case exercised here.
    #[test]
    fn exec_of_an_unresolvable_command_returns_an_error() {
        exec_in_child_process(
            "run::tests::exec_of_an_unresolvable_command_returns_an_error",
            || {
                let outcome = RunOutcome {
                    ensure: EnsureOutcome::Fresh,
                    install: None,
                    env_path: tempfile::tempdir().unwrap().path().to_path_buf(),
                    command: vec!["ana-test-definitely-not-a-real-binary".to_string()],
                    packages: vec![],
                };
                let err = exec(&outcome, &[]);
                assert!(matches!(err, Error::Exec { .. }));
            },
        );
    }

    #[test]
    fn exec_of_an_empty_command_returns_an_error_not_a_panic() {
        let outcome = RunOutcome {
            ensure: EnsureOutcome::Fresh,
            install: None,
            env_path: tempfile::tempdir().unwrap().path().to_path_buf(),
            command: vec![],
            packages: vec![],
        };
        assert!(matches!(exec(&outcome, &[]), Error::Exec { .. }));
    }

    /// [`exec_program_with_clean_env`]'s only testable path: a real,
    /// found command would replace (Unix) or wait out (Windows) the test
    /// process itself.
    #[test]
    fn exec_program_with_clean_env_of_an_unresolvable_command_returns_an_error() {
        exec_in_child_process(
            "run::tests::exec_program_with_clean_env_of_an_unresolvable_command_returns_an_error",
            || {
                let command = vec!["ana-test-definitely-not-a-real-binary".to_string()];
                let err = exec_program_with_clean_env(
                    "ana-test-definitely-not-a-real-binary",
                    &[],
                    &OsString::from("/usr/bin:/bin"),
                    &BTreeMap::new(),
                    &command,
                );
                assert!(matches!(err, Error::Exec { .. }));
            },
        );
    }
}
