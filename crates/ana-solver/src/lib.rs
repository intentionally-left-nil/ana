//! A [`ana_lockfile::Solver`] backed by `rattler_solve`'s `resolvo` backend,
//! `rattler_repodata_gateway`'s channel repodata fetching, and
//! `rattler_virtual_packages`'s virtual-package detection -- the "Open
//! TODOs" solver crate `investigations/lock_generation_algorithm.md` left
//! for the implementer, filled in against
//! `https://github.com/intentionally-left-nil/rattler` (a fork of
//! `conda/rattler`, pinned at the workspace `Cargo.toml`'s `rev`, per that
//! fork's own request in place of the crates.io releases).
//!
//! [`ana_lockfile::Solver`] (the seam `ana-lockfile` is written against) is
//! a plain, synchronous trait: no solver crate is a compile-time
//! dependency of that crate, and its algorithm is tested entirely with
//! fakes. This crate is the caller-side implementation the seam's own docs
//! describe -- a real [`Solver`] impl, wired in by whoever constructs
//! `ana-lockfile`'s entry points (see `ana`'s own `run.rs`, which passes a
//! `solver: &dyn Solver` through unchanged).
//!
//! One [`RattlerSolver`] is meant to be built once per process and shared
//! (by reference) across every solve: it owns a `tokio` runtime (used to
//! bridge the gateway's async network I/O into the trait's sync `solve`
//! method, per [`ana_lockfile::Solver::solve`]'s own contract) and a
//! `rattler_repodata_gateway::Gateway`, which caches fetched repodata
//! across calls -- rebuilding either per solve would throw that cache away
//! and pay for a fresh runtime every time.
//!
//! What one [`RattlerSolver::solve`] call does, end to end, for a single
//! [`ana_lockfile::SolveRequest`]:
//!
//! 1. Resolve `request.channels`' bare names (always `["defaults"]` today,
//!    per [`ana_lockfile::DEFAULT_CHANNELS`]) to real
//!    [`rattler_conda_types::Channel`]s ([`channels::resolve`]) --
//!    `"defaults"` itself is hardcoded to Anaconda's own
//!    `repo.anaconda.com/pkgs/*` meta-channel (see that module's docs for
//!    why the generic alias resolution 404s for it), every other name
//!    resolves generically.
//! 2. Fetch repodata for `request.platform` *and* `noarch` (every conda
//!    solve needs both subdirs, regardless of which one is being solved
//!    for), recursively -- the whole dependency closure of `request.specs`
//!    (which already includes a `python` constraint if `pyproject.toml`
//!    declares `requires-python`: turning that into a matchspec is
//!    `ana_lockfile`'s job, upstream of this crate, not this crate's --
//!    see [`ana_lockfile::SolveRequest::specs`]'s own docs), not just
//!    their own records.
//! 3. Detect `request.platform`'s virtual packages
//!    (`rattler_virtual_packages::VirtualPackages::detect_for_platform`,
//!    which already knows how to report sane baseline values for a
//!    platform other than the host machine's own -- exactly the
//!    cross-platform-mode case
//!    `investigations/lock_generation_algorithm.md` describes).
//! 4. Solve, biasing towards `request.preferred` (matched back against the
//!    records just fetched -- see [`Solver::solve`]'s docs for why a
//!    stored [`rattler_conda_types::RepoDataRecord`] is re-matched by
//!    identity rather than trusted as-is).
//! 5. Return each winning `RepoDataRecord` directly -- the shape
//!    `ana_lockfile::PlatformSection` now stores end to end (see
//!    `investigations/package_download_and_install_implementation_plan.md`'s
//!    "New finding": a bare `PackageRecord` alone carries no `url` to
//!    install from).
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod channels;
mod error;

use std::collections::HashMap;
use std::path::PathBuf;

use ana_lockfile::{SolveRequest, Solver};
use rattler_conda_types::{ChannelConfig, PackageRecord, Platform, RepoDataRecord};
use rattler_networking::LazyClient;
use rattler_repodata_gateway::{Gateway, RepoData};
use rattler_solve::{
    resolvo, ChannelPriority, RepoDataIter, SolveStrategy, SolverImpl, SolverTask,
};
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};

pub use error::Error;

/// A real, network-backed [`Solver`] -- see the module docs for what one
/// [`solve`](Solver::solve) call does.
pub struct RattlerSolver {
    /// Bridges `rattler_repodata_gateway::Gateway`'s async API into
    /// [`Solver::solve`]'s plain synchronous one. Shared with the rest of
    /// the process (`ana-installer`'s downloads, `ana-pypi-conda-map`'s
    /// mapping refresh) rather than owned per-solver, per
    /// `investigations/package_download_and_install_implementation_plan.md`'s
    /// Phase 5 -- `main.rs` builds one runtime and one
    /// `ana_installer::Downloader` (whose client this solver's `Gateway`
    /// also uses) for the whole process.
    runtime_handle: tokio::runtime::Handle,
    /// Fetches and caches channel repodata across every solve this
    /// instance performs.
    gateway: Gateway,
    /// Resolves `SolveRequest::channels`' bare names (`"defaults"`) to
    /// real channel URLs.
    channel_config: ChannelConfig,
}

impl RattlerSolver {
    /// Builds a solver whose fetched repodata is cached under `cache_dir`
    /// (created lazily on first use if missing) and whose bare channel
    /// names resolve relative to `root_dir` -- [`ChannelConfig`]'s own
    /// requirement for resolving a *local-path* channel, which `ana`
    /// itself never actually produces (every channel `ana-lockfile` asks
    /// for today is the bare alias `"defaults"`, per
    /// [`ana_lockfile::DEFAULT_CHANNELS`]); callers typically pass the
    /// project root here anyway, since that is the one directory already
    /// on hand at every call site.
    ///
    /// `runtime_handle` and `client` are supplied by the caller (`main.rs`)
    /// rather than built here: one `tokio::runtime::Runtime` and one
    /// `LazyClient` (with retry middleware) are shared process-wide across
    /// this solver, `ana-installer`'s downloads, and
    /// `ana-pypi-conda-map`'s mapping refresh, instead of three
    /// independent thread pools/HTTP clients in one process.
    pub fn new(
        cache_dir: PathBuf,
        root_dir: PathBuf,
        runtime_handle: tokio::runtime::Handle,
        client: LazyClient,
    ) -> Self {
        let gateway = Gateway::builder()
            .with_cache_dir(cache_dir)
            .with_client(client)
            .finish();
        let channel_config = ChannelConfig::default_with_root_dir(root_dir);
        Self {
            runtime_handle,
            gateway,
            channel_config,
        }
    }
}

impl Solver for RattlerSolver {
    fn solve(
        &self,
        request: SolveRequest<'_>,
    ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
        self.runtime_handle
            .block_on(solve(&self.gateway, &self.channel_config, request))
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
    }
}

/// The async body of [`RattlerSolver::solve`] -- a free function (not a
/// method) so it borrows exactly the two fields it needs
/// (`&Gateway`/`&ChannelConfig`), not `&RattlerSolver` itself, which would
/// otherwise tie its lifetime to a `&self` the shared `tokio::runtime::Handle`
/// living alongside those fields has no bearing on.
async fn solve(
    gateway: &Gateway,
    channel_config: &ChannelConfig,
    request: SolveRequest<'_>,
) -> Result<Vec<RepoDataRecord>, Error> {
    let channels = channels::resolve(&request.channels, channel_config, request.platform)?;

    // Every conda solve needs `noarch`'s records too, regardless of which
    // platform is being solved for -- `noarch` packages live in their own
    // subdir, independent of `request.platform`.
    let mut platforms = vec![request.platform];
    if request.platform != Platform::NoArch {
        platforms.push(Platform::NoArch);
    }

    // Moved, not cloned: `request.specs` already carries every constraint
    // this solve needs -- `python`, from `pyproject.toml`'s
    // `requires-python`, included (turning that into a matchspec is
    // `ana_lockfile`'s job, upstream of this crate -- see
    // `ana_lockfile::SolveRequest::specs`'s own docs) -- so there is no
    // local, mutated copy to build here the way this crate once needed
    // when it special-cased `requires_python` itself. The one clone
    // below is structural, not incidental: `Gateway::query` consumes one
    // owned `Vec<MatchSpec>`, and `SolverTask::specs` needs its own,
    // independent owned copy of the same specs afterwards -- there is no
    // way to hand the same `Vec` to both.
    let specs = request.specs;
    let query_output = gateway
        .query(channels, platforms, specs.clone())
        .recursive(true)
        .await?;

    // Borrowed, not cloned: `query_output` (and the `Arc<RepoDataRecord>`s
    // it owns internally) lives for the rest of this function, so there is
    // no need to deep-copy every field of every fetched record just to get
    // an owned collection -- the recursive closure of a solve can be a
    // large number of records, and this runs on every solve.
    let available: Vec<&RepoDataRecord> = query_output.iter().flat_map(RepoData::iter).collect();

    // Indexed by "name-version-build" (a package's real identity within a
    // subdir) rather than scanned per lookup: matching each of the
    // (typically far fewer) `preferred` records against `available` with
    // a linear `find` would be O(preferred * available) *and* fragile,
    // since `PackageRecord`'s `PartialEq` compares every field (hashes,
    // `run_exports`, timestamps, ...) -- a record the channel re-published
    // with only a metadata correction (e.g. a repodata patch) would
    // silently stop matching its own previous self, even though it's
    // still "the same" package as far as a solve is concerned.
    fn identity_key(record: &PackageRecord) -> String {
        format!(
            "{}-{}-{}",
            record.name.as_normalized(),
            record.version,
            record.build
        )
    }
    let available_by_identity: HashMap<String, &RepoDataRecord> = available
        .iter()
        .map(|record| (identity_key(&record.package_record), *record))
        .collect();

    // Bias the solve towards the previous lock section's records --
    // `SolveRequest::preferred`'s whole reason for existing (so a
    // re-resolve tends to reproduce the previous answer wherever it's
    // still legal). Matched back against the records just fetched by
    // identity (name-version-build), not trusted as-is even though the
    // stored record is now a full `RepoDataRecord`: a previously-locked
    // record's `url` can go stale (channel repodata patched, package
    // pulled) in a way name/version/build alone wouldn't catch, so the
    // freshly-fetched record for the same identity always wins over the
    // one carried in from the lock. A preferred record no longer present
    // upstream is simply not favored, never an error. Only ever read
    // (`request.preferred` is a borrowed slice, not an owned `Vec`),
    // never cloned.
    let favored: Vec<&RepoDataRecord> = request
        .preferred
        .iter()
        .filter_map(|preferred| available_by_identity.get(&identity_key(&preferred.package_record)))
        .copied()
        .collect();

    let virtual_packages = VirtualPackages::detect_for_platform(
        request.platform,
        &VirtualPackageOverrides::default(),
        None,
    )?
    .into_generic_virtual_packages()
    .collect();

    let task = SolverTask {
        // `available` is `Vec<&RepoDataRecord>` (borrowed, never cloned --
        // see above); `RepoDataIter` is `rattler_solve`'s own wrapper for
        // handing a solver borrowed records directly, without collecting
        // them into an owned `Vec<RepoDataRecord>` first.
        available_packages: vec![RepoDataIter(available.iter().copied())],
        locked_packages: favored,
        pinned_packages: Vec::new(),
        virtual_packages,
        specs,
        constraints: Vec::new(),
        timeout: None,
        channel_priority: ChannelPriority::default(),
        exclude_newer: None,
        strategy: SolveStrategy::default(),
        dependency_overrides: Vec::new(),
        excluded_candidates: HashMap::new(),
        cancellation_token: None,
    };

    let mut backend = resolvo::Solver;
    let result = backend.solve(task)?;

    // The full `RepoDataRecord`s, unmodified -- `ana_lockfile::PlatformSection`
    // stores exactly this shape now, `url`/`channel`/`identifier` included
    // (see `investigations/package_download_and_install_implementation_plan.md`'s
    // "New finding"), rather than unwrapping down to a bare `PackageRecord`.
    Ok(result.records)
}
