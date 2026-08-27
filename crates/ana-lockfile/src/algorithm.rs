//! The lock-generation algorithm, end to end -- a direct implementation of
//! `investigations/lock_generation_algorithm.md`'s pseudocode, in its three
//! modes:
//!
//! - [`ensure_current_platform`] -- **default mode** (`ana run`/`ana
//!   install`/`ana sync`): touches only `platform`'s section (callers pass
//!   `Platform::current()`) plus the cache file.
//! - [`lock_platform`] -- **cross-platform mode** (`ana lock [--platform
//!   <p>]`): always solves exactly one explicitly-named platform's section;
//!   never touches `env_path` or the cache, unless `p` is the current
//!   platform (then it refreshes the cache exactly like default mode's
//!   final step).
//! - [`check`] -- **CI mode**: reports every covered platform as
//!   `Valid`/`Stale`, entirely offline; with `fix`, re-solves stale
//!   sections via the same cross-platform flow. Never reads or writes the
//!   cache file, for any platform -- its whole value proposition is a
//!   complete, from-scratch verification against `ana.lock` +
//!   `pyproject.toml`, suitable for a CI job that has never seen this
//!   checkout before.
//!
//! All three hold the environment's advisory lock across their entire run,
//! solves included: solves are rare and per-environment, and the alternative
//! (re-acquiring around the write, re-validating in between) buys nothing
//! worth the complexity.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use rattler_conda_types::Platform;
use uv_normalize::GroupName;
use uv_pep440::VersionSpecifiers;

use crate::cache::{self, CacheFile};
use crate::error::Error;
use crate::fs_util::EnvironmentLock;
use crate::lock_file::{
    parse_platform_section, splice_section, splice_sections, LockFile, PlatformSection,
};
use crate::matchspec::{convert_for_platform, ConvertedRequirements};
use crate::project::Project;
use crate::solver::{SolveRequest, Solver, DEFAULT_CHANNELS};
use ana_paths::EnvironmentPaths;

/// What [`ensure_current_platform`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// Stage-1 hit: both cache hashes matched, nothing was read beyond the
    /// lock section, nothing was written.
    Fresh,
    /// Stage-1 missed but stage 2 found the requirements unchanged; only
    /// the cache file was rewritten. `ana.lock` was *not* touched -- the
    /// whole point of the split bookkeeping.
    CacheRefreshed,
    /// The platform's section was re-solved and spliced into `ana.lock`,
    /// and the cache was rewritten.
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

/// Default mode, the investigation's steps 1-11: make `platform`'s section
/// of the environment's lock agree with `pyproject.toml`, doing as little work as
/// possible.
///
/// 1. (Path discovery already happened -- the caller hands us `paths`.)
/// 2. Acquire the environment's advisory lock, held through step 11.
/// 3. A missing lock or a missing section skips straight to regeneration;
///    a syntactically corrupt lock is [`Error::CorruptLock`], never a
///    silent rewrite.
/// 4. Extract `platform`'s section.
/// 5. Stage 1: if both hashes in the cache match the current
///    `pyproject.toml` and this section, succeed and do nothing.
/// 6. Delete the cache on any miss (a crash after this point must not
///    leave a stale cache claiming validity).
/// 7. Convert `pyproject.toml`'s requirements for `platform`.
/// 8. Stage 2: set-diff those matchspecs (and `requires_python`, as its
///    own field) against the section; any difference regenerates.
/// 9. Otherwise rewrite the cache with the new hashes and exit -- this
///    write is mandatory, or every future invocation re-misses stage 1.
/// 10. Regenerate: solve `platform`'s matchspecs (seeded with the previous
///     section's packages as preferences), re-read the lock, splice in
///     only this section, atomic write.
/// 11. Rewrite the cache with the post-solve hashes.
pub fn ensure_current_platform(
    project: &Project,
    paths: &EnvironmentPaths,
    groups: &[GroupName],
    platform: Platform,
    solver: &dyn Solver,
) -> Result<EnsureOutcome, Error> {
    let lock_path = paths.advisory_lock_path();
    let mut lock = open_advisory_lock(&lock_path)?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path,
        source,
    })?;

    // Cheap up-front group validation so a typo'd `--group` errors even
    // when a stage-1 hit would otherwise skip selection entirely; the
    // selection itself (a deep clone of every requirement) is deferred
    // until a stage actually needs it.
    project.validate_groups(groups)?;
    let pyproject_hash = project.source_hash();

    // Steps 3-4. Only this platform's section is parsed -- default mode
    // never looks at the others, so it doesn't pay to deserialize every
    // foreign platform's package records.
    let section = read_lock_section(&paths.lock_path, platform)?;

    let Some(section) = section else {
        // Step 3's skip: no usable section for this platform at all.
        cache::delete(&paths.env_path);
        let selected = project.select_requirements(groups)?;
        let converted = convert_for_platform(&selected, platform)?;
        regenerate(project, paths, platform, converted, None, solver, true)?;
        return Ok(EnsureOutcome::Resolved);
    };

    // Step 5: stage 1.
    let section_hash = section.hash();
    if let Some(cache) = cache::read(&paths.env_path) {
        if cache.pyproject_hash == pyproject_hash && cache.ana_lock_hash == section_hash {
            return Ok(EnsureOutcome::Fresh);
        }
    }

    // Step 6.
    cache::delete(&paths.env_path);

    // Steps 7-8: stage 2.
    let selected = project.select_requirements(groups)?;
    let converted = convert_for_platform(&selected, platform)?;
    if requirements_match(
        &section,
        &converted,
        project.pyproject().requires_python.as_ref(),
    ) {
        // Step 9: mandatory cache refresh. `ana.lock` is not touched.
        cache::write(
            &paths.env_path,
            &CacheFile {
                pyproject_hash,
                ana_lock_hash: section_hash,
            },
        );
        return Ok(EnsureOutcome::CacheRefreshed);
    }

    // Steps 10-11.
    regenerate(
        project,
        paths,
        platform,
        converted,
        Some(&section),
        solver,
        true,
    )?;
    Ok(EnsureOutcome::Resolved)
}

/// Cross-platform mode: solve exactly one explicitly-named platform's
/// section and write it, with no stage-1/stage-2 shortcut -- an explicit
/// `ana lock` is the only path that picks up newly published upstream
/// packages when the requirements haven't changed ("refresh the pins").
///
/// Never touches `env_path` or the cache file, which are scoped to the
/// native platform's environment -- except when `platform` *is*
/// `Platform::current()`, in which case the cache is refreshed exactly
/// like default mode's step 11.
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
    let converted = convert_for_platform(&selected, platform)?;

    // The previous section seeds the solve as preferences, if it exists.
    let previous = read_lock_section(&paths.lock_path, platform)?;

    regenerate(
        project,
        paths,
        platform,
        converted,
        previous.as_ref(),
        solver,
        platform == Platform::current(),
    )
}

/// CI mode: is `ana.lock` out of date? Checks every platform that has a
/// section, plus every entry of `declared` (a declared platform with no
/// section is `Stale` -- the declaration is what makes a missing section
/// detectable at all), entirely offline: value lookup + conversion only,
/// no solver, no network, and no cache file reads or writes for any
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
        let converted = convert_for_platform(&selected, platform)?;
        let section = lock_file
            .as_ref()
            .and_then(|lock_file| lock_file.platforms.get(&platform));
        let valid = section.is_some_and(|section| {
            requirements_match(
                section,
                &converted,
                project.pyproject().requires_python.as_ref(),
            )
        });
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
        // full-file rewrite per platform. Check mode never touches the
        // cache, even when fixing the current platform's section.
        let mut fixed = Vec::with_capacity(stale.len());
        for (platform, converted) in stale {
            let previous = lock_file
                .as_ref()
                .and_then(|lock_file| lock_file.platforms.get(&platform));
            let section = solve_section(project, platform, converted, previous, solver)?;
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

/// Read only `platform`'s section of the lock file, for the modes that
/// never look at any other section. Missing file or missing/broken
/// section come back as `None` (a regeneration trigger); a syntactically
/// corrupt file is [`Error::CorruptLock`].
fn read_lock_section(
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

/// Stage 2: a plain equality check on two sets of canonical matchspec
/// strings, plus `requires_python` as its own field. Any difference --
/// name added, removed, or changed -- is stale. Deliberately no
/// `matches()`-based semantic compatibility check against the stored
/// `PackageRecord`s: an unnecessary resolve is safe, just wasted work.
fn requirements_match(
    section: &PlatformSection,
    converted: &ConvertedRequirements,
    requires_python: Option<&VersionSpecifiers>,
) -> bool {
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
    if stored != current {
        return false;
    }

    match (&section.requires_python, requires_python) {
        (None, None) => true,
        (Some(stored), Some(current)) => {
            // An unparseable stored value can't be proven equal, so it's
            // stale -- fails open into a regenerate, never into a wrong
            // "valid".
            VersionSpecifiers::from_str(stored).is_ok_and(|stored| stored == *current)
        }
        _ => false,
    }
}

/// The resolve step shared by every mode: solve, then re-read/splice/
/// atomic-write the section, then -- only when `refresh_cache` -- rewrite
/// the stage-1 cache (default mode's step 11; cross-platform mode passes
/// `platform == Platform::current()`; check mode always passes `false`).
///
/// The solve runs under the caller's held advisory lock, network I/O
/// included; the lock file is re-read inside [`splice_section`]
/// immediately before writing, so a concurrent writer for a *different*
/// platform's section is never reverted.
fn regenerate(
    project: &Project,
    paths: &EnvironmentPaths,
    platform: Platform,
    converted: ConvertedRequirements,
    previous: Option<&PlatformSection>,
    solver: &dyn Solver,
    refresh_cache: bool,
) -> Result<(), Error> {
    let section = solve_section(project, platform, converted, previous, solver)?;
    let section_hash = refresh_cache.then(|| section.hash());
    splice_section(&paths.lock_path, platform, &section)?;

    if let Some(section_hash) = section_hash {
        cache::write(
            &paths.env_path,
            &CacheFile {
                pyproject_hash: project.source_hash(),
                ana_lock_hash: section_hash,
            },
        );
    }
    Ok(())
}

/// The solve half of [`regenerate`], separated out so `check --fix` can
/// solve every stale platform first and splice them all in a single
/// write. Pure solve + section construction; touches nothing on disk.
fn solve_section(
    project: &Project,
    platform: Platform,
    converted: ConvertedRequirements,
    previous: Option<&PlatformSection>,
    solver: &dyn Solver,
) -> Result<PlatformSection, Error> {
    let requires_python = project.pyproject().requires_python.clone();
    let packages = solver
        .solve(SolveRequest {
            platform,
            specs: converted.specs,
            requires_python: requires_python.clone(),
            preferred: previous
                .map(|section| section.packages.clone())
                .unwrap_or_default(),
            channels: DEFAULT_CHANNELS
                .iter()
                .map(|channel| (*channel).to_string())
                .collect(),
        })
        .map_err(|source| Error::Solve { platform, source })?;

    Ok(PlatformSection {
        requires_python: requires_python.map(|specifiers| specifiers.to_string()),
        requirements: converted.locked,
        packages,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;
    use std::sync::Mutex;

    use rattler_conda_types::{PackageName, PackageRecord, Version};

    use super::*;

    /// A solver that "resolves" each requested spec to a canned
    /// `name-1.0.0` record and records every call, so tests can assert
    /// *whether* a solve happened, not just what it produced.
    struct FakeSolver {
        calls: Mutex<Vec<(Platform, Vec<String>)>>,
    }

    impl FakeSolver {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(Platform, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Solver for FakeSolver {
        fn solve(
            &self,
            request: SolveRequest,
        ) -> Result<Vec<PackageRecord>, Box<dyn std::error::Error + Send + Sync>> {
            self.calls.lock().unwrap().push((
                request.platform,
                request.specs.iter().map(ToString::to_string).collect(),
            ));
            assert_eq!(request.channels, vec!["defaults".to_string()]);
            Ok(request
                .specs
                .iter()
                .filter_map(|spec| spec.name.as_exact())
                .map(|name| {
                    let mut record = PackageRecord::new(
                        PackageName::new_unchecked(name.as_normalized()),
                        Version::from_str("1.0.0").unwrap(),
                        "py312h1234567_0".to_string(),
                    );
                    record.subdir = request.platform.as_str().to_string();
                    record
                })
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

        fn cache_exists(&self) -> bool {
            cache::read(&self.paths.env_path).is_some()
        }
    }

    /// The platform default-mode tests solve for. Just a parameter there --
    /// `ensure_current_platform` never compares it against the host.
    const CURRENT: Platform = Platform::Linux64;

    /// A platform that is genuinely not the host, whatever the host is --
    /// `lock_platform` refreshes the cache when asked to solve
    /// `Platform::current()`, so foreign-platform tests must dodge it. Also
    /// never `CURRENT` (Linux64), or "foreign" sections would collide with
    /// the ones default-mode tests solve for.
    fn foreign() -> Platform {
        match Platform::current() {
            Platform::Win64 => Platform::Osx64,
            _ => Platform::Win64,
        }
    }

    #[test]
    fn no_lock_resolves_and_writes_lock_and_cache() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let outcome =
            ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver)
                .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 1);
        assert!(
            fixture.root.join(".ana/locks/default.lock").exists(),
            "the advisory lock lives under .ana/locks/, not the project root"
        );
        assert!(!fixture.root.join(".lock").exists());

        let section = &fixture.lock().platforms[&CURRENT];
        assert_eq!(section.requires_python.as_deref(), Some(">=3.9"));
        assert_eq!(
            section
                .requirements
                .iter()
                .map(|r| r.matchspec.as_str())
                .collect::<Vec<_>>(),
            vec!["numpy >=1.20"]
        );
        assert_eq!(section.packages.len(), 1);
        assert!(fixture.cache_exists());
    }

    #[test]
    fn second_run_with_no_changes_is_a_stage1_hit() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        let lock_before = fixture.lock_text();

        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();

        assert_eq!(outcome, EnsureOutcome::Fresh);
        // No second solve, and the committed file was not touched.
        assert_eq!(solver.calls().len(), 1);
        assert_eq!(fixture.lock_text(), lock_before);
    }

    #[test]
    fn cosmetic_pyproject_edit_refreshes_cache_without_touching_lock() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver).unwrap();
        let lock_before = fixture.lock_text();

        // An edit that changes the file's hash but not its requirements.
        fixture.rewrite_pyproject(&format!("{PYPROJECT}\n# a comment\n"));
        let outcome =
            ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver)
                .unwrap();

        assert_eq!(outcome, EnsureOutcome::CacheRefreshed);
        assert_eq!(solver.calls().len(), 1, "no re-solve for a no-op edit");
        assert_eq!(
            fixture.lock_text(),
            lock_before,
            "ana.lock must not be dirtied by a no-op check"
        );
        assert!(fixture.cache_exists());
    }

    #[test]
    fn requirement_change_resolves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver).unwrap();

        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let outcome =
            ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver)
                .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 2);
        let section = &fixture.lock().platforms[&CURRENT];
        assert_eq!(
            section
                .requirements
                .iter()
                .map(|r| r.matchspec.as_str())
                .collect::<Vec<_>>(),
            vec!["numpy >=1.21"]
        );
    }

    #[test]
    fn requires_python_change_resolves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver).unwrap();

        fixture.rewrite_pyproject(&PYPROJECT.replace(">=3.9", ">=3.10"));
        let outcome =
            ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver)
                .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(
            fixture.lock().platforms[&CURRENT]
                .requires_python
                .as_deref(),
            Some(">=3.10")
        );
    }

    #[test]
    fn lock_that_moved_under_us_falls_to_stage2_then_refreshes_cache() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();

        // Simulate a teammate's re-resolve landing (branch switch / git
        // pull): same requirements, different resolved packages. The
        // section hash no longer matches the cache.
        let mut moved = fixture.lock().platforms[&CURRENT].clone();
        moved.packages[0].build_number = 7;
        splice_section(&fixture.paths.lock_path, CURRENT, &moved).unwrap();

        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();

        // Stage 1 missed (lock moved) but stage 2 found the requirements
        // unchanged: cache refresh, no re-solve.
        assert_eq!(outcome, EnsureOutcome::CacheRefreshed);
        assert_eq!(solver.calls().len(), 1);
        // And the *next* run is a stage-1 hit again.
        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        assert_eq!(outcome, EnsureOutcome::Fresh);
    }

    #[test]
    fn corrupt_cache_is_a_stage1_miss_not_an_error() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        fs::write(
            fixture.paths.env_path.join("pyproject_hash.json"),
            b"not json",
        )
        .unwrap();

        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        assert_eq!(outcome, EnsureOutcome::CacheRefreshed);
        assert_eq!(solver.calls().len(), 1);
    }

    #[test]
    fn corrupt_lock_is_an_error_and_is_left_untouched() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        fs::write(&fixture.paths.lock_path, b"not [toml").unwrap();
        let result =
            ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver);

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
    fn unknown_group_errors_even_on_a_stage1_hit() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        ensure_current_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();

        let groups = vec![GroupName::from_str("nope").unwrap()];
        let result = ensure_current_platform(&project, &fixture.paths, &groups, CURRENT, &solver);
        assert!(matches!(result, Err(Error::UnknownGroup(name)) if name == "nope"));
    }

    #[test]
    fn missing_current_platform_section_resolves_only_that_platform() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // A lock that only covers a foreign platform.
        lock_platform(&fixture.project(), &fixture.paths, &[], foreign(), &solver).unwrap();
        assert!(fixture.lock().platforms.contains_key(&foreign()));

        let outcome =
            ensure_current_platform(&fixture.project(), &fixture.paths, &[], CURRENT, &solver)
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
        )
        .unwrap();

        let section = &fixture.lock().platforms[&CURRENT];
        let requirements: Vec<(&str, &str)> = section
            .requirements
            .iter()
            .map(|r| (r.matchspec.as_str(), r.source.as_str()))
            .collect();
        assert_eq!(
            requirements,
            vec![("numpy >=1.20", "runtime"), ("ruff", "group:dev"),]
        );
    }

    #[test]
    fn cross_platform_mode_solves_foreign_section_without_touching_cache() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(&fixture.project(), &fixture.paths, &[], foreign(), &solver).unwrap();

        let section = &fixture.lock().platforms[&foreign()];
        assert_eq!(section.packages.len(), 1);
        assert_eq!(section.packages[0].subdir, foreign().as_str());
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
        // Nothing changed; an explicit lock solves anyway ("refresh the
        // pins" is the whole point of the mode).
        lock_platform(&project, &fixture.paths, &[], foreign(), &solver).unwrap();
        assert_eq!(solver.calls().len(), 2);
    }

    #[test]
    fn lock_for_the_current_platform_refreshes_the_cache() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], Platform::current(), &solver).unwrap();
        assert!(fixture.cache_exists());

        // And default mode then hits stage 1 for that platform.
        let outcome =
            ensure_current_platform(&project, &fixture.paths, &[], Platform::current(), &solver)
                .unwrap();
        assert_eq!(outcome, EnsureOutcome::Fresh);
        assert_eq!(solver.calls().len(), 1);
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
    fn check_never_reads_or_writes_the_cache() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let project = fixture.project();

        lock_platform(&project, &fixture.paths, &[], CURRENT, &solver).unwrap();
        // A corrupt cache must not influence check mode at all.
        fs::create_dir_all(&fixture.paths.env_path).unwrap();
        fs::write(
            fixture.paths.env_path.join("pyproject_hash.json"),
            b"garbage",
        )
        .unwrap();
        let cache_before = fs::read(fixture.paths.env_path.join("pyproject_hash.json")).unwrap();

        let report = check(&project, &fixture.paths, &[], &[], true, Some(&solver)).unwrap();
        assert!(report.is_fresh());
        let cache_after = fs::read(fixture.paths.env_path.join("pyproject_hash.json")).unwrap();
        assert_eq!(
            cache_before, cache_after,
            "check mode must not write the cache"
        );
    }
}
