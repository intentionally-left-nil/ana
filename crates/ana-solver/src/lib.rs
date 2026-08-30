//! A [`ana_lockfile::Solver`] backed by `rattler_solve`'s `resolvo` backend,
//! `rattler_repodata_gateway`'s channel repodata fetching, and
//! `rattler_virtual_packages`'s virtual-package detection, built against
//! `https://github.com/intentionally-left-nil/rattler` (a fork of
//! `conda/rattler`, pinned at the workspace `Cargo.toml`'s `rev`).
//!
//! [`ana_lockfile::Solver`] is a plain, synchronous trait with no solver
//! crate as a compile-time dependency; this crate is the real impl wired
//! in by whoever constructs `ana-lockfile`'s entry points (see `ana`'s own
//! `run.rs`).
//!
//! One [`RattlerSolver`] is meant to be built once per process and shared
//! (by reference) across every solve: it owns a `tokio` runtime (bridging
//! the gateway's async network I/O into the trait's sync `solve` method)
//! and a `rattler_repodata_gateway::Gateway`, which caches fetched
//! repodata across calls.
//!
//! What one [`RattlerSolver::solve`] call does, end to end:
//!
//! 1. Resolve `request.channels`' bare names to real
//!    [`rattler_conda_types::Channel`]s ([`channels::resolve`]) --
//!    `"defaults"` is hardcoded to Anaconda's own
//!    `repo.anaconda.com/pkgs/*` meta-channel; every other name resolves
//!    generically.
//! 2. Fetch repodata for `request.platform` *and* `noarch`, recursively --
//!    the whole dependency closure of `request.specs`, not just their own
//!    records.
//! 3. Detect `request.platform`'s virtual packages
//!    (`rattler_virtual_packages::VirtualPackages::detect_for_platform`).
//! 4. Solve, biasing towards `request.preferred` (matched back against the
//!    records just fetched -- see [`Solver::solve`]'s docs for why a
//!    stored [`rattler_conda_types::RepoDataRecord`] is re-matched by
//!    identity rather than trusted as-is).
//! 5. Return each winning `RepoDataRecord` directly -- the shape
//!    `ana_lockfile::PlatformSection` stores end to end (a bare
//!    `PackageRecord` alone carries no `url` to install from).
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod channels;
mod error;
mod progress;

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
use progress::FetchProgress;

/// A real, network-backed [`Solver`] -- see the module docs for what one
/// [`solve`](Solver::solve) call does.
pub struct RattlerSolver {
    /// Bridges `rattler_repodata_gateway::Gateway`'s async API into
    /// [`Solver::solve`]'s plain synchronous one. Shared with the rest of
    /// the process rather than owned per-solver.
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
    /// names resolve relative to `root_dir`.
    ///
    /// `runtime_handle` and `client` are supplied by the caller (`main.rs`)
    /// rather than built here: one `tokio::runtime::Runtime` and one
    /// `LazyClient` are shared process-wide across this solver,
    /// `ana-installer`'s downloads, and `ana-pypi-conda-map`'s mapping
    /// refresh.
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
/// method) so it borrows only `&Gateway`/`&ChannelConfig`, not
/// `&RattlerSolver` itself.
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
    // this solve needs. The one clone below is structural: `Gateway::query`
    // consumes one owned `Vec<MatchSpec>`, and `SolverTask::specs` needs
    // its own independent copy afterwards.
    let specs = request.specs;
    let expected_fetches = channels.len() * platforms.len();
    let fetch_progress = FetchProgress::new(expected_fetches);
    let query_output = gateway
        .query(channels, platforms, specs.clone())
        .recursive(true)
        .with_reporter(fetch_progress)
        .await?;

    // Borrowed, not cloned: `query_output` outlives the rest of this
    // function, so there is no need to deep-copy every fetched record
    // just to get an owned collection.
    let available: Vec<&RepoDataRecord> = query_output.iter().flat_map(RepoData::iter).collect();

    // Indexed by "name-version-build" rather than scanned per lookup:
    // `PackageRecord`'s `PartialEq` compares every field (hashes,
    // `run_exports`, timestamps, ...), so a record the channel
    // re-published with only a metadata correction would silently stop
    // matching its own previous self.
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

    // Bias the solve towards the previous lock section's records,
    // matched back by identity (name-version-build) rather than trusted
    // as-is: the freshly-fetched record for the same identity always
    // wins over the one carried in from the lock, since a stored
    // record's `url` can go stale. A preferred record no longer present
    // upstream is simply not favored, never an error.
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
        // `RepoDataIter` is `rattler_solve`'s own wrapper for handing a
        // solver borrowed records directly, without collecting them
        // into an owned `Vec<RepoDataRecord>` first.
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
    let solving_line = ana_progress::StatusLine::new();
    let result = progress::solve_label(&solving_line, move || backend.solve(task))?;

    // The full `RepoDataRecord`s, unmodified -- `ana_lockfile::PlatformSection`
    // stores exactly this shape, `url` included, not a bare `PackageRecord`.
    Ok(result.records)
}
