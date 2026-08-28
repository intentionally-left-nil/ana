//! The `ana run` flow: discover the environment's paths, bring its lock
//! up to date, materialize the environment for real, then run the
//! command inside it.
//!
//! [`run_command`] implements the flow end to end: steps 1-4 (bringing
//! `ana.lock`'s section for the current platform up to date, biased by
//! the env lock's packages) live in
//! `ana_lockfile::ensure_current_platform_locked`; steps 5-6 (comparing
//! the now-current section's packages against the env lock's, and
//! reconciling -- with the env lock's `dirty`-flag writes around it --
//! only if they differ) live here, since they span both `ana-lockfile`
//! (the env lock itself) and `ana-installer` (the actual install).
//! [`exec`] is the separate step that actually runs the command -- kept
//! apart from `run_command` so the whole lock/ensure/reconcile pipeline
//! stays unit-testable without an actual process replacement (which would
//! tear down the test binary itself) happening inside a test. The real
//! solver behind [`Solver`] is `ana-solver`'s `RattlerSolver` (wired in by
//! `main.rs`); [`NoSolver`] stays here as a solver-free stand-in for
//! tests, turning "the lock actually needs regenerating" into an explicit
//! error instead of a silent wrong answer whenever a test deliberately
//! doesn't want a real, network-bound solve.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ana_installer::{Downloader, ReconcileMode};
use ana_lockfile::{
    acquire_environment_lock, ensure_current_platform_locked, read_lock_section, EnsureOutcome,
    EnvLock, Project, SolveRequest, Solver,
};
use ana_paths::discover_paths;
use rattler::install::{InstallationResultRecord, Transaction};
use rattler_conda_types::{Platform, RepoDataRecord};
use uv_normalize::GroupName;

use crate::Error;

/// What a successful [`run_command`] did, and everything [`exec`] needs
/// to actually run the command.
#[derive(Debug)]
pub struct RunOutcome {
    /// What bringing `ana.lock`'s section up to date did, for the caller
    /// to report.
    pub ensure: EnsureOutcome,
    /// The reconcile's resulting [`Transaction`], if one ran at all --
    /// `None` means step 5 found the section's packages already matched
    /// the env lock's, so `ana_installer::reconcile` was never even
    /// called.
    pub install: Option<Box<Transaction<InstallationResultRecord, RepoDataRecord>>>,
    /// The environment's prefix -- [`exec`] resolves the command's `PATH`
    /// from this.
    pub env_path: PathBuf,
    /// The command to run inside the environment, verbatim.
    pub command: Vec<String>,
}

/// `ana run [--group <name>]... <command>...`, with `project_dir` as the
/// project root (the process's working directory, in the binary).
///
/// Discovers the environment's paths (via `ana-paths`), then runs
/// `ana-lockfile`'s default mode for the current platform, then -- only
/// if needed -- `ana-installer`'s reconcile for the same platform, all
/// under one continuously-held advisory lock (acquired here, released
/// when this function returns -- before [`exec`] is ever called). `ana
/// run`'s reconcile mode is `Inexact`.
///
/// There is deliberately no walk-up to find the root: `project_dir` must
/// be the directory containing `pyproject.toml`.
pub fn run_command(
    project_dir: &Path,
    groups: &[GroupName],
    command: &[String],
    solver: &dyn Solver,
    runtime: &tokio::runtime::Handle,
    downloader: &Downloader,
) -> Result<RunOutcome, Error> {
    if !project_dir.join("pyproject.toml").is_file() {
        return Err(Error::NoProjectRoot);
    }
    let paths = discover_paths(project_dir, groups);
    let project = Project::load(project_dir)?;
    let platform = Platform::current();

    let mut lock = acquire_environment_lock(&paths)?;
    let guard = lock.acquire().map_err(|source| Error::Lock {
        path: paths.advisory_lock_path(),
        source,
    })?;

    // Steps 1-4: bring `ana.lock`'s section for `platform` up to date
    // (this is also where a `dirty` env lock wipes `env_path` and starts
    // fresh, and where a stale section's solve is biased by the env
    // lock's own packages -- see that function's docs).
    let ensure =
        ensure_current_platform_locked(&guard, &project, &paths, groups, platform, solver)?;

    let mut section = read_lock_section(&paths.lock_path, platform)?
        .ok_or(Error::MissingPlatformSection { platform })?;
    section.canonicalize();

    // Step 5: compare the now-current section's packages against the env
    // lock's -- read fresh here (rather than threaded through `ensure`'s
    // return value) so this always reflects reality even after a dirty
    // wipe, still under the same held advisory lock the whole time.
    // Both sides go through `canonicalize()` -- the crate's one
    // definition of "canonical order for comparison" -- rather than a
    // bespoke `.sort()`, so this comparison can never drift from what
    // `splice_section`/the env lock's own writes consider canonical.
    let env_lock_path = paths.env_lock_path();
    let env_lock = EnvLock::read(&env_lock_path, platform);
    let mut previous = env_lock.section.unwrap_or_default();
    previous.canonicalize();

    let install = if section.packages == previous.packages {
        // Nothing to install; run the command. No clone of `section`'s
        // packages was needed for this comparison, so the common case
        // (nothing changed) never pays for one.
        None
    } else {
        // Mark dirty *before* the real install starts. This write must
        // propagate on failure (`?`, not swallowed): without it landing,
        // a crash during the install that follows is indistinguishable
        // from "never started."
        EnvLock::write(&env_lock_path, platform, true, None)?;

        // Cloned only here, on the (rare) "packages actually differ"
        // path: `reconcile` needs to own its `desired` set, and `section`
        // is still needed afterward to record what's now installed --
        // deferring the clone this far means it's paid only when a real
        // install is about to happen, not on every invocation.
        let desired = section.packages.clone();
        let transaction = runtime.block_on(ana_installer::reconcile(
            &guard,
            downloader,
            &paths,
            platform,
            desired,
            ReconcileMode::Inexact,
        ))?;

        // On success, record the section that's now actually installed.
        // Best-effort: a lost write here only costs one extra
        // dirty-triggered wipe on the next invocation, not correctness.
        let _ = EnvLock::write(&env_lock_path, platform, false, Some(&section));

        Some(transaction)
    };

    // `guard` (and `lock`) drop here, at the end of this function's
    // scope, before returning to the caller -- the lock is released
    // before `exec` is ever reached, without an explicit `drop`.
    Ok(RunOutcome {
        ensure,
        install,
        env_path: paths.env_path,
        command: command.to_vec(),
    })
}

/// Actually run `outcome.command` inside `outcome.env_path`: prepend the
/// environment's executable directory (directories, on Windows) to
/// `PATH`, then either `exec` (Unix -- replaces this process image,
/// preserving signal/exit-code behavior the way `uv run`/`pixi run` do)
/// or spawn+wait+[`std::process::exit`] (Windows, which has no `exec`
/// syscall equivalent). Deliberately does **not** run any activation
/// script (`conda activate`'s full environment-variable/hook machinery)
/// -- a `PATH`-prepend is the minimum needed to make `ana run python ...`
/// actually find the installed interpreter, the same minimal approach
/// `uv run` uses for its own venvs.
///
/// Never returns on success, on any platform -- the return type exists
/// only for the failure path (`command[0]` couldn't even be started).
pub fn exec(outcome: &RunOutcome) -> Error {
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
    exec_replacing(program, args, &path, &outcome.command)
}

#[cfg(unix)]
fn exec_replacing(program: &str, args: &[String], path: &OsString, command: &[String]) -> Error {
    use std::os::unix::process::CommandExt;
    // `CommandExt::exec` only ever returns on failure -- success
    // replaces this process image and never comes back here at all.
    let source = std::process::Command::new(program)
        .args(args)
        .env("PATH", path)
        .exec();
    Error::Exec {
        command: command.to_vec(),
        source,
    }
}

#[cfg(not(unix))]
fn exec_replacing(program: &str, args: &[String], path: &OsString, command: &[String]) -> Error {
    // Windows has no `exec` syscall equivalent: spawn, wait, and exit
    // this process with the child's own exit code instead.
    match std::process::Command::new(program)
        .args(args)
        .env("PATH", path)
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(source) => Error::Exec {
            command: command.to_vec(),
            source,
        },
    }
}

/// `env_path`'s executable directory (directories, on Windows), prepended
/// to the current process's own `PATH`.
fn prepend_env_path(env_path: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        // Windows has no single `bin/`: the interpreter itself lives at
        // the prefix root, console-script shims under `Scripts/`.
        dirs.push(env_path.to_path_buf());
        dirs.push(env_path.join("Scripts"));
    } else {
        dirs.push(env_path.join("bin"));
    }
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// A solver-free [`Solver`] stand-in: any invocation that actually needs a
/// solve fails explicitly, rather than silently. `ana-solver`'s
/// `RattlerSolver` is the real implementation (wired in by `main.rs`);
/// this one exists for tests that want to assert "the solver was never
/// consulted" or exercise the offline stage-1/stage-2 paths without
/// pulling in network I/O. Fresh-lock invocations never reach it.
pub struct NoSolver;

impl Solver for NoSolver {
    fn solve(
        &self,
        _request: SolveRequest,
    ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
        Err(Box::new(SolveNotImplemented))
    }
}

/// [`NoSolver`]'s error, named so a test using it reads as an intentional
/// "no solver was supplied" notice rather than a failure of the solve
/// itself.
#[derive(Debug, thiserror::Error)]
#[error(
    "regenerating the lock requires a solver, and no solver is wired into ana yet \
     (this invocation used NoSolver -- see ana-solver for the real implementation)"
)]
struct SolveNotImplemented;

/// Render a command the way a user could paste it back into a shell:
/// arguments joined with spaces, any argument containing shell-significant
/// characters single-quoted. Used in [`Error::Exec`]'s message ("the
/// command that failed was: ...") -- display-only, nothing here is
/// executed.
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

    use std::fs;
    use std::str::FromStr;
    use std::sync::Mutex;

    use ana_lockfile::SolveRequest;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{NoArchType, PackageName, PackageRecord, Version};

    use super::*;

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

    /// The one real, installable record every test's solver hands back,
    /// regardless of which spec was requested -- `run.rs`'s own tests
    /// only need to prove the lock/ensure/reconcile/exec pipeline works
    /// end to end, not to distinguish which package is which (that's
    /// `ana-lockfile`/`ana-solver`'s job, tested there with fully fake,
    /// never-installed records).
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
        let url = url::Url::from_file_path(fixture_path()).unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url,
            channel: None,
        }
    }

    /// The same canned-record fake `ana-lockfile` tests with, except it
    /// always resolves every spec down to the one fixture record (see
    /// [`fixture_record`]) so a real `reconcile` call downstream has
    /// something genuinely installable.
    struct FakeSolver;

    impl Solver for FakeSolver {
        fn solve(
            &self,
            _request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![fixture_record()])
        }
    }

    /// A fresh runtime + downloader per test, rooted at its own temp
    /// cache dir -- never shares cache state (or its global lock) with
    /// another test or with a real `~/.cache/rattler`.
    struct Env {
        _cache: tempfile::TempDir,
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
            let downloader = Downloader::new(cache.path()).unwrap();
            Self {
                _cache: cache,
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
            run_command(
                dir,
                groups,
                command,
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

        // Second run hits both short-circuits: no re-solve (the section's
        // requirements are unchanged) and no re-install (the env lock's
        // packages already match).
        let second = env.run(dir.path(), &[], &command, &FakeSolver).unwrap();
        assert_eq!(second.ensure, EnsureOutcome::Fresh);
        assert!(
            second.install.is_none(),
            "nothing changed since the first install, so reconcile must not even be called"
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
        // The default selection's paths are untouched.
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
        // No lock yet: regeneration is required, so the missing solver
        // surfaces.
        let err = env.run(dir.path(), &[], &command, &NoSolver).unwrap_err();
        assert!(err.to_string().contains("no solver is wired into ana yet"));

        // With a fresh lock and a matching install, NoSolver is never
        // consulted.
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
            Err(Error::NoProjectRoot)
        ));
    }

    #[test]
    fn unknown_group_is_an_error() {
        let dir = project_root();
        let env = Env::new();
        let groups = vec![GroupName::from_str("nope").unwrap()];
        assert!(matches!(
            env.run(dir.path(), &groups, &["true".to_string()], &FakeSolver),
            Err(Error::Lockfile(ana_lockfile::Error::UnknownGroup(name))) if name == "nope"
        ));
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

    /// [`exec`]'s only testable path: a command that can't even be
    /// started. A real, found command would replace (Unix) or wait out
    /// (Windows) the test process itself, so this is deliberately the
    /// one case exercised here.
    #[test]
    fn exec_of_an_unresolvable_command_returns_an_error() {
        let outcome = RunOutcome {
            ensure: EnsureOutcome::Fresh,
            install: None,
            env_path: tempfile::tempdir().unwrap().path().to_path_buf(),
            command: vec!["ana-test-definitely-not-a-real-binary".to_string()],
        };
        let err = exec(&outcome);
        assert!(matches!(err, Error::Exec { .. }));
    }

    #[test]
    fn exec_of_an_empty_command_returns_an_error_not_a_panic() {
        let outcome = RunOutcome {
            ensure: EnsureOutcome::Fresh,
            install: None,
            env_path: tempfile::tempdir().unwrap().path().to_path_buf(),
            command: vec![],
        };
        assert!(matches!(exec(&outcome), Error::Exec { .. }));
    }
}
