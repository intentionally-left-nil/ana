//! The lock-generation algorithm, end to end, in its three modes:
//!
//! - [`ensure_current_platform`] -- **default mode** (`ana run`/`ana
//!   install`/`ana sync`): touches only `platform`'s section (callers pass
//!   `Platform::current()`) plus reading (and, if `dirty`, wiping)
//!   `env_path`'s own lock file (`crate::env_lock`). A stale section is
//!   re-solved biased by the *env lock's* packages, not `ana.lock`'s own
//!   possibly-stale ones -- see this function's docs for why.
//! - [`lock_platform`] -- **cross-platform mode** (`ana lock [--platform
//!   <p>]`): always solves exactly one explicitly-named platform's section;
//!   never touches `env_path` at all, for any platform, including the
//!   current one -- environment materialization is a separate concern from
//!   "refresh this platform's pins."
//! - [`check`] -- **CI mode**: reports every covered platform as
//!   `Valid`/`Stale`, entirely offline; with `fix`, re-solves stale
//!   sections via the same cross-platform flow. Never touches `env_path`,
//!   for any platform -- its whole value proposition is a complete,
//!   from-scratch verification against `ana.lock` + `pyproject.toml`,
//!   suitable for a CI job that has never seen this checkout before.
//!
//! All three hold the environment's advisory lock across their entire run,
//! solves included: solves are rare and per-environment, and the alternative
//! (re-acquiring around the write, re-validating in between) buys nothing
//! worth the complexity.
//!
//! What this module does *not* do: decide whether an install is needed, or
//! run one. Comparing the now-current section's `packages` against the env
//! lock's, and reconciling if they differ, spans `ana-installer` too, so it
//! lives in `ana::run_command`, which calls
//! [`ensure_current_platform_locked`] to bring the section current and then
//! reads the env lock itself (via [`crate::EnvLock`]) for that comparison.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use rattler_conda_types::{Platform, RepoDataRecord};
use uv_normalize::GroupName;

use crate::env_lock::EnvLock;
use crate::error::Error;
use crate::fs_util::{EnvironmentLock, EnvironmentLockGuard};
use crate::lock_file::{
    parse_platform_section, splice_section, splice_sections, LockFile, PlatformSection,
};
use crate::matchspec::{convert_for_platform, ConvertedRequirements};
use crate::project::Project;
use crate::solver::{SolveRequest, Solver, DEFAULT_CHANNELS};
use ana_paths::EnvironmentPaths;

/// What [`ensure_current_platform`] did to `ana.lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// The existing section's requirements already matched
    /// `pyproject.toml`'s current requirements: nothing was read beyond
    /// the lock section, nothing was written.
    Fresh,
    /// The platform's section was missing or its requirements had
    /// drifted from `pyproject.toml`; it was re-solved and spliced into
    /// `ana.lock`.
    Resolved,
}

/// One platform's verdict in a [`CheckReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformStatus {
    /// The section exists and its requirements match a from-scratch
    /// conversion of the current `pyproject.toml`.
    Valid,
    /// The section's requirements differ, or a declared platform has no
    /// section at all.
    Stale,
}

/// [`check`]'s per-platform verdicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub platforms: BTreeMap<Platform, PlatformStatus>,
}

impl CheckReport {
    /// Whether every platform under consideration checked out valid.
    pub fn is_fresh(&self) -> bool {
        self.platforms
            .values()
            .all(|status| *status == PlatformStatus::Valid)
    }
}

/// Opens (but does not yet acquire) `paths`' environment advisory lock --
/// the entry point for a caller that needs to hold the lock across more
/// than one call into this crate (and into `ana_installer::reconcile`
/// too, layered inside the same lock rather than a second one): acquire
/// once via `EnvironmentLock::acquire`, pass the resulting guard into
/// [`ensure_current_platform_locked`] and onward, then let it drop at the
/// end of the caller's own critical section.
///
/// ```ignore
/// let mut lock = ana_lockfile::acquire_environment_lock(&paths)?;
/// let guard = lock.acquire()?;
/// let ensure = ana_lockfile::ensure_current_platform_locked(&guard, &project, &paths, groups, platform, solver, false)?;
/// // ... e.g. ana_installer::reconcile(&guard, ...), still under the same lock ...
/// ```
pub fn acquire_environment_lock(paths: &EnvironmentPaths) -> Result<EnvironmentLock, Error> {
    open_advisory_lock(&paths.advisory_lock_path())
}

/// Default mode: make `platform`'s section of `ana.lock` agree with
/// `pyproject.toml`, doing as little work as possible, and biasing any
/// solve toward what's actually installed right now rather than
/// `ana.lock`'s own (possibly long-stale, from a different
/// branch/checkout state) packages.
///
/// 1. Read `<env_path>/ana.lock` (the env lock; see [`crate::EnvLock`]).
///    Missing or corrupt reads as `{ dirty: false, section: None }` --
///    never an error, since this file is local and gitignored.
/// 2. If it says `dirty`, a previous reconcile may have been interrupted
///    partway through: delete `env_path` recursively (which also deletes
///    the env lock file itself) and proceed exactly as if step 1 had
///    found nothing.
/// 3. Convert `pyproject.toml`'s current requirements (for every
///    requested group, plus `requires-python`) to matchspecs for
///    `platform`.
/// 4. Read `ana.lock`'s own section for `platform`. Missing, or its
///    `requirements` differing from step 3's conversion (an
///    order-independent set comparison) means the lock is stale: solve,
///    biased by the env lock's `packages` from step 1/2 (**not**
///    `ana.lock`'s own, possibly-stale, packages -- the env lock reflects
///    what's actually installed, which is a much better solve hint after
///    e.g. a branch switch than a lock section nobody has reconciled to
///    yet), then splice the result into `ana.lock`. Otherwise the
///    existing section is already current; use it as-is.
///
/// A thin wrapper around [`ensure_current_platform_locked`] that acquires
/// the lock itself, for every caller that doesn't need to extend the
/// critical section beyond this one call (every caller except
/// `ana::run_command`).
pub fn ensure_current_platform(
    project: &Project,
    paths: &EnvironmentPaths,
    groups: &[GroupName],
    platform: Platform,
    solver: &dyn Solver,
    frozen: bool,
) -> Result<EnsureOutcome, Error> {
    let mut lock = acquire_environment_lock(paths)?;
    let guard = lock.acquire().map_err(|source| Error::Lock {
        path: paths.advisory_lock_path(),
        source,
    })?;
    ensure_current_platform_locked(&guard, project, paths, groups, platform, solver, frozen)
}

/// [`ensure_current_platform`]'s actual logic, taking proof that the
/// environment's advisory lock ([`EnvironmentLockGuard`]) is already held
/// -- the extracted seam `ana::run_command` calls directly so its own
/// held lock (from [`acquire_environment_lock`]) extends unbroken through
/// the reconcile that follows in `ana::run_command` itself (see this
/// module's docs), instead of this function acquiring (and momentarily
/// releasing) its own.
///
/// `frozen` changes step 4's stale branch only: instead of solving and
/// splicing a new section into `ana.lock`, a stale (or missing) section
/// is reported as [`Error::Frozen`] -- `ana.lock` is never written.
/// Everything else (the dirty-env-lock wipe in steps 1-2, and the
/// fast-path `Fresh` return when the section already matches) is
/// unaffected: `--frozen` only ever blocks a *lock file* write, never the
/// environment being (re)created or reconciled from whatever `ana.lock`
/// already holds.
pub fn ensure_current_platform_locked(
    _guard: &EnvironmentLockGuard<'_>,
    project: &Project,
    paths: &EnvironmentPaths,
    groups: &[GroupName],
    platform: Platform,
    solver: &dyn Solver,
    frozen: bool,
) -> Result<EnsureOutcome, Error> {
    // Cheap up-front group validation so a typo'd `--group` errors even
    // when the section turns out already current; the selection itself
    // (a deep clone of every requirement) is deferred until it's needed.
    project.validate_groups(groups)?;

    // Steps 1-2.
    let env_lock = EnvLock::read(&paths.env_lock_path(), platform);
    let preferred: Vec<RepoDataRecord> = if env_lock.dirty {
        delete_env_path(&paths.env_path)?;
        Vec::new()
    } else {
        env_lock
            .section
            .map(|section| section.packages)
            .unwrap_or_default()
    };

    // Step 3.
    let selected = project.select_requirements(groups)?;
    let converted = convert_for_platform(
        &selected,
        project.pyproject().requires_python.as_ref(),
        platform,
    )?;

    // Step 4.
    let section = read_lock_section(&paths.lock_path, platform)?;
    let is_fresh = section
        .as_ref()
        .is_some_and(|section| requirements_match(section, &converted));
    if is_fresh {
        return Ok(EnsureOutcome::Fresh);
    }

    if frozen {
        return Err(Error::Frozen { platform });
    }

    let new_section = solve_section(platform, converted, &preferred, solver)?;
    splice_section(&paths.lock_path, platform, &new_section)?;
    Ok(EnsureOutcome::Resolved)
}

/// Cross-platform mode: solve exactly one explicitly-named platform's
/// section and write it, with no staleness shortcut at all -- an explicit
/// `ana lock` is the only path that picks up newly published upstream
/// packages when the requirements haven't changed ("refresh the pins").
///
/// Never touches `env_path` or its lock file, for any platform, including
/// `Platform::current()` -- environment materialization is
/// `ana::run_command`'s concern, entered only through default mode.
pub fn lock_platform(
    project: &Project,
    paths: &EnvironmentPaths,
    groups: &[GroupName],
    platform: Platform,
    solver: &dyn Solver,
) -> Result<(), Error> {
    let lock_path = paths.advisory_lock_path();
    let mut lock = open_advisory_lock(&lock_path)?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path,
        source,
    })?;

    let selected = project.select_requirements(groups)?;
    let converted = convert_for_platform(
        &selected,
        project.pyproject().requires_python.as_ref(),
        platform,
    )?;

    // The previous section (`ana.lock`'s own, for this platform) seeds
    // the solve as preferences, if it exists.
    let previous = read_lock_section(&paths.lock_path, platform)?;
    let preferred: &[RepoDataRecord] = previous
        .as_ref()
        .map(|section| section.packages.as_slice())
        .unwrap_or(&[]);

    let section = solve_section(platform, converted, preferred, solver)?;
    splice_section(&paths.lock_path, platform, &section)
}

/// CI mode: is `ana.lock` out of date? Checks every platform that has a
/// section, plus every entry of `declared` (a declared platform with no
/// section is `Stale` -- the declaration is what makes a missing section
/// detectable at all), entirely offline: value lookup + conversion only,
/// no solver, no network, and no `env_path` reads or writes for any
/// platform including the current one.
///
/// With `fix: true`, each stale platform is re-solved via the
/// cross-platform flow (which *is* network-bound) and all fixed sections
/// are spliced back in a single read/parse/write of `ana.lock`; the file
/// still changes only for sections that were actually stale.
///
/// A syntactically corrupt `ana.lock` is [`Error::CorruptLock`], never a
/// vacuous "fresh" verdict: this mode's whole purpose is a complete
/// from-scratch verification, and a lock that can't be parsed proves
/// nothing.
pub fn check(
    project: &Project,
    paths: &EnvironmentPaths,
    groups: &[GroupName],
    declared: &[Platform],
    fix: bool,
    solver: Option<&dyn Solver>,
) -> Result<CheckReport, Error> {
    if fix && solver.is_none() {
        return Err(Error::FixWithoutSolver);
    }

    let lock_path = paths.advisory_lock_path();
    let mut lock = open_advisory_lock(&lock_path)?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path,
        source,
    })?;

    let selected = project.select_requirements(groups)?;
    let lock_file = read_lock(&paths.lock_path)?;

    // The platform set under consideration: sections present in the lock,
    // unioned with the declared set.
    let mut platforms: BTreeSet<Platform> = declared.iter().copied().collect();
    if let Some(lock_file) = &lock_file {
        platforms.extend(lock_file.platforms.keys().copied());
    }

    let mut report = BTreeMap::new();
    let mut stale = Vec::new();
    for platform in platforms {
        let converted = convert_for_platform(
            &selected,
            project.pyproject().requires_python.as_ref(),
            platform,
        )?;
        let section = lock_file
            .as_ref()
            .and_then(|lock_file| lock_file.platforms.get(&platform));
        let valid = section.is_some_and(|section| requirements_match(section, &converted));
        if valid {
            report.insert(platform, PlatformStatus::Valid);
        } else {
            report.insert(platform, PlatformStatus::Stale);
            stale.push((platform, converted));
        }
    }

    if let (true, Some(solver)) = (fix, solver) {
        // Solve every stale platform first, then splice all the fixed
        // sections in one read/parse/write of `ana.lock`, rather than a
        // full-file rewrite per platform.
        let mut fixed = Vec::with_capacity(stale.len());
        for (platform, converted) in stale {
            let previous = lock_file
                .as_ref()
                .and_then(|lock_file| lock_file.platforms.get(&platform));
            let preferred: &[RepoDataRecord] = previous
                .map(|section| section.packages.as_slice())
                .unwrap_or(&[]);
            let section = solve_section(platform, converted, preferred, solver)?;
            report.insert(platform, PlatformStatus::Valid);
            fixed.push((platform, section));
        }
        if !fixed.is_empty() {
            splice_sections(&paths.lock_path, &fixed)?;
        }
    }

    Ok(CheckReport { platforms: report })
}

/// Open the environment's advisory lock file (acquisition is the caller's next
/// statement, so the guard and the lock live for the same scope).
fn open_advisory_lock(lock_path: &Path) -> Result<EnvironmentLock, Error> {
    EnvironmentLock::open(lock_path).map_err(|source| Error::Lock {
        path: lock_path.to_path_buf(),
        source,
    })
}

/// Recursively remove `env_path` -- algorithm step 2, run when the env
/// lock says `dirty = true`. A missing directory is not an error (the
/// environment was never materialized, or a previous crash happened
/// before it existed at all); any other failure propagates, since
/// leaving a possibly half-installed prefix in place while proceeding as
/// if it were clean would be worse than erroring out.
fn delete_env_path(env_path: &Path) -> Result<(), Error> {
    ana_fs_util::remove_dir_all_if_exists(env_path).map_err(|source| Error::DeleteEnv {
        path: env_path.to_path_buf(),
        source,
    })
}

/// Read the whole lock file. Missing comes back as `None` (every platform
/// is then trivially stale); a syntactically *or* semantically corrupt
/// file is [`Error::CorruptLock`], never silently treated as empty -- a
/// committed lock that can't be parsed must surface, not pass or vanish.
/// Real I/O failures still propagate.
fn read_lock(lock_path: &std::path::Path) -> Result<Option<LockFile>, Error> {
    match fs::read_to_string(lock_path) {
        Ok(text) => LockFile::parse(&text)
            .map(Some)
            .map_err(|err| Error::CorruptLock {
                path: lock_path.to_path_buf(),
                reason: err.to_string(),
            }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Read {
            path: lock_path.to_path_buf(),
            source: err,
        }),
    }
}

/// Read only `platform`'s section of the lock file, for callers that
/// never look at any other section. Missing file or missing/broken
/// section come back as `None` (a regeneration trigger); a syntactically
/// corrupt file is [`Error::CorruptLock`]. Public so callers outside this
/// crate (e.g. `ana::run_command`, reading the just-ensured platform's
/// resolved packages) can reuse this scoped parse instead of
/// [`LockFile::read`]'s full-document parse, which would pay to
/// deserialize every other platform's section for no reason.
pub fn read_lock_section(
    lock_path: &std::path::Path,
    platform: Platform,
) -> Result<Option<PlatformSection>, Error> {
    match fs::read_to_string(lock_path) {
        Ok(text) => parse_platform_section(&text, platform).map_err(|err| Error::CorruptLock {
            path: lock_path.to_path_buf(),
            reason: err.to_string(),
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Read {
            path: lock_path.to_path_buf(),
            source: err,
        }),
    }
}

/// Is `section`'s stored `requirements` still what `pyproject.toml`
/// converts to right now? A plain equality check on two sets of
/// canonical matchspec strings -- `requires-python`'s derived `python`
/// matchspec included, since it is folded into `requirements` like any
/// other entry, not a separate field. Any difference -- name added,
/// removed, or changed (including a `requires-python` edit, which
/// changes the `python` entry's matchspec string) -- is stale.
/// Deliberately no `matches()`-based semantic compatibility check
/// against the stored `PackageRecord`s: an unnecessary resolve is safe,
/// just wasted work.
fn requirements_match(section: &PlatformSection, converted: &ConvertedRequirements) -> bool {
    let stored: BTreeSet<&str> = section
        .requirements
        .iter()
        .map(|req| req.matchspec.as_str())
        .collect();
    let current: BTreeSet<&str> = converted
        .locked
        .iter()
        .map(|req| req.matchspec.as_str())
        .collect();
    stored == current
}

/// The solve step shared by every mode: solve, then build the canonical
/// [`PlatformSection`] the caller splices in. Pure solve + section
/// construction; touches nothing on disk (splicing is the caller's job,
/// so `check --fix` can solve every stale platform first and splice them
/// all in a single write).
fn solve_section(
    platform: Platform,
    converted: ConvertedRequirements,
    preferred: &[RepoDataRecord],
    solver: &dyn Solver,
) -> Result<PlatformSection, Error> {
    let packages = solver
        .solve(SolveRequest {
            platform,
            specs: converted.specs,
            preferred,
            channels: DEFAULT_CHANNELS
                .iter()
                .map(|channel| (*channel).to_string())
                .collect(),
        })
        .map_err(|source| Error::Solve { platform, source })?;

    let mut section = PlatformSection {
        requirements: converted.locked,
        packages,
    };
    section.canonicalize();
    Ok(section)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::str::FromStr;
    use std::sync::Mutex;

    use rattler_conda_types::{PackageName, PackageRecord, Version};

    use super::*;

    /// Builds a minimal but complete [`RepoDataRecord`] for a canned
    /// `name-1.0.0` package on `platform`.
    fn fake_record(name: &str, platform: Platform) -> RepoDataRecord {
        fake_record_with_version(name, "1.0.0", platform)
    }

    fn fake_record_with_version(name: &str, version: &str, platform: Platform) -> RepoDataRecord {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            "py312h1234567_0".to_string(),
        );
        record.subdir = platform.as_str().to_string();
        let identifier = rattler_conda_types::package::DistArchiveIdentifier::try_from_filename(
            &format!("{name}-{version}-py312h1234567_0.conda"),
        )
        .unwrap();
        RepoDataRecord {
            package_record: record,
            identifier,
            url: url::Url::parse(&format!(
                "file:///fake/{name}-{version}-py312h1234567_0.conda"
            ))
            .unwrap(),
            channel: None,
        }
    }

    /// A solver that "resolves" each requested spec to a canned
    /// `name-1.0.0` record and records every call (including the
    /// `preferred` bias it was handed), so tests can assert *whether* a
    /// solve happened and *what it was biased with*, not just what it
    /// produced.
    struct FakeSolver {
        calls: Mutex<Vec<SolverCall>>,
    }

    /// One recorded [`FakeSolver::solve`] call: the platform, the
    /// requested specs (as strings), and the `preferred` bias (as
    /// `"name=version"` strings).
    type SolverCall = (Platform, Vec<String>, Vec<String>);

    impl FakeSolver {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<SolverCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Solver for FakeSolver {
        fn solve(
            &self,
            request: SolveRequest<'_>,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            let preferred: Vec<String> = request
                .preferred
                .iter()
                .map(|record| {
                    format!(
                        "{}={}",
                        record.package_record.name.as_normalized(),
                        record.package_record.version
                    )
                })
                .collect();
            self.calls.lock().unwrap().push((
                request.platform,
                request.specs.iter().map(ToString::to_string).collect(),
                preferred,
            ));
            assert_eq!(request.channels, vec!["defaults".to_string()]);
            Ok(request
                .specs
                .iter()
                .filter_map(|spec| spec.name.as_exact())
                .map(|name| fake_record(name.as_normalized(), request.platform))
                .collect())
        }
    }

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
requires-python = ">=3.9"
dependencies = ["numpy>=1.20"]

[dependency-groups]
dev = ["ruff"]
"#;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        paths: EnvironmentPaths,
    }

    impl Fixture {
        fn new(pyproject: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            fs::write(root.join("pyproject.toml"), pyproject).unwrap();
            // The default environment's paths, from the same discovery
            // entry point the CLI uses.
            let paths = ana_paths::discover_paths(&root, &[]);
            Self {
                _dir: dir,
                root,
                paths,
            }
        }

        fn project(&self) -> Project {
            Project::load(&self.root).unwrap()
        }

        fn rewrite_pyproject(&self, contents: &str) {
            fs::write(self.root.join("pyproject.toml"), contents).unwrap();
        }

        fn lock_text(&self) -> String {
            fs::read_to_string(&self.paths.lock_path).unwrap()
        }

        fn lock(&self) -> LockFile {
            LockFile::read(&self.paths.lock_path).unwrap().unwrap()
        }

        fn write_env_lock(
            &self,
            platform: Platform,
            dirty: bool,
            section: Option<&PlatformSection>,
        ) {
            EnvLock::write(&self.paths.env_lock_path(), platform, dirty, section).unwrap();
        }
    }

    /// The platform default-mode tests solve for. Just a parameter there --
    /// `ensure_current_platform` never compares it against the host.
    const CURRENT: Platform = Platform::Linux64;

    /// A platform that is genuinely not the host, whatever the host is --
    /// also never `CURRENT` (Linux64), or "foreign" sections would collide
    /// with the ones default-mode tests solve for.
    fn foreign() -> Platform {
        match Platform::current() {
            Platform::Win64 => Platform::Osx64,
            _ => Platform::Win64,
        }
    }

    #[test]
    fn no_lock_resolves_and_writes_lock() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let outcome = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 1);
        assert!(
            fixture.root.join(".ana/locks/default.lock").exists(),
            "the advisory lock lives under .ana/locks/, not the project root"
        );
        assert!(!fixture.root.join(".lock").exists());

        let section = &fixture.lock().platforms[&CURRENT];
        let requirements: Vec<(&str, &str)> = section
            .requirements
            .iter()
            .map(|r| (r.matchspec.as_str(), r.source.as_str()))
            .collect();
        assert_eq!(
            requirements,
            vec![
                ("numpy >=1.20", "runtime"),
                ("python >=3.9", "requires-python"),
            ],
            "requires-python's derived `python` matchspec is now an ordinary \
             locked requirement, distinguished only by its source"
        );
        // numpy *and* the `python >=3.9` matchspec `requires-python`
        // implies -- solved as an ordinary package, not a solver-side
        // special case.
        assert_eq!(section.packages.len(), 2);
        assert!(section
            .packages
            .iter()
            .any(|p| p.package_record.name.as_normalized() == "python"));
    }

    #[test]
    fn second_run_with_no_changes_is_fresh() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, false).unwrap();
        let lock_before = fixture.lock_text();

        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, false)
                .unwrap();

        assert_eq!(outcome, EnsureOutcome::Fresh);
        // No second solve, and the committed file was not touched.
        assert_eq!(solver.calls().len(), 1);
        assert_eq!(fixture.lock_text(), lock_before);
    }

    #[test]
    fn cosmetic_pyproject_edit_stays_fresh_without_touching_lock() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();
        let lock_before = fixture.lock_text();

        // An edit that doesn't change the requirement set at all.
        fixture.rewrite_pyproject(&format!("{PYPROJECT}\n# a comment\n"));
        let outcome = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Fresh);
        assert_eq!(solver.calls().len(), 1, "no re-solve for a no-op edit");
        assert_eq!(
            fixture.lock_text(),
            lock_before,
            "ana.lock must not be dirtied by a no-op check"
        );
    }

    #[test]
    fn requirement_change_resolves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let outcome = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 2);
        let section = &fixture.lock().platforms[&CURRENT];
        assert!(section
            .requirements
            .iter()
            .any(|r| r.matchspec == "numpy >=1.21"));
    }

    #[test]
    fn requires_python_change_resolves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        fixture.rewrite_pyproject(&PYPROJECT.replace(">=3.9", ">=3.10"));
        let outcome = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        let section = &fixture.lock().platforms[&CURRENT];
        assert!(section
            .requirements
            .iter()
            .any(|r| r.source == "requires-python" && r.matchspec == "python >=3.10"));
    }

    #[test]
    fn packages_moved_under_us_with_unchanged_requirements_stays_fresh() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, false).unwrap();

        // Simulate a teammate's re-resolve landing (branch switch / git
        // pull): same requirements, different resolved packages.
        let mut moved = fixture.lock().platforms[&CURRENT].clone();
        moved.packages[0].package_record.build_number = 7;
        splice_section(&fixture.paths.lock_path, CURRENT, &moved).unwrap();

        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, false)
                .unwrap();

        // The requirements are still an exact match: no re-solve, purely
        // an offline check.
        assert_eq!(outcome, EnsureOutcome::Fresh);
        assert_eq!(solver.calls().len(), 1);
    }

    #[test]
    fn stale_solve_is_biased_by_the_env_locks_packages_not_ana_locks() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // No `ana.lock` at all yet, but the env lock already records a
        // (fictitious) previously-installed numpy -- as if this
        // environment had been materialized against a different
        // requirement set, or another platform's `ana.lock` had been
        // deleted and only `env_path`'s own bookkeeping survived.
        let env_section = PlatformSection {
            requirements: Vec::new(),
            packages: vec![fake_record_with_version("numpy", "9.9.9", CURRENT)],
        };
        fixture.write_env_lock(CURRENT, false, Some(&env_section));

        ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].2.contains(&"numpy=9.9.9".to_string()),
            "the solve must be biased by the env lock's packages: {:?}",
            calls[0].2
        );
    }

    #[test]
    fn dirty_env_lock_wipes_env_path_and_solves_with_no_bias() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // A half-installed prefix: some file inside `env_path`, plus a
        // `dirty = true` env lock recording a previously-preferred
        // package that must *not* bias the next solve, since the
        // environment it came from might not even be intact.
        fs::create_dir_all(&fixture.paths.env_path).unwrap();
        fs::write(fixture.paths.env_path.join("marker"), b"partial install").unwrap();
        let env_section = PlatformSection {
            requirements: Vec::new(),
            packages: vec![fake_record_with_version("numpy", "9.9.9", CURRENT)],
        };
        fixture.write_env_lock(CURRENT, true, Some(&env_section));

        let outcome = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert!(
            !fixture.paths.env_path.exists(),
            "a dirty env lock must wipe env_path recursively"
        );
        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].2.is_empty(),
            "a dirty wipe must not carry any preference into the solve: {:?}",
            calls[0].2
        );
    }

    #[test]
    fn frozen_stale_lock_errors_without_writing() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // No lock at all yet: a from-scratch `--frozen` run must fail
        // rather than create one.
        let result = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            true,
        );
        assert!(matches!(result, Err(Error::Frozen { platform }) if platform == CURRENT));
        assert!(solver.calls().is_empty(), "no solve on a frozen miss");
        assert!(!fixture.paths.lock_path.exists());
    }

    #[test]
    fn frozen_stale_lock_after_a_requirement_change_errors_without_writing() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();
        let lock_before = fixture.lock_text();

        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let result = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            true,
        );

        assert!(matches!(result, Err(Error::Frozen { platform }) if platform == CURRENT));
        assert_eq!(solver.calls().len(), 1, "no re-solve while frozen");
        assert_eq!(
            fixture.lock_text(),
            lock_before,
            "ana.lock must not be touched by a failed --frozen check"
        );
    }

    #[test]
    fn frozen_fresh_lock_is_unaffected() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, false).unwrap();

        // The lock is already current: `--frozen` never even has an
        // opinion here, since step 4's fast path returns before the
        // frozen check is reached.
        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, true).unwrap();
        assert_eq!(outcome, EnsureOutcome::Fresh);
        assert_eq!(solver.calls().len(), 1);
    }

    #[test]
    fn corrupt_lock_is_an_error_and_is_left_untouched() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        fs::write(&fixture.paths.lock_path, b"not [toml").unwrap();
        let result = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        );

        assert!(matches!(result, Err(Error::CorruptLock { .. })));
        assert_eq!(
            fs::read_to_string(&fixture.paths.lock_path).unwrap(),
            "not [toml",
            "a corrupt lock must never be silently rewritten"
        );
        assert!(solver.calls().is_empty(), "no solve on a corrupt lock");
    }

    #[test]
    fn check_with_corrupt_lock_is_an_error_not_a_fresh_verdict() {
        let fixture = Fixture::new(PYPROJECT);

        fs::write(&fixture.paths.lock_path, b"not [toml").unwrap();
        let result = check(&fixture.project(), &fixture.paths, &[], &[], false, None);

        assert!(matches!(result, Err(Error::CorruptLock { .. })));
    }

    #[test]
    fn unknown_group_errors_even_when_the_lock_is_fresh() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver, false).unwrap();

        let groups = vec![GroupName::from_str("nope").unwrap()];
        let result =
            ensure_current_platform(&project, &fixture.paths, &groups, CURRENT, &solver, false);
        assert!(matches!(result, Err(Error::UnknownGroup(name)) if name == "nope"));
    }

    #[test]
    fn missing_current_platform_section_resolves_only_that_platform() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // A lock that only covers a foreign platform.
        lock_platform(&fixture.project(), &fixture.paths, &[], foreign(), &solver).unwrap();
        assert!(fixture.lock().platforms.contains_key(&foreign()));

        let outcome = ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &[],
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        let lock = fixture.lock();
        assert!(
            lock.platforms.contains_key(&foreign()),
            "the foreign section must survive"
        );
        assert!(lock.platforms.contains_key(&CURRENT));
    }

    #[test]
    fn group_requirements_are_selected_and_recorded() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let groups = vec![GroupName::from_str("dev").unwrap()];

        ensure_current_platform(
            &fixture.project(),
            &fixture.paths,
            &groups,
            CURRENT,
            &solver,
            false,
        )
        .unwrap();

        let section = &fixture.lock().platforms[&CURRENT];
        let runtime_and_group: Vec<(&str, &str)> = section
            .requirements
            .iter()
            .filter(|r| r.source != "requires-python")
            .map(|r| (r.matchspec.as_str(), r.source.as_str()))
            .collect();
        assert_eq!(
            runtime_and_group,
            vec![("numpy >=1.20", "runtime"), ("ruff", "group:dev"),]
        );
    }

    #[test]
    fn cross_platform_mode_solves_foreign_section_without_touching_env_path() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(&fixture.project(), &fixture.paths, &[], foreign(), &solver).unwrap();

        let section = &fixture.lock().platforms[&foreign()];
        // numpy *and* the `python >=3.9` matchspec `requires-python`
        // implies.
        assert_eq!(section.packages.len(), 2);
        assert!(section
            .packages
            .iter()
            .all(|p| p.package_record.subdir == foreign().as_str()));
        assert!(
            !fixture.paths.env_path.exists(),
            "a foreign solve must not touch env_path"
        );
    }

    #[test]
    fn cross_platform_mode_always_solves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], foreign(), &solver).unwrap();
        // Nothing changed; an explicit lock solves anyway ("refresh the pins").
        lock_platform(&project, &fixture.paths, &[], foreign(), &solver).unwrap();
        assert_eq!(solver.calls().len(), 2);
    }

    #[test]
    fn lock_for_the_current_platform_never_touches_env_path() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], Platform::current(), &solver).unwrap();

        assert!(
            !fixture.paths.env_path.exists(),
            "cross-platform mode never touches env_path, even for the current platform"
        );
    }

    #[test]
    fn check_reports_valid_and_stale() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        // Current platform covered, foreign declared but absent.
        lock_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();

        let report = check(
            &project,
            &fixture.paths,
            &[],
            &[CURRENT, foreign()],
            false,
            None,
        )
        .unwrap();

        assert_eq!(report.platforms[&CURRENT], PlatformStatus::Valid);
        assert_eq!(report.platforms[&foreign()], PlatformStatus::Stale);
        assert!(!report.is_fresh());
        // Checking is offline: no solve happened for it.
        assert_eq!(solver.calls().len(), 1);
    }

    #[test]
    fn check_detects_requirement_drift() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver).unwrap();
        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=2.0"));

        let report = check(&fixture.project(), &fixture.paths, &[], &[], false, None).unwrap();
        assert_eq!(report.platforms[&CURRENT], PlatformStatus::Stale);
    }

    #[test]
    fn check_fix_resolves_only_stale_platforms() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        // Both platforms covered, then drift the requirements.
        lock_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        lock_platform(&project, &fixture.paths, &[], foreign(), &solver).unwrap();
        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "scipy"));
        let project = fixture.project();

        let report = check(&project, &fixture.paths, &[], &[], true, Some(&solver)).unwrap();
        assert!(report.is_fresh());
        // 2 initial solves + 2 fixes.
        assert_eq!(solver.calls().len(), 4);

        // A re-check from the same inputs is now fully valid, offline.
        let report = check(&project, &fixture.paths, &[], &[], false, None).unwrap();
        assert!(report.is_fresh());
        assert_eq!(solver.calls().len(), 4);
    }

    #[test]
    fn check_fix_with_no_stale_sections_is_a_noop() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        lock_platform(&project, &fixture.paths, &[], foreign(), &solver).unwrap();
        let lock_before = fixture.lock_text();

        let report = check(&project, &fixture.paths, &[], &[], true, Some(&solver)).unwrap();
        assert!(report.is_fresh());
        assert_eq!(solver.calls().len(), 2, "no stale sections, no fixes");
        assert_eq!(fixture.lock_text(), lock_before);
    }

    #[test]
    fn check_fix_only_resolves_the_stale_platform() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        lock_platform(&project, &fixture.paths, &[], foreign(), &solver).unwrap();

        // Drift only what linux-64 sees: a linux-only marker is invisible
        // to the foreign platform's conversion, so its section stays valid.
        fixture.rewrite_pyproject(&PYPROJECT.replace(
            "dependencies = [\"numpy>=1.20\"]",
            "dependencies = [\"numpy>=1.20\", \"py-cpuinfo; sys_platform == 'linux'\"]",
        ));
        let project = fixture.project();

        let report = check(&project, &fixture.paths, &[], &[], true, Some(&solver)).unwrap();
        assert!(report.is_fresh());
        assert_eq!(
            solver.calls().len(),
            3,
            "only the stale platform is re-solved"
        );
        assert_eq!(solver.calls()[2].0, CURRENT);
    }

    #[test]
    fn check_fix_without_solver_is_an_error() {
        let fixture = Fixture::new(PYPROJECT);
        let report = check(&fixture.project(), &fixture.paths, &[], &[], true, None);
        assert!(matches!(report, Err(Error::FixWithoutSolver)));
    }

    #[test]
    fn check_never_touches_env_path() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();

        let report = check(&project, &fixture.paths, &[], &[], true, Some(&solver)).unwrap();
        assert!(report.is_fresh());
        assert!(
            !fixture.paths.env_path.exists(),
            "check mode must not touch env_path, fix or no fix"
        );
    }
}
