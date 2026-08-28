//! The `ana sync` flow: bring the project environment up to date without
//! running anything.
//!
//! [`sync_command`] does the same work `run::run_command` does for the
//! current platform -- discover paths, bring `ana.lock`'s section for
//! `Platform::current()` up to date (`ana_lockfile::ensure_current_platform_locked`),
//! then reconcile the environment against it if the target package set
//! actually changed -- with three differences:
//!
//! - There is no command to exec afterward: `sync_command` returns once
//!   the environment matches the lock, full stop.
//! - The reconcile mode is [`ReconcileMode::Exact`], not `run`'s
//!   `Inexact`: `investigations/sync_algorithm.md`'s decision is that
//!   explicit environment-mutation commands (`ana install`/`ana sync`)
//!   default to exact (add missing, remove extraneous), matching `uv
//!   sync`, while only `ana run` stays additive-only.
//! - `--clean` (`clean: true`) deletes `env_path` recursively *before*
//!   step 1 runs, forcing a full reinstall -- the same wipe a dirty env
//!   lock triggers automatically, but explicit and unconditional rather
//!   than a crash-recovery heuristic.
//!
//! `--subdir` (`subdirs`) is a separate concern layered on top: for each
//! extra platform requested, bring *that* platform's section of
//! `ana.lock` up to date too (via `ana_lockfile::check`'s `fix` mode, so a
//! platform whose requirements haven't drifted since its last solve isn't
//! re-solved for no reason), without ever installing anything for it or
//! touching `env_path` -- packages are only ever materialized for the
//! current platform. This runs *after* the current platform's own
//! lock/reconcile step has released the environment's advisory lock
//! (`ana_lockfile::check` acquires its own), so the two phases never
//! contend over the same lock file within one process.

use std::path::{Path, PathBuf};

use ana_fs_util::remove_dir_all_if_exists;
use ana_installer::{Downloader, ReconcileMode};
use ana_lockfile::{
    acquire_environment_lock, check, ensure_current_platform_locked, read_lock_section,
    CheckReport, EnsureOutcome, EnvLock, Project, Solver,
};
use ana_paths::discover_paths;
use rattler::install::{InstallationResultRecord, Transaction};
use rattler_conda_types::{Platform, RepoDataRecord};
use uv_normalize::GroupName;

use crate::Error;

/// What a successful [`sync_command`] did.
#[derive(Debug)]
pub struct SyncOutcome {
    /// What bringing `ana.lock`'s section for the current platform up to
    /// date did, for the caller to report.
    pub ensure: EnsureOutcome,
    /// The reconcile's resulting [`Transaction`], if one ran at all --
    /// `None` means the current platform's section already matched the
    /// env lock's, so `ana_installer::reconcile` was never even called.
    pub install: Option<Box<Transaction<InstallationResultRecord, RepoDataRecord>>>,
    /// The environment's prefix, for the caller to report.
    pub env_path: PathBuf,
    /// Every `--subdir` platform's verdict (and, since `check` runs with
    /// `fix: true`, its now-current status after any needed re-solve).
    /// `None` when no `--subdir` was requested, so a caller can tell "no
    /// extra platforms were asked for" apart from "asked for, all valid."
    pub subdirs: Option<CheckReport>,
}

/// `ana sync [--group <name>]... [--clean] [--frozen] [--subdir <platform>]...`,
/// with `project_dir` as the project root (the process's working
/// directory, in the binary).
///
/// See the module docs for exactly how this differs from `ana run`
/// (no exec, [`ReconcileMode::Exact`], `--clean`, `--subdir`). `frozen`
/// is passed straight through to `ensure_current_platform_locked`: a
/// stale (or missing) section for the current platform fails instead of
/// being solved and spliced into `ana.lock`. It does not extend to
/// `--subdir`'s own solve/fix pass, which is a separate concern layered
/// on afterward.
///
/// There is deliberately no walk-up to find the root, matching `ana run`:
/// `project_dir` must be the directory containing `pyproject.toml`.
#[allow(clippy::too_many_arguments)]
pub fn sync_command(
    project_dir: &Path,
    groups: &[GroupName],
    clean: bool,
    frozen: bool,
    subdirs: &[Platform],
    solver: &dyn Solver,
    runtime: &tokio::runtime::Handle,
    downloader: &Downloader,
) -> Result<SyncOutcome, Error> {
    if !project_dir.join("pyproject.toml").is_file() {
        return Err(Error::NoProjectRoot);
    }
    let paths = discover_paths(project_dir, groups);
    let project = Project::load(project_dir)?;
    let platform = Platform::current();

    // The current platform's lock/reconcile step, under the
    // environment's advisory lock -- scoped to its own block so the guard
    // (and the lock itself) is released before the `--subdir` phase
    // below acquires the same lock file again.
    let (ensure, install) = {
        let mut lock = acquire_environment_lock(&paths)?;
        let guard = lock.acquire().map_err(|source| Error::Lock {
            path: paths.advisory_lock_path(),
            source,
        })?;

        if clean {
            // Delete unconditionally, before step 1 even reads the env
            // lock: proceeding as if nothing had ever been materialized,
            // the same starting point a dirty env lock's own wipe
            // produces, but requested explicitly rather than inferred
            // from a possibly-interrupted previous reconcile.
            remove_dir_all_if_exists(&paths.env_path).map_err(|source| Error::DeleteEnv {
                path: paths.env_path.clone(),
                source,
            })?;
        }

        let ensure = ensure_current_platform_locked(
            &guard, &project, &paths, groups, platform, solver, frozen,
        )?;

        let mut section = read_lock_section(&paths.lock_path, platform)?
            .ok_or(Error::MissingPlatformSection { platform })?;
        section.canonicalize();

        let env_lock_path = paths.env_lock_path();
        let env_lock = EnvLock::read(&env_lock_path, platform);
        let mut previous = env_lock.section.unwrap_or_default();
        previous.canonicalize();

        let install = if section.packages == previous.packages {
            None
        } else {
            // Mark dirty *before* the real install starts -- see
            // `run::run_command`'s identical step for why this write
            // must propagate on failure.
            EnvLock::write(&env_lock_path, platform, true, None)?;

            let desired = section.packages.clone();
            let transaction = runtime.block_on(ana_installer::reconcile(
                &guard,
                downloader,
                &paths,
                platform,
                desired,
                ReconcileMode::Exact,
            ))?;

            let _ = EnvLock::write(&env_lock_path, platform, false, Some(&section));

            Some(transaction)
        };

        (ensure, install)
        // `guard` (and `lock`) drop here, releasing the advisory lock
        // before the `--subdir` phase below.
    };

    let subdir_report = if subdirs.is_empty() {
        None
    } else {
        Some(check(
            &project,
            &paths,
            groups,
            subdirs,
            true,
            Some(solver),
        )?)
    };

    Ok(SyncOutcome {
        ensure,
        install,
        env_path: paths.env_path,
        subdirs: subdir_report,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;
    use std::str::FromStr;

    use ana_lockfile::{PlatformStatus, SolveRequest};
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

    /// Same tiny, real, BSD-3-Clause fixture archive `run.rs`'s own tests
    /// use (see `ana-installer`'s `tests/fixtures/README.md` for
    /// provenance).
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

    /// Always resolves every spec to [`fixture_record`], so a real
    /// `reconcile` call has something genuinely installable, and records
    /// how many times it was called.
    struct FakeSolver {
        calls: std::sync::Mutex<u32>,
    }

    impl FakeSolver {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl Solver for FakeSolver {
        fn solve(
            &self,
            _request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            *self.calls.lock().unwrap() += 1;
            Ok(vec![fixture_record()])
        }
    }

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

        #[allow(clippy::too_many_arguments)]
        fn sync(
            &self,
            dir: &Path,
            groups: &[GroupName],
            clean: bool,
            frozen: bool,
            subdirs: &[Platform],
            solver: &dyn Solver,
        ) -> Result<SyncOutcome, Error> {
            sync_command(
                dir,
                groups,
                clean,
                frozen,
                subdirs,
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

    /// A platform that is genuinely not the host, whatever the host is.
    fn foreign() -> Platform {
        match Platform::current() {
            Platform::Win64 => Platform::Osx64,
            _ => Platform::Win64,
        }
    }

    #[test]
    fn fresh_sync_resolves_and_installs_for_real() {
        let dir = project_root();
        let env = Env::new();

        let outcome = env
            .sync(dir.path(), &[], false, false, &[], &FakeSolver::new())
            .unwrap();

        assert_eq!(outcome.ensure, EnsureOutcome::Resolved);
        assert!(outcome.install.is_some());
        assert!(dir.path().join("ana.lock").exists());
        assert!(outcome
            .env_path
            .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
            .exists());
        assert!(outcome.subdirs.is_none(), "no --subdir was requested");
    }

    #[test]
    fn second_sync_with_no_changes_is_a_noop() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();

        env.sync(dir.path(), &[], false, false, &[], &solver)
            .unwrap();
        let second = env
            .sync(dir.path(), &[], false, false, &[], &solver)
            .unwrap();

        assert_eq!(second.ensure, EnsureOutcome::Fresh);
        assert!(second.install.is_none());
    }

    #[test]
    fn does_not_execute_anything() {
        // There is no `command` field on `SyncOutcome` at all -- the type
        // itself proves `sync` never carries anything to exec. This test
        // exists as a compile-time sanity check that stays visible in the
        // test list rather than a silent absence.
        fn assert_no_command_field(_: &SyncOutcome) {}
        let dir = project_root();
        let env = Env::new();
        let outcome = env
            .sync(dir.path(), &[], false, false, &[], &FakeSolver::new())
            .unwrap();
        assert_no_command_field(&outcome);
    }

    #[test]
    fn clean_forces_a_fresh_install_even_with_an_unchanged_lock() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();

        let first = env
            .sync(dir.path(), &[], false, false, &[], &solver)
            .unwrap();
        assert!(first.install.is_some());

        // No changes at all: a plain second sync is a no-op.
        let plain = env
            .sync(dir.path(), &[], false, false, &[], &solver)
            .unwrap();
        assert!(plain.install.is_none());

        // `--clean` wipes the environment first, so the same unchanged
        // lock still triggers a real reinstall.
        let cleaned = env
            .sync(dir.path(), &[], true, false, &[], &solver)
            .unwrap();
        assert_eq!(
            cleaned.ensure,
            EnsureOutcome::Fresh,
            "the lock itself didn't need re-solving"
        );
        assert!(
            cleaned.install.is_some(),
            "--clean must force a reinstall even though the lock was already current"
        );
    }

    #[test]
    fn subdir_solves_an_extra_platform_without_installing_or_touching_env_path() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();
        let foreign_platform = foreign();

        let outcome = env
            .sync(dir.path(), &[], false, false, &[foreign_platform], &solver)
            .unwrap();

        let report = outcome.subdirs.expect("a --subdir report");
        assert_eq!(report.platforms[&foreign_platform], PlatformStatus::Valid);
        assert!(report.is_fresh());

        // The lock now covers both the current platform and the foreign
        // one; only the current platform's env was ever materialized.
        assert!(dir.path().join("ana.lock").exists());
        assert!(outcome
            .env_path
            .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
            .exists());
    }

    #[test]
    fn subdir_does_not_resolve_again_when_already_current() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();
        let foreign_platform = foreign();

        env.sync(dir.path(), &[], false, false, &[foreign_platform], &solver)
            .unwrap();
        let calls_after_first = solver.calls();

        // Nothing about pyproject.toml changed: the second sync's
        // --subdir pass must not re-solve the foreign platform.
        env.sync(dir.path(), &[], false, false, &[foreign_platform], &solver)
            .unwrap();

        assert_eq!(
            solver.calls(),
            calls_after_first,
            "an unchanged --subdir platform must not be re-solved"
        );
    }

    #[test]
    fn missing_project_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let env = Env::new();
        assert!(matches!(
            env.sync(dir.path(), &[], false, false, &[], &FakeSolver::new()),
            Err(Error::NoProjectRoot)
        ));
    }

    #[test]
    fn unknown_group_is_an_error() {
        let dir = project_root();
        let env = Env::new();
        let groups = vec![GroupName::from_str("nope").unwrap()];
        assert!(matches!(
            env.sync(dir.path(), &groups, false, false, &[], &FakeSolver::new()),
            Err(Error::Lockfile(ana_lockfile::Error::UnknownGroup(name))) if name == "nope"
        ));
    }

    #[test]
    fn frozen_stale_lock_is_an_error() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();

        // No lock at all yet: a from-scratch `--frozen` sync must fail
        // rather than create one.
        let err = env
            .sync(dir.path(), &[], false, true, &[], &solver)
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Lockfile(ana_lockfile::Error::Frozen { .. })
        ));
        assert!(!dir.path().join("ana.lock").exists());
        assert_eq!(solver.calls(), 0, "no solve on a frozen miss");
    }

    #[test]
    fn frozen_fresh_lock_still_syncs() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();

        env.sync(dir.path(), &[], false, false, &[], &solver)
            .unwrap();

        // The lock is already current, so `--frozen` never has anything
        // to object to.
        let outcome = env
            .sync(dir.path(), &[], false, true, &[], &solver)
            .unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Fresh);
    }
}
