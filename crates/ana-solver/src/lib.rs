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
//! 1. Build a real [`rattler_conda_types::Channel`] from each of
//!    `request.channels`' already-authorized [`ana_channels::ChannelId`]s
//!    -- this crate never resolves a bare name or `"defaults"` itself;
//!    that policy question is `ana_channels::resolve_channels`'s, answered
//!    before a [`ana_lockfile::SolveRequest`] is even built.
//! 2. Fetch repodata for `request.platform` *and* `noarch`, recursively --
//!    the whole dependency closure of `request.specs`, not just their own
//!    records.
//! 3. Detect `request.platform`'s virtual packages
//!    (`rattler_virtual_packages::VirtualPackages::detect_for_platform`).
//! 4. Solve, biasing towards `request.preferred` (matched back against the
//!    records just fetched -- see [`Solver::solve`]'s docs for why a
//!    stored [`rattler_conda_types::RepoDataRecord`] is re-matched by
//!    identity rather than trusted as-is), and excluding any candidate a
//!    `request.channel_restrictions` entry rules out for its package (see
//!    [`build_excluded_candidates`]).
//! 5. Return each winning `RepoDataRecord` directly -- the shape
//!    `ana_lockfile::PlatformSection` stores end to end (a bare
//!    `PackageRecord` alone carries no `url` to install from).
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod error;
mod progress;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ana_channels::ChannelId;
use ana_lockfile::{SolveRequest, Solver};
use rattler_conda_types::{Channel, PackageName, PackageRecord, Platform, RepoDataRecord};
use rattler_networking::LazyClient;
use rattler_repodata_gateway::{Gateway, RepoData};
use rattler_solve::{
    resolvo, ChannelPriority, RepoDataIter, SolveStrategy, SolverImpl, SolverTask,
};
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};
use url::Url;

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
    let channels: Vec<Channel> = request
        .channels
        .iter()
        .map(|id| Channel::from_url(id.as_url().clone()))
        .collect();

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

    let excluded_candidates = build_excluded_candidates(&available, &request.channel_restrictions);

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
        // Restriction is already enforced per-package via
        // `excluded_candidates`, so `Strict` (the upstream default,
        // which confines every package to its first-seen channel) would
        // be unnecessarily aggressive for unrestricted packages.
        channel_priority: ChannelPriority::Flexible,
        exclude_newer: None,
        strategy: SolveStrategy::default(),
        dependency_overrides: Vec::new(),
        excluded_candidates,
        cancellation_token: None,
    };

    let mut backend = resolvo::Solver;
    let solving_line = ana_progress::StatusLine::new();
    let result = progress::solve_label(&solving_line, move || backend.solve(task))?;

    // The full `RepoDataRecord`s, unmodified -- `ana_lockfile::PlatformSection`
    // stores exactly this shape, `url` included, not a bare `PackageRecord`.
    Ok(result.records)
}

/// Builds `SolverTask::excluded_candidates` from `available`: for each
/// package named in `restrictions`, every fetched record of that name
/// whose `url` does not fall under its required [`ChannelId`] is ruled
/// out, with one shared reason string per restriction (not per record) --
/// `restrictions` never applies to a restricted package's own transitive
/// dependencies, since it is only ever consulted for records whose own
/// name is a key of the map.
fn build_excluded_candidates(
    available: &[&RepoDataRecord],
    restrictions: &HashMap<PackageName, ChannelId>,
) -> HashMap<Url, Arc<str>> {
    let mut excluded = HashMap::new();
    for (name, required_channel) in restrictions {
        let reason: Arc<str> = format!(
            "{} is restricted to {} ({})",
            name.as_normalized(),
            ana_channels::display(required_channel),
            required_channel.as_url()
        )
        .into();
        for record in available
            .iter()
            .filter(|record| &record.package_record.name == name)
        {
            if !required_channel.contains_url(&record.url) {
                excluded.insert(record.url.clone(), Arc::clone(&reason));
            }
        }
    }
    excluded
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use rattler_conda_types::{PackageRecord, Version};

    use super::*;

    fn record(name: &str, url: &str) -> RepoDataRecord {
        let package_record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str("1.0.0").unwrap(),
            "0".to_string(),
        );
        let identifier = rattler_conda_types::package::DistArchiveIdentifier::try_from_filename(
            &format!("{name}-1.0.0-0.conda"),
        )
        .unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url: Url::parse(url).unwrap(),
            channel: None,
        }
    }

    fn channel(url: &str) -> ChannelId {
        ana_channels::resolve_qualifier(url).unwrap()
    }

    #[test]
    fn a_record_outside_the_required_channel_is_excluded_one_inside_is_not() {
        let main = record(
            "conda",
            "https://repo.anaconda.com/pkgs/main/linux-64/conda-1.0.0-0.conda",
        );
        let forge = record(
            "conda",
            "https://conda.anaconda.org/conda-forge/linux-64/conda-1.0.0-0.conda",
        );
        let available = vec![&main, &forge];
        let restrictions = HashMap::from([(
            PackageName::new_unchecked("conda"),
            channel("https://repo.anaconda.com/pkgs/main"),
        )]);

        let excluded = build_excluded_candidates(&available, &restrictions);

        assert!(!excluded.contains_key(&main.url));
        assert!(excluded.contains_key(&forge.url));
    }

    #[test]
    fn the_exclusion_reason_is_shared_across_every_record_of_one_restriction() {
        let forge_linux = record(
            "conda",
            "https://conda.anaconda.org/conda-forge/linux-64/conda-1.0.0-0.conda",
        );
        let forge_noarch = record(
            "conda",
            "https://conda.anaconda.org/conda-forge/noarch/conda-1.0.0-0.conda",
        );
        let available = vec![&forge_linux, &forge_noarch];
        let restrictions = HashMap::from([(
            PackageName::new_unchecked("conda"),
            channel("https://repo.anaconda.com/pkgs/main"),
        )]);

        let excluded = build_excluded_candidates(&available, &restrictions);

        assert_eq!(excluded.len(), 2);
        assert!(Arc::ptr_eq(
            &excluded[&forge_linux.url],
            &excluded[&forge_noarch.url]
        ));
    }

    #[test]
    fn a_restriction_never_applies_to_a_differently_named_package() {
        let unrelated = record(
            "numpy",
            "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0.0-0.conda",
        );
        let available = vec![&unrelated];
        let restrictions = HashMap::from([(
            PackageName::new_unchecked("conda"),
            channel("https://repo.anaconda.com/pkgs/main"),
        )]);

        let excluded = build_excluded_candidates(&available, &restrictions);

        assert!(excluded.is_empty());
    }

    #[test]
    fn a_tokened_channel_restriction_matches_by_url_regardless_of_record_channel() {
        let right_token = record(
            "conda",
            "https://conda.example/t/secret/main/linux-64/conda-1.0.0-0.conda",
        );
        let wrong_token = record(
            "conda",
            "https://conda.example/t/other/main/linux-64/conda-1.0.0-0.conda",
        );
        let available = vec![&right_token, &wrong_token];
        let restrictions = HashMap::from([(
            PackageName::new_unchecked("conda"),
            channel("https://conda.example/t/secret/main"),
        )]);

        let excluded = build_excluded_candidates(&available, &restrictions);

        assert!(!excluded.contains_key(&right_token.url));
        assert!(excluded.contains_key(&wrong_token.url));
    }

    #[test]
    fn no_restrictions_excludes_nothing() {
        let any = record(
            "conda",
            "https://conda.anaconda.org/conda-forge/linux-64/conda-1.0.0-0.conda",
        );
        let excluded = build_excluded_candidates(&[&any], &HashMap::new());
        assert!(excluded.is_empty());
    }
}
