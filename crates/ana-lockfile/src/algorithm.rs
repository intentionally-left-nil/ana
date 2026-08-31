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
//! solves included.
//!
//! What this module does *not* do: decide whether an install is needed, or
//! run one. That spans `ana-installer` too, so it lives in
//! `ana::run_command`, which calls [`ensure_current_platform_locked`] to
//! bring the section current and then reads the env lock itself (via
//! [`crate::EnvLock`]) for that comparison.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use ana_channels::{ChannelOverride, ChannelPolicy, ChannelSet, EffectiveChannels};
use ana_environment::Environment;
use ana_matchspec_convert::{
    convert_for_platform_with_matchspec_entries, matchspec_entries, ConvertedRequirements,
    MatchspecEntry,
};
use ana_paths::EnvironmentPaths;
use ana_pypi_conda_map::MappingHandle;
use rattler_conda_types::{ChannelUrl, Platform, RepoDataRecord};

use crate::env_lock::EnvLock;
use crate::error::Error;
use crate::fs_util::{EnvironmentLock, EnvironmentLockGuard};
use crate::lock_file::{
    parse_platform_section, splice_section, splice_sections, LockFile, PlatformSection,
};
use crate::solver::{SolveRequest, Solver};

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

/// The channel inputs shared by every mode in this module -- *how* to
/// solve. *What* to solve is the [`Environment`] every mode also takes.
/// Bundled so each function doesn't carry its own flat, ever-growing
/// parameter list.
#[derive(Debug, Clone, Copy)]
pub struct SolveScope<'a> {
    /// The channel allow-policy every solve is checked against:
    /// `default_channels ∪ allowed_channels`, resolved once (see
    /// `ana_channels::ChannelPolicy`).
    pub channels: &'a ChannelPolicy,
    /// The `pypi_name -> conda_name` lookup table every PEP 508
    /// requirement's name is checked against on its way to a matchspec
    /// (see `crate::matchspec::convert_for_platform`). Always a real
    /// handle, never optional.
    pub pypi_to_conda_map: &'a MappingHandle,
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
/// let mut lock = ana_lockfile::acquire_environment_lock(env.paths())?;
/// let guard = lock.acquire()?;
/// let ensure = ana_lockfile::ensure_current_platform_locked(&guard, &env, platform, &scope, solver, false)?;
/// // ... e.g. ana_installer::reconcile(&guard, ...), still under the same lock ...
/// ```
pub fn acquire_environment_lock(paths: &EnvironmentPaths) -> Result<EnvironmentLock, Error> {
    open_advisory_lock(&paths.advisory_lock_path())
}

/// Default mode: make `platform`'s section of `ana.lock` agree with
/// `pyproject.toml`, doing as little work as possible, and biasing any
/// solve toward what's actually installed right now rather than
/// `ana.lock`'s own (possibly long-stale) packages.
///
/// Reads the env lock (missing/corrupt reads as clean, never an error);
/// if `dirty`, wipes `env_path` first. Converts `pyproject.toml`'s
/// current requirements to matchspecs for `platform` and compares them
/// against `ana.lock`'s existing section: if they match, the section is
/// used as-is. Otherwise it's solved, biased by the env lock's packages
/// (not `ana.lock`'s own, since the env lock reflects what's actually
/// installed -- a much better hint after e.g. a branch switch), and
/// spliced into `ana.lock`.
///
/// A thin wrapper around [`ensure_current_platform_locked`] that acquires
/// the lock itself, for every caller that doesn't need to extend the
/// critical section beyond this one call (every caller except
/// `ana::run_command`).
pub fn ensure_current_platform(
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
    frozen: bool,
) -> Result<EnsureOutcome, Error> {
    let mut lock = acquire_environment_lock(env.paths())?;
    let guard = lock.acquire().map_err(|source| Error::Lock {
        path: env.paths().advisory_lock_path(),
        source,
    })?;
    ensure_current_platform_locked(&guard, env, platform, scope, solver, frozen)
}

/// [`ensure_current_platform`]'s actual logic, taking proof that the
/// environment's advisory lock ([`EnvironmentLockGuard`]) is already held
/// -- the extracted seam `ana::run_command` calls directly so its own
/// held lock (from [`acquire_environment_lock`]) extends unbroken through
/// the reconcile that follows in `ana::run_command` itself, instead of
/// this function acquiring (and momentarily releasing) its own.
///
/// `frozen` changes the stale branch only: instead of solving and
/// splicing a new section into `ana.lock`, a stale (or missing) section
/// is reported as [`Error::Frozen`] (or, if the section's `requirements`
/// matched but one of its `packages` no longer falls under `channels` --
/// see [`section_is_trustworthy`] -- the specific
/// [`ana_channels::Error::ChannelNotAllowed`] (as [`Error::Channels`])
/// instead, so `--frozen` never masks a
/// security-relevant rejection behind a generic staleness message).
/// `ana.lock` is never written. The dirty-env-lock wipe and the fast-path
/// `Fresh` return are unaffected: `--frozen` only ever blocks a *lock
/// file* write, never the environment being (re)created or reconciled.
pub fn ensure_current_platform_locked(
    _guard: &EnvironmentLockGuard<'_>,
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
    frozen: bool,
) -> Result<EnsureOutcome, Error> {
    let paths = env.paths();
    let computation = compute_platform_trust(paths, env, platform, scope)?;
    if computation.env_lock_dirty {
        delete_env_path(&paths.env_path)?;
    }

    // A section is trusted as `Fresh` only if it is *both* textually
    // unchanged from `pyproject.toml`/`requirements.txt` *and* every one
    // of its already-locked `packages` still falls under `channels` --
    // see `section_is_trustworthy`. Its `Err` (a channel violation, as
    // opposed to `Ok(false)`, ordinary drift) is not propagated with `?`
    // here: outside `--frozen`, either reason for distrusting the
    // section falls through to the same solve-and-splice below, which
    // discards and replaces *only this platform's section* with a
    // freshly, safely solved one.
    match computation.trust {
        Ok(true) => return Ok(EnsureOutcome::Fresh),
        Ok(false) => {
            if frozen {
                return Err(Error::Frozen { platform });
            }
        }
        Err(channel_violation) => {
            if frozen {
                return Err(channel_violation);
            }
        }
    }

    let new_section = solve_section(
        platform,
        computation.converted,
        &computation.preferred,
        solver,
        &computation.channels,
    )?;
    splice_section(&paths.lock_path, platform, &new_section)?;
    Ok(EnsureOutcome::Resolved)
}

/// What a real (non-`--frozen`) resolve would leave in place for one
/// platform's section of `ana.lock`, computed without ever writing
/// anything -- what backs `ana sync --dry`'s report.
#[derive(Debug, Clone)]
pub struct SectionPlan {
    pub platform: Platform,
    /// The section as it exists on disk right now, canonicalized
    /// (`None` if there is none yet).
    pub previous: Option<PlatformSection>,
    /// What a real sync would leave in place: `previous` untouched when
    /// it's already trustworthy, or a freshly solved section otherwise.
    pub next: PlatformSection,
}

impl SectionPlan {
    /// Whether `next` differs from `previous` -- always `true` when
    /// `previous` is `None`, since a section that doesn't exist yet is
    /// always a change.
    pub fn changed(&self) -> bool {
        self.previous.as_ref() != Some(&self.next)
    }
}

/// The read-only counterpart to [`ensure_current_platform_locked`]: plans
/// what it would do for `platform`'s section, without writing `ana.lock`,
/// deleting `env_path` (even if the env lock reports `dirty`), or
/// splicing anything. For `ana sync --dry`.
///
/// Never fails on staleness the way `--frozen` does -- a dry run always
/// reports what an ordinary (non-`--frozen`) sync would produce, since
/// its whole point is to preview that outcome regardless of the real
/// invocation's own `--frozen` flag.
pub fn plan_current_platform(
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
) -> Result<SectionPlan, Error> {
    let mut lock = acquire_environment_lock(env.paths())?;
    let guard = lock.acquire().map_err(|source| Error::Lock {
        path: env.paths().advisory_lock_path(),
        source,
    })?;
    plan_current_platform_locked(&guard, env, platform, scope, solver)
}

/// The read-only counterpart to [`check`]'s `fix` mode: plans what it
/// would do, without writing `ana.lock`, for the same platform set that
/// mode covers -- `platforms` (deduplicated) ∪ every platform with a
/// section in `ana.lock`, minus the current platform, which
/// [`plan_current_platform`] already planned (a real sync's `check --fix`
/// phase likewise never re-solves it: `ensure_current_platform_locked`
/// just wrote its section, so it reads `Valid`). Each [`SectionPlan`]'s
/// `next` is that platform's own untouched `previous` when already valid,
/// or a freshly solved section (biased by that platform's own previous
/// packages, never the current, installed environment's -- there may not
/// be one for a foreign platform) otherwise. For `ana sync --dry`'s
/// `--subdir` platforms.
pub fn plan_platforms(
    env: &Environment,
    platforms: &[Platform],
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
) -> Result<Vec<SectionPlan>, Error> {
    let paths = env.paths();
    let lock_path = paths.advisory_lock_path();
    let mut lock = open_advisory_lock(&lock_path)?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path,
        source,
    })?;

    let selected = env.select();
    let lock_file = read_lock(&paths.lock_path)?;
    let entries = matchspec_entries(&selected);
    let channels = scope
        .channels
        .effective_channels(env.channels(), &channel_overrides(&entries))?;

    let mut covered: BTreeSet<Platform> = platforms.iter().copied().collect();
    if let Some(lock_file) = &lock_file {
        covered.extend(lock_file.platforms.keys().copied());
    }
    covered.remove(&Platform::current());

    let mut plans = Vec::with_capacity(covered.len());
    for platform in covered {
        let converted = convert_for_platform_with_matchspec_entries(
            &entries,
            &selected,
            env.requires_python(),
            platform,
            scope.pypi_to_conda_map,
        )?;
        let mut previous = lock_file
            .as_ref()
            .and_then(|lock_file| lock_file.platforms.get(&platform))
            .cloned();
        if let Some(section) = previous.as_mut() {
            section.canonicalize();
        }
        let valid = previous.as_ref().is_some_and(|section| {
            section_is_trustworthy(section, &converted, scope.channels, &channels).unwrap_or(false)
        });
        let next = if valid {
            previous.clone().unwrap_or_default()
        } else {
            let preferred: &[RepoDataRecord] = previous
                .as_ref()
                .map(|section| section.packages.as_slice())
                .unwrap_or(&[]);
            solve_section(platform, converted, preferred, solver, &channels)?
        };
        plans.push(SectionPlan {
            platform,
            previous,
            next,
        });
    }
    Ok(plans)
}

/// [`plan_current_platform`]'s actual logic, taking proof that the
/// environment's advisory lock ([`EnvironmentLockGuard`]) is already held.
fn plan_current_platform_locked(
    _guard: &EnvironmentLockGuard<'_>,
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
) -> Result<SectionPlan, Error> {
    let computation = compute_platform_trust(env.paths(), env, platform, scope)?;
    let next = if computation.trust.unwrap_or(false) {
        computation.previous.clone().unwrap_or_default()
    } else {
        solve_section(
            platform,
            computation.converted,
            &computation.preferred,
            solver,
            &computation.channels,
        )?
    };

    Ok(SectionPlan {
        platform,
        previous: computation.previous,
        next,
    })
}

/// What computing whether a platform's section can be trusted as-is
/// needs, shared by [`ensure_current_platform_locked`] and
/// [`plan_current_platform_locked`]: everything *except* the
/// disk-mutating dirty-env-path wipe (the real function's own concern,
/// driven by `env_lock_dirty`) and the frozen-error/splice decision (each
/// caller's own).
struct PlatformComputation {
    /// The section as it exists on disk right now, canonicalized.
    previous: Option<PlatformSection>,
    /// `Ok(true)` if `previous` can be trusted as-is; `Ok(false)` for
    /// ordinary drift; `Err` for a channel-policy violation -- see
    /// [`section_is_trustworthy`].
    trust: Result<bool, Error>,
    converted: ConvertedRequirements,
    channels: EffectiveChannels,
    /// What the env lock reported: `ensure_current_platform_locked` wipes
    /// `env_path` on this; a plan must not.
    env_lock_dirty: bool,
    /// The bias a solve of this platform is seeded with: nothing when the
    /// env lock is `dirty` (its own packages are just as untrustworthy a
    /// bias as they'd be after the real wipe), its packages otherwise.
    preferred: Vec<RepoDataRecord>,
}

fn compute_platform_trust(
    paths: &EnvironmentPaths,
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
) -> Result<PlatformComputation, Error> {
    let env_lock = EnvLock::read(&paths.env_lock_path(), platform);
    let preferred: Vec<RepoDataRecord> = if env_lock.dirty {
        Vec::new()
    } else {
        env_lock
            .section
            .map(|section| section.packages)
            .unwrap_or_default()
    };

    // `matchspec_entries` is computed once and shared by the
    // channel-policy check and the conversion below.
    let selected = env.select();
    let selected_matchspec_entries = matchspec_entries(&selected);
    let channels = scope.channels.effective_channels(
        env.channels(),
        &channel_overrides(&selected_matchspec_entries),
    )?;
    let converted = convert_for_platform_with_matchspec_entries(
        &selected_matchspec_entries,
        &selected,
        env.requires_python(),
        platform,
        scope.pypi_to_conda_map,
    )?;

    let mut previous = read_lock_section(&paths.lock_path, platform)?;
    if let Some(section) = previous.as_mut() {
        section.canonicalize();
    }
    let trust = match &previous {
        Some(section) => section_is_trustworthy(section, &converted, scope.channels, &channels),
        None => Ok(false),
    };

    Ok(PlatformComputation {
        previous,
        trust,
        converted,
        channels,
        env_lock_dirty: env_lock.dirty,
        preferred,
    })
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
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
) -> Result<(), Error> {
    let paths = env.paths();
    let lock_path = paths.advisory_lock_path();
    let mut lock = open_advisory_lock(&lock_path)?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path,
        source,
    })?;

    let selected = env.select();
    let selected_matchspec_entries = matchspec_entries(&selected);
    let channels = scope.channels.effective_channels(
        env.channels(),
        &channel_overrides(&selected_matchspec_entries),
    )?;
    let converted = convert_for_platform_with_matchspec_entries(
        &selected_matchspec_entries,
        &selected,
        env.requires_python(),
        platform,
        scope.pypi_to_conda_map,
    )?;

    // The previous section seeds the solve as preferences, if it exists.
    let previous = read_lock_section(&paths.lock_path, platform)?;
    let preferred: &[RepoDataRecord] = previous
        .as_ref()
        .map(|section| section.packages.as_slice())
        .unwrap_or(&[]);

    let section = solve_section(platform, converted, preferred, solver, &channels)?;
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
    env: &Environment,
    declared: &[Platform],
    scope: &SolveScope<'_>,
    fix: bool,
    solver: Option<&dyn Solver>,
) -> Result<CheckReport, Error> {
    if fix && solver.is_none() {
        return Err(Error::FixWithoutSolver);
    }

    let paths = env.paths();
    let lock_path = paths.advisory_lock_path();
    let mut lock = open_advisory_lock(&lock_path)?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path,
        source,
    })?;

    let selected = env.select();
    let lock_file = read_lock(&paths.lock_path)?;

    let entries = matchspec_entries(&selected);

    // Runs unconditionally, before any platform's status is computed: a
    // violation fails the whole call even when every platform would
    // otherwise report `Valid`.
    let channels = scope
        .channels
        .effective_channels(env.channels(), &channel_overrides(&entries))?;

    let mut platforms: BTreeSet<Platform> = declared.iter().copied().collect();
    if let Some(lock_file) = &lock_file {
        platforms.extend(lock_file.platforms.keys().copied());
    }

    let mut report = BTreeMap::new();
    let mut stale = Vec::new();
    for platform in platforms {
        let converted = convert_for_platform_with_matchspec_entries(
            &entries,
            &selected,
            env.requires_python(),
            platform,
            scope.pypi_to_conda_map,
        )?;
        let section = lock_file
            .as_ref()
            .and_then(|lock_file| lock_file.platforms.get(&platform));
        let valid = section.is_some_and(|section| {
            section_is_trustworthy(section, &converted, scope.channels, &channels).unwrap_or(false)
        });
        if valid {
            report.insert(platform, PlatformStatus::Valid);
        } else {
            report.insert(platform, PlatformStatus::Stale);
            stale.push((platform, converted));
        }
    }

    if let (true, Some(solver)) = (fix, solver) {
        let mut fixed = Vec::with_capacity(stale.len());
        for (platform, converted) in stale {
            let previous = lock_file
                .as_ref()
                .and_then(|lock_file| lock_file.platforms.get(&platform));
            let preferred: &[RepoDataRecord] = previous
                .map(|section| section.packages.as_slice())
                .unwrap_or(&[]);
            let section = solve_section(platform, converted, preferred, solver, &channels)?;
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

/// Recursively remove `env_path`, run when the env lock says `dirty =
/// true`. A missing directory is not an error; any other failure
/// propagates, since leaving a possibly half-installed prefix in place
/// while proceeding as if it were clean would be worse than erroring out.
fn delete_env_path(env_path: &Path) -> Result<(), Error> {
    ana_fs_util::remove_dir_all_if_exists(env_path).map_err(|source| Error::DeleteEnv {
        path: env_path.to_path_buf(),
        source,
    })
}

/// Read the whole lock file. Missing comes back as `None` (every platform
/// is then trivially stale); a syntactically or semantically corrupt file
/// is [`Error::CorruptLock`], never silently treated as empty.
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
/// never need any other section (e.g. `ana::run_command`, reading the
/// just-ensured platform's resolved packages) without paying to
/// deserialize every other platform's section via [`LockFile::read`].
/// Missing file or missing/broken section come back as `None`; a
/// syntactically corrupt file is [`Error::CorruptLock`].
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
/// canonical matchspec strings, `requires-python`'s derived `python`
/// matchspec included. Deliberately no `matches()`-based semantic
/// compatibility check against the stored `PackageRecord`s: an
/// unnecessary resolve is safe, just wasted work.
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

/// Whether `section` is safe to trust as-is, with no real solve --
/// `ensure_current_platform_locked`'s `Fresh` verdict and `check`'s
/// `Valid` one both mean exactly this. Three independent conditions, all
/// required: `section.requirements` must match `converted`
/// ([`requirements_match`]); `section.channels_digest` must match
/// `channels.digest` (catches a `default_channels`/`allowed_channels`/
/// `conda-channels` change since this section was last solved, even a
/// reorder that no `packages` check alone would notice); and every one
/// of `section.packages` must still fall under `channels.set`
/// ([`validate_locked_packages`]) -- `ana.lock` itself is untrusted
/// input, exactly like `pyproject.toml`, so a hand-edit or malicious
/// checkout can change `packages` without touching a declared
/// requirement or the channel config.
///
/// `Ok(false)` (ordinary drift) and `Err` (a channel-policy violation)
/// are deliberately distinct: callers that care why a section isn't
/// trustworthy (`ensure_current_platform_locked`, deciding what to
/// report under `--frozen`) can tell them apart; callers that don't
/// (`check`) just call `.unwrap_or(false)`.
fn section_is_trustworthy(
    section: &PlatformSection,
    converted: &ConvertedRequirements,
    policy: &ChannelPolicy,
    channels: &EffectiveChannels,
) -> Result<bool, Error> {
    if !requirements_match(section, converted) {
        return Ok(false);
    }
    if section.channels_digest != channels.digest {
        return Ok(false);
    }
    validate_locked_packages(policy, channels, &section.packages)?;
    Ok(true)
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
    channels: &EffectiveChannels,
) -> Result<PlatformSection, Error> {
    let packages = solver
        .solve(SolveRequest {
            platform,
            specs: converted.specs,
            preferred,
            channels: channels.set.for_platform(platform),
        })
        .map_err(|source| Error::Solve { platform, source })?;

    let mut section = PlatformSection {
        requirements: converted.locked,
        packages,
        channels_digest: channels.digest.clone(),
    };
    section.canonicalize();
    Ok(section)
}

/// Maps `entries` (`ana_matchspec_convert::matchspec_entries`'s output) to
/// the [`ChannelOverride`]s `ChannelPolicy::effective_channels` checks:
/// one per entry that actually sets a `channel::`/`url=` override, in the
/// same order.
fn channel_overrides(entries: &[MatchspecEntry]) -> Vec<ChannelOverride<'_>> {
    entries
        .iter()
        .filter(|entry| entry.spec.channel.is_some() || entry.spec.url.is_some())
        .map(|entry| ChannelOverride {
            channel: entry.spec.channel.as_deref(),
            url: entry.spec.url.as_ref(),
            context: &entry.source,
        })
        .collect()
}

/// Validates that every one of `packages`' `channel`/`url` still falls
/// under `channels` -- the exact, already-authorized set
/// [`ChannelPolicy::effective_channels`] just returned for this same
/// call. `crate::algorithm`'s `Fresh`/`Valid` fast paths run this before
/// trusting an already-locked [`PlatformSection`] without a real solve,
/// since `effective_channels` alone only validates *declared* overrides,
/// never what actually ended up in a previous solve's `packages`.
///
/// A record with `channel: Some(_)` must name a channel present in
/// `channels.set` *and* have a `url` that reconstructs exactly as
/// `<channel>/<subdir>/<filename>` ([`channel_matches_package_url`]) --
/// `channel` is free-text, set independently of `url` in `ana.lock`, and
/// never itself consulted by anything that actually fetches a package
/// (see that function's docs), so it cannot be trusted on its own. A
/// record with `channel: None` -- produced only by a bare package-URL
/// dependency -- is checked with `policy.authorizes_artifact(&record.url)`
/// instead, since it never named a channel at all.
///
/// Every violation is collected into one `ana_channels::Error::ChannelNotAllowed`,
/// same as `effective_channels`.
fn validate_locked_packages(
    policy: &ChannelPolicy,
    channels: &EffectiveChannels,
    packages: &[RepoDataRecord],
) -> Result<(), Error> {
    let violations: Vec<String> = packages
        .iter()
        .filter(|package| !locked_package_is_authorized(policy, &channels.set, package))
        .map(|package| {
            format!(
                "  {:?} (locked package {:?}): does not fall under any allowed channel",
                package.url.as_str(),
                package.package_record.name.as_normalized(),
            )
        })
        .collect();

    if !violations.is_empty() {
        return Err(ana_channels::Error::ChannelNotAllowed(violations.join("\n")).into());
    }
    Ok(())
}

fn locked_package_is_authorized(
    policy: &ChannelPolicy,
    set: &ChannelSet,
    package: &RepoDataRecord,
) -> bool {
    match &package.channel {
        Some(channel) => channel_matches_package_url(channel, set, package),
        None => policy.authorizes_artifact(&package.url),
    }
}

/// Whether `channel` both names a member of `set` and actually accounts
/// for `package.url`: `url` must equal `<channel>/<subdir>/<filename>`
/// exactly, `<subdir>` a real [`Platform`] (never an extra path segment
/// beyond it), with `<filename>` matching `package.identifier` -- the
/// layout every real solve produces (a fetch URL is always `channel`
/// joined with its subdir and filename; see
/// `rattler_repodata_gateway`'s record construction). `package.channel`
/// is informational free text, independently settable from `url` in
/// `ana.lock`, and `rattler`'s own installer/cache never read it to
/// decide where a package actually comes from -- only `url` is ever
/// fetched from -- so a mismatch here is never trusted on `channel`'s
/// word alone.
///
/// A channel whose repodata redirects packages to a mirror via its own
/// `base_url` override (a real conda feature) produces a `url` this
/// can't reconstruct, so such a record is rejected rather than trusted;
/// re-solving -- not this fast-path check -- is how that channel is
/// picked back up.
fn channel_matches_package_url(channel: &str, set: &ChannelSet, package: &RepoDataRecord) -> bool {
    let Ok(channel_url) = url::Url::parse(channel) else {
        return false;
    };
    let channel_url: ChannelUrl = channel_url.into();
    if !set.contains(&channel_url) {
        return false;
    }
    let Ok(subdir) = Platform::from_str(&package.package_record.subdir) else {
        return false;
    };
    let Ok(expected) = channel_url
        .platform_url(subdir)
        .join(&package.identifier.to_string())
    else {
        return false;
    };
    expected == package.url
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use ana_environment::{EnvironmentRequest, RequirementInput};
    use rattler_conda_types::{PackageName, PackageRecord, Version};
    use uv_normalize::GroupName;

    use ana_matchspec_convert::LockedRequirement;

    use super::*;

    /// The channel list used by every test in this module. Just a test
    /// fixture, not the crate's real default (which lives in
    /// `ana-config`).
    const TEST_CHANNELS: &[&str] = &["defaults"];

    fn test_channels() -> Vec<String> {
        TEST_CHANNELS.iter().map(|s| s.to_string()).collect()
    }

    /// The digest a legitimate solve against `test_channels()` would
    /// stamp into a fresh section, for fixtures that need to isolate the
    /// channel-*policy* check on `packages` from the separate digest
    /// check.
    fn test_channels_digest() -> String {
        policy(&test_channels(), &[])
            .effective_channels(None, &[])
            .unwrap()
            .digest
    }

    /// A channel list literal, for `allowed_channels` test fixtures.
    fn channels(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    /// A [`ChannelPolicy`] built from `default_channels`/`allowed_channels`
    /// literals, for fixtures that used to build a `SolveScope` from a
    /// plain pair of channel lists directly.
    fn policy(default_channels: &[String], allowed_channels: &[String]) -> ChannelPolicy {
        ChannelPolicy::new(default_channels, allowed_channels).unwrap()
    }

    /// A `MappingHandle` with no entries, for tests that don't care about
    /// name mapping.
    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

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
            // Under `https://repo.anaconda.com/pkgs/main/`, one of the
            // urls `"defaults"` expands to, so records built here pass
            // the channel-policy check under `test_channels()` by
            // default.
            url: url::Url::parse(&format!(
                "https://repo.anaconda.com/pkgs/main/{}/{name}-{version}-py312h1234567_0.conda",
                platform.as_str()
            ))
            .unwrap(),
            channel: None,
        }
    }

    /// Like [`fake_record_with_version`], with an explicit `url`/`channel`
    /// for simulating an already-locked record whose metadata names a
    /// channel that would never pass `effective_channels` if checked
    /// (which an already-locked record never is).
    fn fake_record_with_channel_and_url(
        name: &str,
        version: &str,
        platform: Platform,
        channel: &str,
        url: &str,
    ) -> RepoDataRecord {
        let mut record = fake_record_with_version(name, version, platform);
        record.channel = Some(channel.to_string());
        record.url = url::Url::parse(url).unwrap();
        record
    }

    /// A solver that "resolves" each requested spec to a canned
    /// `name-1.0.0` record and records every call, so tests can assert
    /// whether a solve happened and what it was biased with.
    struct FakeSolver {
        calls: Mutex<Vec<SolverCall>>,
    }

    /// One recorded [`FakeSolver::solve`] call: platform, requested specs,
    /// `preferred` bias (as `"name=version"` strings), and channels (as
    /// their canonical base-url strings, in order).
    type SolverCall = (Platform, Vec<String>, Vec<String>, Vec<String>);

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
                request
                    .channels
                    .iter()
                    .map(|channel| channel.base_url.as_str().to_string())
                    .collect(),
            ));
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

    /// Like [`PYPROJECT`], plus a `[tool.ana]` matchspec-only runtime
    /// dependency (`compilers`) and a `dev` group merging a PEP 508 entry
    /// (`ruff`) with a matchspec entry (`cmake`). Used by the
    /// `matchspec_dependency_*`/`matchspec_group_dependency_*` tests below.
    const PYPROJECT_WITH_MATCHSPEC: &str = r#"
[project]
name = "myproj"
requires-python = ">=3.9"
dependencies = ["numpy>=1.20"]

[tool.ana]
matchspec-dependencies = ["compilers"]

[dependency-groups]
dev = ["ruff"]

[tool.ana.matchspec-dependency-groups]
dev = ["cmake"]
"#;

    /// A project-level channel override (`[tool.ana] conda-channels`),
    /// with no per-package override.
    const PYPROJECT_WITH_CONDA_CHANNELS: &str = r#"
[project]
name = "myproj"
requires-python = ">=3.9"
dependencies = ["numpy>=1.20"]

[tool.ana]
conda-channels = ["conda-forge"]
"#;

    /// A per-package `channel::` override on a runtime dependency, with
    /// no project-level `conda-channels` of its own.
    const PYPROJECT_WITH_CHANNEL_OVERRIDE: &str = r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = ["conda-forge::compilers"]
"#;

    /// A per-package `url=`/bare-URL override on a runtime dependency.
    const PYPROJECT_WITH_URL_OVERRIDE: &str = r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = [
    "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.26.0-py311h1234567_0.conda",
]
"#;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        cache_root: tempfile::TempDir,
    }

    impl Fixture {
        fn new(pyproject: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            fs::write(root.join("pyproject.toml"), pyproject).unwrap();
            Self {
                _dir: dir,
                root,
                cache_root: tempfile::tempdir().unwrap(),
            }
        }

        /// Resolves this fixture's project directory against `groups`,
        /// re-reading `pyproject.toml` from disk -- so a test that calls
        /// this again after [`rewrite_pyproject`](Self::rewrite_pyproject)
        /// sees the new content, exactly like a real second invocation
        /// would.
        fn environment(&self, groups: &[GroupName]) -> Environment {
            ana_environment::resolve(&EnvironmentRequest {
                input: RequirementInput::ProjectDir { dir: &self.root },
                groups,
                extra: &[],
                platform: CURRENT,
                pypi_to_conda_map: &no_mapping(),
                global_cache_root: self.cache_root.path(),
            })
            .unwrap()
        }

        fn rewrite_pyproject(&self, contents: &str) {
            fs::write(self.root.join("pyproject.toml"), contents).unwrap();
        }

        fn lock_text(&self, groups: &[GroupName]) -> String {
            fs::read_to_string(&self.environment(groups).paths().lock_path).unwrap()
        }

        fn lock(&self, groups: &[GroupName]) -> LockFile {
            LockFile::read(&self.environment(groups).paths().lock_path)
                .unwrap()
                .unwrap()
        }

        fn write_env_lock(
            &self,
            groups: &[GroupName],
            platform: Platform,
            dirty: bool,
            section: Option<&PlatformSection>,
        ) {
            EnvLock::write(
                &self.environment(groups).paths().env_lock_path(),
                platform,
                dirty,
                section,
            )
            .unwrap();
        }
    }

    /// The platform default-mode tests solve for.
    const CURRENT: Platform = Platform::Linux64;

    /// A platform that is genuinely not the host, and never `CURRENT`
    /// (Linux64), so a "foreign" section never collides with the ones
    /// default-mode tests solve for.
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
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
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

        let section = &fixture.lock(&[]).platforms[&CURRENT];
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

    /// The pypi-to-conda mapping table, threaded through
    /// [`SolveScope::pypi_to_conda_map`], reaches all the way to the
    /// solved lock section: `numpy` is remapped before it ever becomes a
    /// matchspec, so both the locked requirement string and the solved
    /// package's own name reflect the mapped name, never the original
    /// PyPI one.
    #[test]
    fn pypi_to_conda_map_reaches_the_solved_lock_section() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let handle = MappingHandle::from_map(HashMap::from([(
            "numpy".to_string(),
            "mapped-numpy".to_string(),
        )]));

        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &handle,
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);

        let section = &fixture.lock(&[]).platforms[&CURRENT];
        let requirements: Vec<&str> = section
            .requirements
            .iter()
            .map(|r| r.matchspec.as_str())
            .collect();
        assert!(
            requirements.contains(&"mapped-numpy >=1.20"),
            "{requirements:?}"
        );
        assert!(
            !requirements.iter().any(|r| r.starts_with("numpy")),
            "the original, unmapped name must not appear at all: {requirements:?}"
        );
        assert!(section
            .packages
            .iter()
            .any(|p| p.package_record.name.as_normalized() == "mapped-numpy"));
    }

    #[test]
    fn custom_channels_are_passed_through_to_the_solver() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let custom_channels = vec!["conda-forge".to_string()];

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&custom_channels, &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].3,
            vec!["https://conda.anaconda.org/conda-forge/".to_string()],
            "the algorithm must solve with whatever channel list its caller passes, \
             not a hardcoded default"
        );
    }

    /// `ChannelSet::for_platform` includes the `msys2` meta-channel
    /// constituent only when solving for a Windows platform; a
    /// Linux/macOS solve never sees it.
    #[test]
    fn windows_solve_includes_msys2_in_the_channel_list() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            Platform::Win64,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(
            calls[0].3,
            vec![
                "https://repo.anaconda.com/pkgs/main/".to_string(),
                "https://repo.anaconda.cloud/repo/main-x/".to_string(),
                "https://repo.anaconda.com/pkgs/r/".to_string(),
                "https://repo.anaconda.com/pkgs/msys2/".to_string(),
            ]
        );
    }

    #[test]
    fn second_run_with_no_changes_is_fresh() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        let outcome = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Fresh);
        // No second solve, and the committed file was not touched.
        assert_eq!(solver.calls().len(), 1);
        assert_eq!(fixture.lock_text(&[]), lock_before);
    }

    #[test]
    fn cosmetic_pyproject_edit_stays_fresh_without_touching_lock() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        // An edit that doesn't change the requirement set at all.
        fixture.rewrite_pyproject(&format!("{PYPROJECT}\n# a comment\n"));
        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Fresh);
        assert_eq!(solver.calls().len(), 1, "no re-solve for a no-op edit");
        assert_eq!(
            fixture.lock_text(&[]),
            lock_before,
            "ana.lock must not be dirtied by a no-op check"
        );
    }

    #[test]
    fn requirement_change_resolves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 2);
        let section = &fixture.lock(&[]).platforms[&CURRENT];
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
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        fixture.rewrite_pyproject(&PYPROJECT.replace(">=3.9", ">=3.10"));
        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        let section = &fixture.lock(&[]).platforms[&CURRENT];
        assert!(section
            .requirements
            .iter()
            .any(|r| r.source == "requires-python" && r.matchspec == "python >=3.10"));
    }

    // -----------------------------------------------------------------------
    // `tool.ana` matchspec dependencies and the freshness check
    // -----------------------------------------------------------------------
    //
    // `select_requirements` returns a mix of `Dependency::Pep508`/
    // `Dependency::Matchspec` entries (see `ana_dependency::Dependency`),
    // and `convert_for_platform` folds both into the same
    // `ConvertedRequirements.locked` canonical-matchspec-string list --
    // `requirements_match` (the "quick comparison to avoid resolving"
    // path `ensure_current_platform_locked` runs before ever touching the
    // solver) then does a plain string-set comparison with no notion of
    // where an entry came from. The tests below exercise that path
    // directly with a `Dependency::Matchspec` entry present, both when it
    // hasn't changed (cache hit: `Fresh`, no second solve) and when it
    // has (drift: `Resolved`, re-solve) -- for a runtime-level
    // `[tool.ana.matchspec-dependencies]` entry and a group-level
    // `[tool.ana.matchspec-dependency-groups]` entry merged into a group
    // that also has a PEP 508 entry.

    #[test]
    fn matchspec_dependency_second_run_with_no_changes_is_fresh() {
        let fixture = Fixture::new(PYPROJECT_WITH_MATCHSPEC);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        let first = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(first, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 1);

        let section = &fixture.lock(&[]).platforms[&CURRENT];
        assert!(
            section
                .requirements
                .iter()
                .any(|r| r.matchspec == "compilers" && r.source == "runtime"),
            "the tool.ana.matchspec-dependencies entry is a locked runtime requirement, \
             same as an ordinary PEP 508 one"
        );
        let lock_before = fixture.lock_text(&[]);

        let second = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(
            second,
            EnsureOutcome::Fresh,
            "an unchanged matchspec dependency must not be treated as drift"
        );
        assert_eq!(
            solver.calls().len(),
            1,
            "the quick comparison must avoid a second solve"
        );
        assert_eq!(fixture.lock_text(&[]), lock_before);
    }

    #[test]
    fn matchspec_dependency_change_resolves() {
        let fixture = Fixture::new(PYPROJECT_WITH_MATCHSPEC);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        fixture.rewrite_pyproject(
            &PYPROJECT_WITH_MATCHSPEC.replace(r#"["compilers"]"#, r#"["compilers >=1.0"]"#),
        );
        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(
            solver.calls().len(),
            2,
            "a changed matchspec dependency must trigger a re-solve, exactly like \
             an ordinary PEP 508 requirement change"
        );
        let section = &fixture.lock(&[]).platforms[&CURRENT];
        assert!(section
            .requirements
            .iter()
            .any(|r| r.matchspec == "compilers >=1.0"));
    }

    #[test]
    fn matchspec_group_dependency_second_run_with_no_changes_is_fresh() {
        let fixture = Fixture::new(PYPROJECT_WITH_MATCHSPEC);
        let solver = FakeSolver::new();
        let groups = vec![GroupName::from_str("dev").unwrap()];
        let env = fixture.environment(&groups);

        let first = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(first, EnsureOutcome::Resolved);
        assert_eq!(solver.calls().len(), 1);

        let section = &fixture.lock(&groups).platforms[&CURRENT];
        let group_entries: Vec<(&str, &str)> = section
            .requirements
            .iter()
            .filter(|r| r.source == "group:dev")
            .map(|r| (r.matchspec.as_str(), r.source.as_str()))
            .collect();
        assert_eq!(
            group_entries,
            vec![("cmake", "group:dev"), ("ruff", "group:dev")],
            "the group merges its PEP 508 entry (ruff, from [dependency-groups]) with its \
             matchspec entry (cmake, from [tool.ana.matchspec-dependency-groups])"
        );
        let lock_before = fixture.lock_text(&groups);

        let second = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(
            second,
            EnsureOutcome::Fresh,
            "an unchanged merged group must not be treated as drift"
        );
        assert_eq!(
            solver.calls().len(),
            1,
            "the quick comparison must avoid a second solve"
        );
        assert_eq!(fixture.lock_text(&groups), lock_before);
    }

    #[test]
    fn matchspec_group_dependency_change_resolves() {
        let fixture = Fixture::new(PYPROJECT_WITH_MATCHSPEC);
        let solver = FakeSolver::new();
        let groups = vec![GroupName::from_str("dev").unwrap()];

        ensure_current_platform(
            &fixture.environment(&groups),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        fixture.rewrite_pyproject(
            &PYPROJECT_WITH_MATCHSPEC.replace(r#"["cmake"]"#, r#"["cmake >=3.20"]"#),
        );
        let outcome = ensure_current_platform(
            &fixture.environment(&groups),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert_eq!(
            solver.calls().len(),
            2,
            "a changed group-level matchspec dependency must trigger a re-solve"
        );
        let section = &fixture.lock(&groups).platforms[&CURRENT];
        assert!(section
            .requirements
            .iter()
            .any(|r| r.matchspec == "cmake >=3.20"));
    }

    #[test]
    fn packages_moved_under_us_with_unchanged_requirements_stays_fresh() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        // Simulate a teammate's re-resolve landing (branch switch / git
        // pull): same requirements, different resolved packages.
        let mut moved = fixture.lock(&[]).platforms[&CURRENT].clone();
        moved.packages[0].package_record.build_number = 7;
        splice_section(&fixture.environment(&[]).paths().lock_path, CURRENT, &moved).unwrap();

        let outcome = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
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
            channels_digest: String::new(),
        };
        fixture.write_env_lock(&[], CURRENT, false, Some(&env_section));

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
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
        fs::create_dir_all(&fixture.environment(&[]).paths().env_path).unwrap();
        fs::write(
            fixture.environment(&[]).paths().env_path.join("marker"),
            b"partial install",
        )
        .unwrap();
        let env_section = PlatformSection {
            requirements: Vec::new(),
            packages: vec![fake_record_with_version("numpy", "9.9.9", CURRENT)],
            channels_digest: String::new(),
        };
        fixture.write_env_lock(&[], CURRENT, true, Some(&env_section));

        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        assert!(
            !fixture.environment(&[]).paths().env_path.exists(),
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
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            true,
        );
        assert!(matches!(result, Err(Error::Frozen { platform }) if platform == CURRENT));
        assert!(solver.calls().is_empty(), "no solve on a frozen miss");
        assert!(!fixture.environment(&[]).paths().lock_path.exists());
    }

    #[test]
    fn frozen_stale_lock_after_a_requirement_change_errors_without_writing() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let result = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            true,
        );

        assert!(matches!(result, Err(Error::Frozen { platform }) if platform == CURRENT));
        assert_eq!(solver.calls().len(), 1, "no re-solve while frozen");
        assert_eq!(
            fixture.lock_text(&[]),
            lock_before,
            "ana.lock must not be touched by a failed --frozen check"
        );
    }

    #[test]
    fn frozen_fresh_lock_is_unaffected() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        // The lock is already current: `--frozen` never even has an
        // opinion here, since step 4's fast path returns before the
        // frozen check is reached.
        let outcome = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            true,
        )
        .unwrap();
        assert_eq!(outcome, EnsureOutcome::Fresh);
        assert_eq!(solver.calls().len(), 1);
    }

    #[test]
    fn corrupt_lock_is_an_error_and_is_left_untouched() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        fs::write(&fixture.environment(&[]).paths().lock_path, b"not [toml").unwrap();
        let result = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        );

        assert!(matches!(result, Err(Error::CorruptLock { .. })));
        assert_eq!(
            fs::read_to_string(&fixture.environment(&[]).paths().lock_path).unwrap(),
            "not [toml",
            "a corrupt lock must never be silently rewritten"
        );
        assert!(solver.calls().is_empty(), "no solve on a corrupt lock");
    }

    #[test]
    fn check_with_corrupt_lock_is_an_error_not_a_fresh_verdict() {
        let fixture = Fixture::new(PYPROJECT);

        fs::write(&fixture.environment(&[]).paths().lock_path, b"not [toml").unwrap();
        let result = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        );

        assert!(matches!(result, Err(Error::CorruptLock { .. })));
    }

    #[test]
    fn missing_current_platform_section_resolves_only_that_platform() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // A lock that only covers a foreign platform.
        lock_platform(
            &fixture.environment(&[]),
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        assert!(fixture.lock(&[]).platforms.contains_key(&foreign()));

        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(outcome, EnsureOutcome::Resolved);
        let lock = fixture.lock(&[]);
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
            &fixture.environment(&groups),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        let section = &fixture.lock(&groups).platforms[&CURRENT];
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

        lock_platform(
            &fixture.environment(&[]),
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let section = &fixture.lock(&[]).platforms[&foreign()];
        // numpy *and* the `python >=3.9` matchspec `requires-python`
        // implies.
        assert_eq!(section.packages.len(), 2);
        assert!(section
            .packages
            .iter()
            .all(|p| p.package_record.subdir == foreign().as_str()));
        assert!(
            !fixture.environment(&[]).paths().env_path.exists(),
            "a foreign solve must not touch env_path"
        );
    }

    #[test]
    fn cross_platform_mode_always_solves() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        // Nothing changed; an explicit lock solves anyway ("refresh the pins").
        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 2);
    }

    #[test]
    fn lock_for_the_current_platform_never_touches_env_path() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            Platform::current(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert!(
            !fixture.environment(&[]).paths().env_path.exists(),
            "cross-platform mode never touches env_path, even for the current platform"
        );
    }

    #[test]
    fn check_reports_valid_and_stale() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        // Current platform covered, foreign declared but absent.
        lock_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let report = check(
            &env,
            &[CURRENT, foreign()],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
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

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=2.0"));

        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();
        assert_eq!(report.platforms[&CURRENT], PlatformStatus::Stale);
    }

    #[test]
    fn check_fix_resolves_only_stale_platforms() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        // Both platforms covered, then drift the requirements.
        lock_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "scipy"));
        let env = fixture.environment(&[]);

        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            true,
            Some(&solver),
        )
        .unwrap();
        assert!(report.is_fresh());
        // 2 initial solves + 2 fixes.
        assert_eq!(solver.calls().len(), 4);

        // A re-check from the same inputs is now fully valid, offline.
        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();
        assert!(report.is_fresh());
        assert_eq!(solver.calls().len(), 4);
    }

    #[test]
    fn check_fix_with_no_stale_sections_is_a_noop() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            true,
            Some(&solver),
        )
        .unwrap();
        assert!(report.is_fresh());
        assert_eq!(solver.calls().len(), 2, "no stale sections, no fixes");
        assert_eq!(fixture.lock_text(&[]), lock_before);
    }

    #[test]
    fn check_fix_only_resolves_the_stale_platform() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        // Drift only what linux-64 sees: a linux-only marker is invisible
        // to the foreign platform's conversion, so its section stays valid.
        fixture.rewrite_pyproject(&PYPROJECT.replace(
            "dependencies = [\"numpy>=1.20\"]",
            "dependencies = [\"numpy>=1.20\", \"py-cpuinfo; sys_platform == 'linux'\"]",
        ));
        let env = fixture.environment(&[]);

        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            true,
            Some(&solver),
        )
        .unwrap();
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
        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            true,
            None,
        );
        assert!(matches!(report, Err(Error::FixWithoutSolver)));
    }

    #[test]
    fn check_never_touches_env_path() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            true,
            Some(&solver),
        )
        .unwrap();
        assert!(report.is_fresh());
        assert!(
            !fixture.environment(&[]).paths().env_path.exists(),
            "check mode must not touch env_path, fix or no fix"
        );
    }

    // -------------------------------------------------------------------
    // `plan_current_platform`/`plan_platforms`: the read-only
    // counterparts backing `ana sync --dry`. Every test here asserts
    // `ana.lock` (and `env_path`) are untouched, on top of the plan's
    // own content -- that's the entire point of these functions over
    // `ensure_current_platform`/`check`'s writing ones.
    // -------------------------------------------------------------------

    #[test]
    fn plan_current_platform_with_no_lock_reports_previous_none_and_a_solved_next() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let plan = plan_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert_eq!(plan.platform, CURRENT);
        assert!(plan.previous.is_none());
        assert!(plan.changed());
        // numpy *and* the `python >=3.9` matchspec `requires-python`
        // implies.
        assert_eq!(plan.next.packages.len(), 2);
        assert_eq!(solver.calls().len(), 1);
        assert!(
            !fixture.environment(&[]).paths().lock_path.exists(),
            "a plan must never write ana.lock"
        );
        assert!(
            !fixture.environment(&[]).paths().env_path.exists(),
            "a plan must never touch env_path"
        );
    }

    #[test]
    fn plan_current_platform_with_a_fresh_lock_reports_next_equal_to_previous_with_no_extra_solve()
    {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);
        let lock_before = fixture.lock_text(&[]);

        let plan = plan_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert_eq!(plan.previous.as_ref(), Some(&plan.next));
        assert!(!plan.changed());
        assert_eq!(
            solver.calls().len(),
            1,
            "a fresh section must not be re-solved just to plan it"
        );
        assert_eq!(
            fixture.lock_text(&[]),
            lock_before,
            "a plan must never write ana.lock, even when it does solve"
        );
    }

    #[test]
    fn plan_current_platform_after_a_requirement_change_reports_a_different_next() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let plan = plan_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert!(plan.changed());
        assert!(plan
            .next
            .requirements
            .iter()
            .any(|r| r.matchspec == "numpy >=1.21"));
        assert_eq!(
            solver.calls().len(),
            2,
            "the drifted section must be solved to plan it"
        );
        assert_eq!(
            fixture.lock_text(&[]),
            lock_before,
            "planning a stale section must never write ana.lock"
        );
    }

    #[test]
    fn plan_current_platform_with_a_dirty_env_lock_never_wipes_env_path_and_solves_with_no_bias() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        fs::create_dir_all(&fixture.environment(&[]).paths().env_path).unwrap();
        fs::write(
            fixture.environment(&[]).paths().env_path.join("marker"),
            b"partial install",
        )
        .unwrap();
        let env_section = PlatformSection {
            requirements: Vec::new(),
            packages: vec![fake_record_with_version("numpy", "9.9.9", CURRENT)],
            channels_digest: String::new(),
        };
        fixture.write_env_lock(&[], CURRENT, true, Some(&env_section));

        let plan = plan_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert!(plan.changed());
        assert!(
            fixture.environment(&[]).paths().env_path.exists(),
            "a plan must never wipe env_path, even when the env lock is dirty"
        );
        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].2.is_empty(),
            "a dirty env lock must still plan with no bias: {:?}",
            calls[0].2
        );
    }

    #[test]
    fn plan_platforms_with_a_valid_section_reports_next_equal_to_previous_with_no_solve() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);

        let plans = plan_platforms(
            &env,
            &[foreign()],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].platform, foreign());
        assert_eq!(plans[0].previous.as_ref(), Some(&plans[0].next));
        assert!(!plans[0].changed());
        assert_eq!(
            solver.calls().len(),
            1,
            "a valid foreign section must not be re-solved just to plan it"
        );
    }

    #[test]
    fn plan_platforms_with_no_lock_solves_and_reports_previous_none() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let plans = plan_platforms(
            &fixture.environment(&[]),
            &[foreign()],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert_eq!(plans.len(), 1);
        assert!(plans[0].previous.is_none());
        assert!(plans[0].changed());
        assert_eq!(solver.calls().len(), 1);
        assert!(
            !fixture.environment(&[]).paths().lock_path.exists(),
            "plan_platforms must never write ana.lock"
        );
        assert!(
            !fixture.environment(&[]).paths().env_path.exists(),
            "plan_platforms must never touch env_path"
        );
    }

    #[test]
    fn plan_platforms_never_touches_env_path_even_for_the_current_platform() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let plans = plan_platforms(
            &fixture.environment(&[]),
            &[Platform::current()],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert!(
            plans.is_empty(),
            "the current platform is plan_current_platform's alone"
        );
        assert_eq!(solver.calls().len(), 0);
        assert!(!fixture.environment(&[]).paths().env_path.exists());
    }

    #[test]
    fn plan_platforms_covers_locked_platforms_beyond_the_declared_ones_like_check_does() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        lock_platform(
            &env,
            foreign(),
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);

        // Drift the requirements so the locked foreign section is stale,
        // then plan *without* declaring it: `check --fix` would re-solve
        // it (its platform set is declared ∪ locked), so the dry report
        // must cover it too.
        fixture.rewrite_pyproject(&PYPROJECT.replace("numpy>=1.20", "numpy>=1.21"));
        let plans = plan_platforms(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].platform, foreign());
        assert!(plans[0].changed());
        assert_eq!(solver.calls().len(), 2);
    }

    #[test]
    fn plan_platforms_solves_a_repeated_platform_once() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let plans = plan_platforms(
            &fixture.environment(&[]),
            &[foreign(), foreign()],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        assert_eq!(plans.len(), 1);
        assert_eq!(solver.calls().len(), 1);
    }

    // -------------------------------------------------------------------
    // Channel-policy validation (`ana_channels::ChannelPolicy`, wired
    // through `SolveScope::channels` and `Project::channels()`).
    // -------------------------------------------------------------------

    #[test]
    fn project_channels_override_reaches_the_solver_when_allowed() {
        let fixture = Fixture::new(PYPROJECT_WITH_CONDA_CHANNELS);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &channels(&["conda-forge"])),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].3,
            vec!["https://conda.anaconda.org/conda-forge/".to_string()],
            "the project's own conda-channels list replaces default_channels entirely"
        );
    }

    #[test]
    fn project_channels_override_naming_a_disallowed_channel_fails_without_calling_the_solver() {
        let fixture = Fixture::new(PYPROJECT_WITH_CONDA_CHANNELS);
        let solver = FakeSolver::new();

        let result = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        );

        assert!(matches!(
            result,
            Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
        ));
        assert!(solver.calls().is_empty());
        assert!(!fixture.environment(&[]).paths().lock_path.exists());
    }

    /// A channel-policy violation fails even when the section is
    /// otherwise fresh -- the check runs unconditionally, not only on
    /// the stale-solve path.
    #[test]
    fn project_channels_violation_fails_even_when_the_section_is_otherwise_fresh() {
        let fixture = Fixture::new(PYPROJECT_WITH_CONDA_CHANNELS);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &channels(&["conda-forge"])),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);

        let result = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        );

        assert!(matches!(
            result,
            Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
        ));
        assert_eq!(
            solver.calls().len(),
            1,
            "no second solve attempt once the policy check fails"
        );
    }

    #[test]
    fn per_package_channel_override_is_added_to_the_solvers_channels_when_allowed() {
        let fixture = Fixture::new(PYPROJECT_WITH_CHANNEL_OVERRIDE);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &channels(&["conda-forge"])),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].3,
            vec![
                "https://repo.anaconda.com/pkgs/main/".to_string(),
                "https://repo.anaconda.cloud/repo/main-x/".to_string(),
                "https://repo.anaconda.com/pkgs/r/".to_string(),
                "https://conda.anaconda.org/conda-forge/".to_string(),
            ]
        );
    }

    #[test]
    fn per_package_channel_override_fails_cleanly_when_not_allowed() {
        let fixture = Fixture::new(PYPROJECT_WITH_CHANNEL_OVERRIDE);
        let solver = FakeSolver::new();

        let result = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        );

        assert!(matches!(
            result,
            Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
        ));
        assert!(solver.calls().is_empty());
    }

    #[test]
    fn per_package_url_override_adds_its_matched_channel_when_allowed() {
        let fixture = Fixture::new(PYPROJECT_WITH_URL_OVERRIDE);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &channels(&["conda-forge"])),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].3,
            vec![
                "https://repo.anaconda.com/pkgs/main/".to_string(),
                "https://repo.anaconda.cloud/repo/main-x/".to_string(),
                "https://repo.anaconda.com/pkgs/r/".to_string(),
                "https://conda.anaconda.org/conda-forge/".to_string(),
            ],
            "the matched allow-set channel is added, not the raw package url"
        );
    }

    #[test]
    fn per_package_url_override_fails_cleanly_when_it_falls_under_no_allowed_channel() {
        let fixture = Fixture::new(PYPROJECT_WITH_URL_OVERRIDE);
        let solver = FakeSolver::new();

        let result = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        );

        assert!(matches!(
            result,
            Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
        ));
        assert!(solver.calls().is_empty());
    }

    #[test]
    fn base_channels_precede_override_channels_end_to_end() {
        let fixture = Fixture::new(PYPROJECT_WITH_CHANNEL_OVERRIDE);
        let solver = FakeSolver::new();

        ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(
                    &channels(&["defaults", "bioconda"]),
                    &channels(&["conda-forge"]),
                ),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        let calls = solver.calls();
        assert_eq!(
            calls[0].3,
            vec![
                "https://repo.anaconda.com/pkgs/main/".to_string(),
                "https://repo.anaconda.cloud/repo/main-x/".to_string(),
                "https://repo.anaconda.com/pkgs/r/".to_string(),
                "https://conda.anaconda.org/bioconda/".to_string(),
                "https://conda.anaconda.org/conda-forge/".to_string(),
            ],
            "base channels keep their own declared order, with the override appended last"
        );
    }

    /// A tightened `allowed_channels` fails `check` even for a section
    /// that would otherwise report `Valid`.
    #[test]
    fn check_channel_violation_fails_before_any_platform_status_is_computed() {
        let fixture = Fixture::new(PYPROJECT_WITH_CONDA_CHANNELS);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &channels(&["conda-forge"])),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let result = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        );

        assert!(matches!(
            result,
            Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
        ));
    }

    // -------------------------------------------------------------------
    // Channel *config* drift (`channels_digest`): `default_channels`/
    // `allowed_channels` reordered, or a channel added, between two
    // calls. `validate_locked_packages` alone cannot catch either case
    // when every already-locked package's `url` still happens to fall
    // under the new list (order is irrelevant to a URL-prefix check, and
    // an *added* channel that nothing currently locked needs never shows
    // up as a violation) -- `section_is_trustworthy`'s `channels_digest`
    // comparison is what catches these, independent of `packages`.
    // -------------------------------------------------------------------

    /// A `channel: None` locked record (produced only by a bare
    /// package-URL dependency) is checked against `policy.authorizes_artifact`
    /// rather than set membership -- an artifact under an `allowed_channels`
    /// wildcard prefix passes even though it names no channel at all.
    #[test]
    fn a_channel_none_locked_record_under_an_authorized_prefix_passes() {
        let fixture = Fixture::new(PYPROJECT);
        let env = fixture.environment(&[]);

        let prefix_policy = policy(&[], &channels(&["https://example.com/pkgs/main/*"]));
        let digest = prefix_policy.effective_channels(None, &[]).unwrap().digest;

        let mut numpy = fake_record_with_version("numpy", "1.0.0", CURRENT);
        numpy.channel = None;
        numpy.url = url::Url::parse(
            "https://example.com/pkgs/main/dev/linux-64/numpy-1.0.0-py312h1234567_0.conda",
        )
        .unwrap();
        let mut python = fake_record_with_version("python", "3.9.0", CURRENT);
        python.channel = None;
        python.url = url::Url::parse(
            "https://example.com/pkgs/main/dev/linux-64/python-3.9.0-py312h1234567_0.conda",
        )
        .unwrap();

        let section = PlatformSection {
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy >=1.20".to_string(),
                    source: "runtime".to_string(),
                },
                LockedRequirement {
                    matchspec: "python >=3.9".to_string(),
                    source: "requires-python".to_string(),
                },
            ],
            packages: vec![numpy, python],
            channels_digest: digest,
        };
        splice_section(&env.paths().lock_path, CURRENT, &section).unwrap();

        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(&[], &channels(&["https://example.com/pkgs/main/*"])),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        assert_eq!(report.platforms[&CURRENT], PlatformStatus::Valid);
    }

    /// Mirror image of the test above. `channel` names an authorized
    /// channel; `url` -- the field `rattler`'s installer and package
    /// cache actually fetch from -- points at a completely different,
    /// unauthorized host. `RepoDataRecord::channel` and `url` are
    /// independently-settable fields in `ana.lock`, so a hand-edited or
    /// malicious lock file can set them inconsistently exactly like
    /// this: `channel_matches_package_url` must reject it rather than
    /// trust `channel`'s claim.
    #[test]
    fn a_tampered_url_under_an_authorized_channel_string_is_rejected() {
        let fixture = Fixture::new(PYPROJECT);
        let env = fixture.environment(&[]);

        let allowed_channel = "https://conda.anaconda.org/conda-forge/";
        let allowed = policy(&channels(&["conda-forge"]), &[]);
        let digest = allowed.effective_channels(None, &[]).unwrap().digest;

        // `channel` is the exact, authorized "conda-forge" base url.
        // `url` -- what would actually be fetched -- is a different host
        // entirely, and is not itself under any authorized prefix.
        let numpy = fake_record_with_channel_and_url(
            "numpy",
            "1.20.0",
            CURRENT,
            allowed_channel,
            "https://evil.example.com/malicious/linux-64/numpy-1.20.0-py312h1234567_0.conda",
        );
        let python = fake_record_with_channel_and_url(
            "python",
            "3.9.0",
            CURRENT,
            allowed_channel,
            "https://evil.example.com/malicious/linux-64/python-3.9.0-py312h1234567_0.conda",
        );

        let section = PlatformSection {
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy >=1.20".to_string(),
                    source: "runtime".to_string(),
                },
                LockedRequirement {
                    matchspec: "python >=3.9".to_string(),
                    source: "requires-python".to_string(),
                },
            ],
            packages: vec![numpy, python],
            channels_digest: digest,
        };
        splice_section(&env.paths().lock_path, CURRENT, &section).unwrap();

        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &allowed,
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        // A package whose `url` is not under any authorized channel must
        // never be trusted, no matter what its `channel` field claims.
        assert_eq!(report.platforms[&CURRENT], PlatformStatus::Stale);
    }

    /// Every package `FakeSolver` returns is fetched from
    /// `repo.anaconda.com/pkgs/main`, which falls under the literal
    /// `"defaults"` entry regardless of where it sits in the channel
    /// list -- so `["conda-forge", "defaults"]` and `["defaults",
    /// "conda-forge"]` are indistinguishable to `validate_locked_packages`,
    /// even though they are two different, non-deterministic solve inputs
    /// (`rattler_solve::ChannelPriority::Flexible`; see `crate::channels`'s
    /// module docs).
    #[test]
    fn reordering_default_channels_is_detected_as_stale_even_though_every_locked_package_still_validates(
    ) {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&channels(&["conda-forge", "defaults"]), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&channels(&["defaults", "conda-forge"]), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        assert_eq!(
            report.platforms[&CURRENT],
            PlatformStatus::Stale,
            "a reordered channel list must be detected even when every already-locked \
             package's url still validates against it"
        );
    }

    #[test]
    fn adding_a_default_channel_is_detected_as_stale() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&channels(&["defaults", "conda-forge"]), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        assert_eq!(
            report.platforms[&CURRENT],
            PlatformStatus::Stale,
            "a channel added to default_channels since the section was solved must be \
             detected, even though no currently-locked package needs it"
        );
    }

    #[test]
    fn removing_a_default_channel_is_detected_as_stale() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&channels(&["defaults", "conda-forge"]), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        assert_eq!(report.platforms[&CURRENT], PlatformStatus::Stale);
    }

    #[test]
    fn ensure_current_platform_locked_resolves_again_when_only_the_channel_config_changed() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&channels(&["conda-forge", "defaults"]), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);

        // Requirements are untouched; only the channel order changed.
        let outcome = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&channels(&["defaults", "conda-forge"]), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(
            outcome,
            EnsureOutcome::Resolved,
            "reordering default_channels between runs must trigger a re-solve, not Fresh"
        );
        assert_eq!(solver.calls().len(), 2);
    }

    /// Two "developers" building the same project with differently
    /// spelled (but canonically equivalent) `allowed_channels`, and a
    /// pinned `[tool.ana] conda-channels` in `pyproject.toml`, must not
    /// see each other's commits as perpetual staleness -- the digest is
    /// independent of that spelling difference (see
    /// `ana_channels::tests::pinning_project_conda_channels_makes_the_digest_independent_of_default_channels`
    /// and the sibling per-package-override test), so a section one
    /// "developer" solves stays `Valid` for the other. Built directly
    /// (like the "malicious section" fixtures above), rather than through
    /// `FakeSolver`, which always returns a `repo.anaconda.com/pkgs/main`
    /// url unrelated to whichever channel was actually requested.
    #[test]
    fn pinned_project_channels_stay_fresh_across_differently_spelled_admin_configs() {
        let fixture = Fixture::new(PYPROJECT_WITH_CONDA_CHANNELS);
        let env = fixture.environment(&[]);

        // "Developer 1"'s admin config.
        let dev_1_digest = policy(&test_channels(), &channels(&["conda-forge"]))
            .effective_channels(env.channels(), &[])
            .unwrap()
            .digest;
        let section = PlatformSection {
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy >=1.20".to_string(),
                    source: "runtime".to_string(),
                },
                LockedRequirement {
                    matchspec: "python >=3.9".to_string(),
                    source: "requires-python".to_string(),
                },
            ],
            packages: vec![fake_record_with_channel_and_url(
                "numpy",
                "1.20.0",
                CURRENT,
                "https://conda.anaconda.org/conda-forge",
                "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.20.0-py312h1234567_0.conda",
            )],
            channels_digest: dev_1_digest,
        };
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &section,
        )
        .unwrap();

        // "Developer 2": a totally different, differently-spelled admin
        // config that still permits `conda-forge` (the project's own
        // pinned channel).
        let report = check(
            &env,
            &[],
            &SolveScope {
                channels: &policy(
                    &channels(&["bioconda"]),
                    &channels(&["https://conda.anaconda.org/conda-forge"]),
                ),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        assert_eq!(
            report.platforms[&CURRENT],
            PlatformStatus::Valid,
            "a pinned conda-channels project must not go stale from unrelated, \
             differently-spelled admin-config drift"
        );
    }

    // -------------------------------------------------------------------
    // Channel policy never covers an already-locked `PlatformSection`'s
    // `packages` -- only *declared* requirement-level overrides
    // (`conda-channels`, `channel::`, `url=`). `requirements_match`
    // (the `Fresh`/`Valid` fast path every mode above takes before ever
    // touching the solver) is a pure string-set comparison over
    // `requirements`; it never inspects `packages` at all. So a
    // `PlatformSection.packages` entry whose `channel`/`url` names a
    // channel entirely absent from `default_channels ∪ allowed_channels`
    // is currently accepted silently whenever the section's
    // `requirements` happen to match `pyproject.toml`'s current
    // conversion -- regardless of how that mismatch between "declared
    // requirement" and "already recorded package" came about: an
    // attacker distributing a matching `pyproject.toml`/`ana.lock` pair
    // from the start, a `git pull` that updates `ana.lock`'s packages
    // without touching `pyproject.toml`, or a direct hand-edit of
    // `ana.lock` outside of `ana` entirely.
    //
    // The required behavior (`section_is_trustworthy`): such a section is
    // never trusted as `Fresh`/`Valid`. Without `--frozen`, it is treated
    // exactly like ordinary staleness -- discarded and re-solved, which
    // splices a fresh, policy-clean section in over it (`ana.lock` is
    // never left holding the tampered content, and there is no separate
    // "delete the file" step: replacing the section *is* starting over).
    // With `--frozen`, which never re-solves or writes anything, the call
    // fails with the specific `Error::Channels(ana_channels::Error::ChannelNotAllowed(_))`, not a generic
    // staleness message -- `--frozen` must not mask a security-relevant
    // rejection.
    // -------------------------------------------------------------------

    /// An attacker distributes a `pyproject.toml`/`ana.lock` pair
    /// together from the very first checkout -- there is no "before" for
    /// `ensure_current_platform` to have ever solved anything itself, and
    /// no local env-lock state either. `ana.lock`'s `requirements` were
    /// crafted to textually match what `pyproject.toml` converts to, but
    /// the locked package still names a channel outside
    /// `default_channels ∪ allowed_channels` -- without `--frozen`, this
    /// must be re-solved from scratch rather than trusted, exactly as if
    /// no lock existed at all.
    #[test]
    fn initial_malicious_pyproject_and_lock_pair_is_discarded_and_re_solved() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        // The exact canonical requirement set `PYPROJECT` converts to for
        // `CURRENT` with no groups selected (see
        // `no_lock_resolves_and_writes_lock`), hand-assembled the way an
        // attacker crafting both files together would have to: by
        // reading what `ana` itself would produce and matching it
        // exactly, with no help from this crate.
        let malicious_section = PlatformSection {
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy >=1.20".to_string(),
                    source: "runtime".to_string(),
                },
                LockedRequirement {
                    matchspec: "python >=3.9".to_string(),
                    source: "requires-python".to_string(),
                },
            ],
            packages: vec![fake_record_with_channel_and_url(
                "numpy",
                "1.99.0",
                CURRENT,
                "https://packages.evil-corp.example/channel",
                "https://packages.evil-corp.example/channel/linux-64/numpy-1.99.0-0.conda",
            )],
            channels_digest: test_channels_digest(),
        };
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &malicious_section,
        )
        .unwrap();

        let outcome = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(
            outcome,
            EnsureOutcome::Resolved,
            "a locked package naming a disallowed channel must never be trusted as Fresh, \
             even when `requirements` happen to already match `pyproject.toml`"
        );
        assert_eq!(
            solver.calls().len(),
            1,
            "the discarded section is re-solved for real"
        );
        let section = &fixture.lock(&[]).platforms[&CURRENT];
        assert!(
            section
                .packages
                .iter()
                .all(|p| p.channel.as_deref() != Some("https://packages.evil-corp.example/channel")),
            "the malicious package must not survive the re-solve: {:?}",
            section.packages
        );
    }

    /// Same scenario, but with `--frozen`: since a frozen run may never
    /// re-solve or write `ana.lock`, the only correct outcome is a hard
    /// failure -- and specifically the channel-policy error, not the
    /// generic `Error::Frozen` staleness message, so an operator running
    /// `--frozen` in CI actually learns *why* rather than being told to
    /// "run without --frozen to update the lock" (which would silently
    /// paper over the violation).
    #[test]
    fn initial_malicious_pyproject_and_lock_pair_is_rejected_under_frozen() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        let malicious_section = PlatformSection {
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy >=1.20".to_string(),
                    source: "runtime".to_string(),
                },
                LockedRequirement {
                    matchspec: "python >=3.9".to_string(),
                    source: "requires-python".to_string(),
                },
            ],
            packages: vec![fake_record_with_channel_and_url(
                "numpy",
                "1.99.0",
                CURRENT,
                "https://packages.evil-corp.example/channel",
                "https://packages.evil-corp.example/channel/linux-64/numpy-1.99.0-0.conda",
            )],
            channels_digest: test_channels_digest(),
        };
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &malicious_section,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        let result = ensure_current_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            true,
        );

        assert!(
            matches!(
                result,
                Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
            ),
            "frozen must hard-fail with the specific channel error, never re-solve: {result:?}"
        );
        assert!(solver.calls().is_empty(), "frozen never solves");
        assert_eq!(
            fixture.lock_text(&[]),
            lock_before,
            "frozen never writes, even to discard a tampered section"
        );
    }

    /// A `git pull` (or an attacker with direct filesystem/repo write
    /// access) can replace `ana.lock`'s `packages` for a platform that
    /// was legitimately solved moments earlier, while leaving its
    /// `requirements` untouched. The requirement-string comparison alone
    /// cannot tell "nothing changed" apart from "the packages were
    /// swapped out from under us" -- the channel-policy check must still
    /// catch the swapped-in package and force a re-solve regardless.
    #[test]
    fn git_pull_of_a_hand_edited_lock_is_discarded_and_re_solved() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        // A real, policy-compliant solve first.
        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);

        // The "hand-edit"/"git pull": only `packages` changes, to a
        // channel/url that was never, and still isn't, permitted by
        // `default_channels ∪ allowed_channels`. This models a plain-text
        // edit to the committed `ana.lock` file just as faithfully as an
        // actual text editor would -- `splice_section` only performs the
        // same TOML rewrite an external tool could byte-for-byte
        // reproduce; nothing about the resulting file distinguishes it
        // from a genuine hand-edit.
        let mut tampered = fixture.lock(&[]).platforms[&CURRENT].clone();
        tampered.packages = vec![fake_record_with_channel_and_url(
            "numpy",
            "1.99.0",
            CURRENT,
            "https://packages.evil-corp.example/channel",
            "https://packages.evil-corp.example/channel/linux-64/numpy-1.99.0-0.conda",
        )];
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &tampered,
        )
        .unwrap();

        let outcome = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();

        assert_eq!(
            outcome,
            EnsureOutcome::Resolved,
            "an already-locked package swapped in from a disallowed channel must never \
             be trusted, even though `requirements` never changed"
        );
        assert_eq!(
            solver.calls().len(),
            2,
            "the tampered section is re-solved for real"
        );
        let section = &fixture.lock(&[]).platforms[&CURRENT];
        assert!(
            section
                .packages
                .iter()
                .all(|p| p.channel.as_deref() != Some("https://packages.evil-corp.example/channel")),
            "the malicious package must not survive the re-solve: {:?}",
            section.packages
        );
    }

    /// Same "hand-edit"/`git pull` scenario, but with `--frozen`: must
    /// hard-fail with the channel-policy error rather than silently
    /// re-solving (which `--frozen`'s whole contract forbids) or trusting
    /// the tampered section as `Fresh`.
    #[test]
    fn git_pull_of_a_hand_edited_lock_is_rejected_under_frozen() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();
        let env = fixture.environment(&[]);

        ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            false,
        )
        .unwrap();
        assert_eq!(solver.calls().len(), 1);

        let mut tampered = fixture.lock(&[]).platforms[&CURRENT].clone();
        tampered.packages = vec![fake_record_with_channel_and_url(
            "numpy",
            "1.99.0",
            CURRENT,
            "https://packages.evil-corp.example/channel",
            "https://packages.evil-corp.example/channel/linux-64/numpy-1.99.0-0.conda",
        )];
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &tampered,
        )
        .unwrap();
        let lock_before = fixture.lock_text(&[]);

        let result = ensure_current_platform(
            &env,
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
            true,
        );

        assert!(
            matches!(
                result,
                Err(Error::Channels(ana_channels::Error::ChannelNotAllowed(_)))
            ),
            "{result:?}"
        );
        assert_eq!(solver.calls().len(), 1, "frozen never re-solves");
        assert_eq!(fixture.lock_text(&[]), lock_before, "frozen never writes");
    }

    /// [`check`]'s offline `Valid`/`Stale` verdict must have the same
    /// guarantee as [`ensure_current_platform`]: `Valid` means "the
    /// requirements match *and* every locked package's channel/url is
    /// still authorized", not just the former. A section that fails the
    /// channel check folds into the same `Stale` verdict as ordinary
    /// requirement drift -- `check` doesn't distinguish why a section
    /// isn't trustworthy, only reports it -- so `--fix` re-solves it via
    /// the exact same, already-safe path as any other stale platform.
    #[test]
    fn check_reports_stale_for_a_maliciously_tampered_locked_package() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let mut tampered = fixture.lock(&[]).platforms[&CURRENT].clone();
        tampered.packages = vec![fake_record_with_channel_and_url(
            "numpy",
            "1.99.0",
            CURRENT,
            "https://packages.evil-corp.example/channel",
            "https://packages.evil-corp.example/channel/linux-64/numpy-1.99.0-0.conda",
        )];
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &tampered,
        )
        .unwrap();

        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            false,
            None,
        )
        .unwrap();

        assert_eq!(
            report.platforms[&CURRENT],
            PlatformStatus::Stale,
            "a locked package naming a disallowed channel must never report `Valid`"
        );
        assert!(!report.is_fresh());
    }

    /// The `--fix` half of the same guarantee: a maliciously tampered
    /// section is not just *reported* `Stale`, it is actually repaired --
    /// re-solved through the ordinary, already channel-restricted solve
    /// path, landing a clean section in `ana.lock`.
    #[test]
    fn check_fix_repairs_a_maliciously_tampered_locked_package() {
        let fixture = Fixture::new(PYPROJECT);
        let solver = FakeSolver::new();

        lock_platform(
            &fixture.environment(&[]),
            CURRENT,
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            &solver,
        )
        .unwrap();

        let mut tampered = fixture.lock(&[]).platforms[&CURRENT].clone();
        tampered.packages = vec![fake_record_with_channel_and_url(
            "numpy",
            "1.99.0",
            CURRENT,
            "https://packages.evil-corp.example/channel",
            "https://packages.evil-corp.example/channel/linux-64/numpy-1.99.0-0.conda",
        )];
        splice_section(
            &fixture.environment(&[]).paths().lock_path,
            CURRENT,
            &tampered,
        )
        .unwrap();

        let report = check(
            &fixture.environment(&[]),
            &[],
            &SolveScope {
                channels: &policy(&test_channels(), &[]),
                pypi_to_conda_map: &no_mapping(),
            },
            true,
            Some(&solver),
        )
        .unwrap();

        assert!(report.is_fresh(), "the repaired section is reported Valid");
        let section = &fixture.lock(&[]).platforms[&CURRENT];
        assert!(
            section
                .packages
                .iter()
                .all(|p| p.channel.as_deref() != Some("https://packages.evil-corp.example/channel")),
            "the malicious package must not survive `check --fix`: {:?}",
            section.packages
        );
    }
}
