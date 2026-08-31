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
//! This crate knows nothing about channel identity or policy:
//! `request.channels` already carries real, resolved
//! [`rattler_conda_types::Channel`]s (via `ana_channels::ChannelPolicy`),
//! so there is no bare-name/`"defaults"` resolution of its own to do.
//!
//! What one [`RattlerSolver::solve`] call does, end to end:
//!
//! 1. Fetch repodata for `request.platform` *and* `noarch`, recursively --
//!    the whole dependency closure of `request.specs`, not just their own
//!    records.
//! 2. Detect `request.platform`'s virtual packages
//!    (`rattler_virtual_packages::VirtualPackages::detect_for_platform`).
//! 3. Solve, biasing towards `request.preferred` (matched back against the
//!    records just fetched -- see [`build_solver_task`]'s docs for why a
//!    stored [`rattler_conda_types::RepoDataRecord`] is re-matched by
//!    identity rather than trusted as-is).
//! 4. Return each winning `RepoDataRecord` directly -- the shape
//!    `ana_lockfile::PlatformSection` stores end to end (a bare
//!    `PackageRecord` alone carries no `url` to install from).
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod error;
mod progress;

use std::collections::HashMap;
use std::path::PathBuf;

use ana_lockfile::{SolveRequest, Solver};
use rattler_conda_types::{GenericVirtualPackage, PackageRecord, Platform, RepoDataRecord};
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
}

impl RattlerSolver {
    /// Builds a solver whose fetched repodata is cached under `cache_dir`
    /// (created lazily on first use if missing).
    ///
    /// `runtime_handle` and `client` are supplied by the caller (`main.rs`)
    /// rather than built here: one `tokio::runtime::Runtime` and one
    /// `LazyClient` are shared process-wide across this solver,
    /// `ana-installer`'s downloads, and `ana-pypi-conda-map`'s mapping
    /// refresh.
    pub fn new(
        cache_dir: PathBuf,
        runtime_handle: tokio::runtime::Handle,
        client: LazyClient,
    ) -> Self {
        let gateway = Gateway::builder()
            .with_cache_dir(cache_dir)
            .with_client(client)
            .finish();
        Self {
            runtime_handle,
            gateway,
        }
    }
}

impl Solver for RattlerSolver {
    fn solve(
        &self,
        request: SolveRequest<'_>,
    ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
        self.runtime_handle
            .block_on(solve(&self.gateway, request))
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)
    }
}

/// The async body of [`RattlerSolver::solve`] -- a free function (not a
/// method) so it borrows only `&Gateway`, not `&RattlerSolver` itself.
async fn solve(gateway: &Gateway, request: SolveRequest<'_>) -> Result<Vec<RepoDataRecord>, Error> {
    let channels = request.channels;

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

    let virtual_packages = VirtualPackages::detect_for_platform(
        request.platform,
        &VirtualPackageOverrides::default(),
        None,
    )?
    .into_generic_virtual_packages()
    .collect();

    let task = build_solver_task(specs, &available, request.preferred, virtual_packages);

    let mut backend = resolvo::Solver;
    let solving_line = ana_progress::StatusLine::new();
    let result = progress::solve_label(&solving_line, move || backend.solve(task))?;

    // The full `RepoDataRecord`s, unmodified -- `ana_lockfile::PlatformSection`
    // stores exactly this shape, `url` included, not a bare `PackageRecord`.
    Ok(result.records)
}

/// Builds the `SolverTask` for `specs` against `available`, biased toward
/// `preferred`. Pure construction -- no gateway, no network -- so it can
/// be exercised directly by this module's own regression tests.
///
/// `preferred` (the previous lock section's records) is matched back into
/// `available` by identity (name-version-build) rather than trusted
/// as-is: the freshly-fetched record for the same identity always wins
/// over the one carried in from the lock, since a stored record's `url`
/// can go stale. A preferred record no longer present upstream is simply
/// not favored, never an error.
///
/// Indexed by "name-version-build" rather than scanned per lookup:
/// `PackageRecord`'s `PartialEq` compares every field (hashes,
/// `run_exports`, timestamps, ...), so a record the channel re-published
/// with only a metadata correction would silently stop matching its own
/// previous self.
fn build_solver_task<'r>(
    specs: Vec<rattler_conda_types::MatchSpec>,
    available: &'r [&'r RepoDataRecord],
    preferred: &[RepoDataRecord],
    virtual_packages: Vec<GenericVirtualPackage>,
) -> SolverTask<'r, Vec<RepoDataIter<std::iter::Copied<std::slice::Iter<'r, &'r RepoDataRecord>>>>>
{
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

    let favored: Vec<&RepoDataRecord> = preferred
        .iter()
        .filter_map(|record| available_by_identity.get(&identity_key(&record.package_record)))
        .copied()
        .collect();

    SolverTask {
        // `RepoDataIter` is `rattler_solve`'s own wrapper for handing a
        // solver borrowed records directly, without collecting them
        // into an owned `Vec<&RepoDataRecord>` first.
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
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{MatchSpec, ParseMatchSpecOptions, Version};

    use super::*;

    /// A minimal, complete [`RepoDataRecord`] for `name-version`, fetched
    /// (as far as this test is concerned) from `channel_url` -- stamped
    /// with `channel: Some(channel_url)`, exactly the way
    /// `rattler_repodata_gateway`'s own `sparse::mod::parse_records_raw`
    /// stamps a real fetch's records.
    fn record(name: &str, version: &str, channel_url: &str) -> RepoDataRecord {
        let mut package_record = PackageRecord::new(
            rattler_conda_types::PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            "0".to_string(),
        );
        package_record.subdir = "linux-64".to_string();
        let identifier =
            DistArchiveIdentifier::try_from_filename(&format!("{name}-{version}-0.conda")).unwrap();
        let url =
            url::Url::parse(&format!("{channel_url}linux-64/{name}-{version}-0.conda")).unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url,
            channel: Some(channel_url.to_string()),
        }
    }

    fn matchspec(text: &str) -> MatchSpec {
        let mut spec = MatchSpec::from_str(text, ParseMatchSpecOptions::lenient()).unwrap();
        if let Some(channel) = spec.channel.take() {
            let normalized = ana_channels::normalize_channel((*channel).clone()).unwrap();
            spec.channel = Some(std::sync::Arc::new(normalized));
        }
        spec
    }

    /// The regression test for the channel-identity bug this plan fixes:
    /// a spec pinned to a normalized channel (`main::conda`, whose
    /// `spec.channel.canonical_name()` is
    /// `https://repo.anaconda.com/pkgs/main/`) must accept a candidate
    /// record stamped with that exact same string -- the way the gateway
    /// stamps a record fetched from that same normalized channel's own
    /// URL, never rattler's own generic `conda.anaconda.org` alias.
    #[test]
    fn a_pinned_spec_accepts_a_candidate_stamped_the_way_the_gateway_stamps_it() {
        let spec = matchspec("main::conda");
        let candidate = record("conda", "1.0.0", "https://repo.anaconda.com/pkgs/main/");
        let available = [&candidate];

        let task = build_solver_task(vec![spec], &available, &[], Vec::new());

        let mut backend = resolvo::Solver;
        let result = backend.solve(task).unwrap();
        assert!(
            result
                .records
                .iter()
                .any(|r| r.package_record.name.as_normalized() == "conda"),
            "{:?}",
            result.records
        );
    }

    /// A candidate from a different channel is excluded for that same
    /// pinned spec.
    #[test]
    fn a_pinned_spec_excludes_a_candidate_from_a_different_channel() {
        let spec = matchspec("main::conda");
        let candidate = record("conda", "1.0.0", "https://conda.anaconda.org/conda-forge/");
        let available = [&candidate];

        let task = build_solver_task(vec![spec], &available, &[], Vec::new());

        let mut backend = resolvo::Solver;
        let result = backend.solve(task);
        assert!(result.is_err(), "{result:?}");
    }

    /// An unpinned package is unaffected by another spec's channel pin.
    #[test]
    fn an_unpinned_package_is_unaffected_by_a_pin_on_another_package() {
        let pinned_spec = matchspec("main::conda");
        let unpinned_spec = matchspec("numpy");
        let pinned_candidate = record("conda", "1.0.0", "https://repo.anaconda.com/pkgs/main/");
        let unpinned_candidate =
            record("numpy", "1.0.0", "https://conda.anaconda.org/conda-forge/");
        let available = [&pinned_candidate, &unpinned_candidate];

        let task = build_solver_task(
            vec![pinned_spec, unpinned_spec],
            &available,
            &[],
            Vec::new(),
        );

        let mut backend = resolvo::Solver;
        let result = backend.solve(task).unwrap();
        assert!(result
            .records
            .iter()
            .any(|r| r.package_record.name.as_normalized() == "numpy"));
    }

    #[test]
    fn preferred_record_is_favored_when_still_available() {
        let spec = matchspec("numpy");
        let old = record("numpy", "1.0.0", "https://conda.anaconda.org/conda-forge/");
        let new = record("numpy", "2.0.0", "https://conda.anaconda.org/conda-forge/");
        let available = [&old, &new];

        let task = build_solver_task(
            vec![spec],
            &available,
            std::slice::from_ref(&old),
            Vec::new(),
        );
        assert_eq!(task.locked_packages.len(), 1);
        assert_eq!(
            task.locked_packages[0].package_record.version,
            old.package_record.version
        );
    }

    #[test]
    fn a_preferred_record_no_longer_available_is_not_an_error() {
        let spec = matchspec("numpy");
        let current = record("numpy", "2.0.0", "https://conda.anaconda.org/conda-forge/");
        let available = [&current];
        let stale_preferred = record("numpy", "0.1.0", "https://conda.anaconda.org/conda-forge/");

        let task = build_solver_task(
            vec![spec],
            &available,
            std::slice::from_ref(&stale_preferred),
            Vec::new(),
        );
        assert!(task.locked_packages.is_empty());
    }
}
