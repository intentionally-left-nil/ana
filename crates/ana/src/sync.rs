//! The `ana sync` flow: bring the project environment up to date without
//! running anything.
//!
//! [`sync_command`] does the same work `run::run_command` does for the
//! current platform, with three differences:
//!
//! - There is no command to exec afterward: `sync_command` returns once
//!   the environment matches the lock.
//! - The reconcile mode is [`ReconcileMode::Exact`], not `run`'s
//!   `Inexact` -- `ana sync` removes extraneous packages; only `ana run`
//!   stays additive-only.
//! - `--clean` (`clean: true`) deletes `env_path` recursively before the
//!   lock/reconcile step runs, forcing a full reinstall.
//!
//! `--subdir` (`subdirs`) is layered on top: for each extra platform
//! requested, bring that platform's section of `ana.lock` up to date too
//! (via `ana_lockfile::check`'s `fix` mode), without ever installing
//! anything for it -- packages are only ever materialized for the
//! current platform. This runs after the current platform's own
//! lock/reconcile step has released the environment's advisory lock, so
//! the two phases never contend over the same lock file.

use std::path::PathBuf;

use ana_environment::Environment;
use ana_fs_util::remove_dir_all_if_exists;
use ana_installer::{Downloader, ReconcileMode};
use ana_lockfile::{
    acquire_environment_lock, check, ensure_current_platform_locked, read_lock_section,
    CheckReport, EnsureOutcome, EnvLock, SolveScope, Solver,
};
use rattler::install::{InstallationResultRecord, Transaction};
use rattler_conda_types::{Platform, RepoDataRecord};

use crate::Error;

/// What a successful [`sync_command`] did.
#[derive(Debug)]
pub struct SyncOutcome {
    /// What bringing `ana.lock`'s section for the current platform up to
    /// date did, for the caller to report.
    pub ensure: EnsureOutcome,
    /// The reconcile's resulting [`Transaction`], if one ran at all --
    /// `None` means the current platform's section already matched the
    /// env lock's, so `ana_installer::reconcile` was never called.
    pub install: Option<Box<Transaction<InstallationResultRecord, RepoDataRecord>>>,
    /// The environment's prefix, for the caller to report.
    pub env_path: PathBuf,
    /// Every `--subdir` platform's verdict, after any needed re-solve.
    /// `None` when no `--subdir` was requested.
    pub subdirs: Option<CheckReport>,
}

/// Everything about *how* [`sync_command`] should sync, independent of
/// which project/environment/channels it's syncing against.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions<'a> {
    /// Delete the environment before syncing, forcing a full reinstall.
    pub clean: bool,
    /// Fail instead of solving/writing a stale (or missing) section for
    /// the current platform.
    pub frozen: bool,
    /// Extra platforms to also solve (but never install) via `check`'s
    /// `fix` mode.
    pub subdirs: &'a [Platform],
}

/// `ana sync [--group <name>]... [--clean] [--frozen] [--subdir <platform>]...`,
/// given `env` (already resolved by the caller -- see
/// `ana_environment::resolve`).
///
/// See the module docs for exactly how this differs from `ana run`.
/// `options.frozen` is passed straight through to
/// `ensure_current_platform_locked` and does not extend to
/// `options.subdirs`' own solve/fix pass, a separate concern layered on
/// afterward.
pub fn sync_command(
    env: &Environment,
    options: &SyncOptions<'_>,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
    runtime: &tokio::runtime::Handle,
    downloader: &Downloader,
) -> Result<SyncOutcome, Error> {
    let paths = env.paths();
    let platform = Platform::current();

    // Scoped to its own block so the guard (and the lock itself) is
    // released before the `--subdir` phase below acquires the same lock
    // file again.
    let (ensure, install) = {
        let mut lock = acquire_environment_lock(paths)?;
        let guard = lock.acquire().map_err(|source| Error::Lock {
            path: paths.advisory_lock_path(),
            source,
        })?;

        if options.clean {
            remove_dir_all_if_exists(&paths.env_path).map_err(|source| Error::DeleteEnv {
                path: paths.env_path.clone(),
                source,
            })?;
        }

        let ensure =
            ensure_current_platform_locked(&guard, env, platform, scope, solver, options.frozen)?;

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
                paths,
                platform,
                desired,
                ReconcileMode::Exact,
            ))?;

            let _ = EnvLock::write(&env_lock_path, platform, false, Some(&section));

            Some(transaction)
        };

        (ensure, install)
        // `guard`/`lock` drop here, releasing the advisory lock before
        // the `--subdir` phase below.
    };

    let subdir_report = if options.subdirs.is_empty() {
        None
    } else {
        Some(check(env, options.subdirs, scope, true, Some(solver))?)
    };

    Ok(SyncOutcome {
        ensure,
        install,
        env_path: paths.env_path.clone(),
        subdirs: subdir_report,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Arc;

    use ana_environment::{EnvironmentRequest, RequirementInput};
    use ana_lockfile::{PlatformStatus, SolveRequest};
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
    /// deliberately exercises a custom one. Not a real host, so tests
    /// never hit the network.
    const FIXTURE_ORIGIN: &str = "https://ana-test-fixture.internal/fixtures";

    fn test_channels() -> Vec<String> {
        vec![FIXTURE_ORIGIN.to_string()]
    }

    fn fixture_url() -> String {
        format!("{FIXTURE_ORIGIN}/{FIXTURE_FILE_NAME}")
    }

    /// Serves `fixture_url()`'s response from the local fixture archive,
    /// so tests never hit the network.
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

    /// The same tiny, real, BSD-3-Clause fixture archive `run.rs`'s
    /// tests use (see `ana-installer`'s `tests/fixtures/README.md` for
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
        let url = url::Url::parse(&fixture_url()).unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url,
            channel: None,
        }
    }

    /// Resolves every spec to [`fixture_record`] and records how many
    /// times it was called.
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

    /// Records the `channels` every `solve` call was made with.
    struct ChannelRecordingSolver {
        seen: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl ChannelRecordingSolver {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Solver for ChannelRecordingSolver {
        fn solve(
            &self,
            request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            self.seen
                .lock()
                .unwrap()
                .push(request.channels.iter().map(ana_channels::display).collect());
            Ok(vec![fixture_record()])
        }
    }

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
                Downloader::for_testing(cache.path(), Arc::new(FixtureMiddleware)).unwrap();
            Self {
                _cache: cache,
                cache_root: tempfile::tempdir().unwrap(),
                runtime,
                downloader,
            }
        }

        fn sync(
            &self,
            dir: &Path,
            groups: &[GroupName],
            clean: bool,
            frozen: bool,
            subdirs: &[Platform],
            solver: &dyn Solver,
        ) -> Result<SyncOutcome, Error> {
            self.sync_with_channels(
                dir,
                groups,
                clean,
                frozen,
                subdirs,
                &test_channels(),
                solver,
            )
        }

        #[allow(clippy::too_many_arguments)]
        fn sync_with_channels(
            &self,
            dir: &Path,
            groups: &[GroupName],
            clean: bool,
            frozen: bool,
            subdirs: &[Platform],
            channels: &[String],
            solver: &dyn Solver,
        ) -> Result<SyncOutcome, Error> {
            let map = no_mapping();
            let env = ana_environment::resolve(&EnvironmentRequest {
                input: RequirementInput::ProjectDir { dir },
                groups,
                extra: &[],
                platform: Platform::current(),
                pypi_to_conda_map: &map,
                global_cache_root: self.cache_root.path(),
            })?;
            let options = SyncOptions {
                clean,
                frozen,
                subdirs,
            };
            let scope = SolveScope {
                default_channels: channels,
                allowed_channels: &[],
                pypi_to_conda_map: &map,
            };
            sync_command(
                &env,
                &options,
                &scope,
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
    fn custom_channels_are_passed_through_to_the_solver() {
        let dir = project_root();
        let env = Env::new();
        let solver = ChannelRecordingSolver::new();
        let custom_channels = vec!["conda-forge".to_string()];

        env.sync_with_channels(
            dir.path(),
            &[],
            false,
            false,
            &[],
            &custom_channels,
            &solver,
        )
        .unwrap();

        assert_eq!(
            solver.seen.lock().unwrap().as_slice(),
            [custom_channels],
            "sync_command must solve with whatever channel list its caller passes"
        );
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
        // No `command` field exists on `SyncOutcome` -- the type itself
        // proves `sync` never carries anything to exec.
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

        let plain = env
            .sync(dir.path(), &[], false, false, &[], &solver)
            .unwrap();
        assert!(plain.install.is_none());

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
            env.sync(dir.path(), &groups, false, false, &[], &FakeSolver::new()),
            Err(Error::Environment(ana_environment::Error::Groups(
                ana_requirements::Error::UnknownGroup(name)
            ))) if name == "nope"
        ));
    }

    #[test]
    fn frozen_stale_lock_is_an_error() {
        let dir = project_root();
        let env = Env::new();
        let solver = FakeSolver::new();

        // No lock at all yet.
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

        let outcome = env
            .sync(dir.path(), &[], false, true, &[], &solver)
            .unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Fresh);
    }
}
