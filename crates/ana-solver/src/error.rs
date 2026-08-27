//! [`crate::RattlerSolver`]'s own error type.
//!
//! Never seen as a typed value by `ana-lockfile` itself: [`crate::RattlerSolver::solve`]
//! boxes every variant into the `Box<dyn std::error::Error + Send + Sync>`
//! `ana_lockfile::Solver::solve` returns -- see that trait's own docs on
//! why no solver crate (this one included) is a compile-time dependency of
//! `ana-lockfile`.

use rattler_conda_types::ParseChannelError;
use rattler_repodata_gateway::GatewayError;
use rattler_solve::SolveError;
use rattler_virtual_packages::DetectVirtualPackageError;

/// Everything that can go wrong building or running a [`crate::RattlerSolver`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Building the `tokio` runtime the solver drives its async gateway
    /// calls through failed -- an OS-level failure (out of threads/file
    /// descriptors), not a solve failure.
    #[error("failed to start the solver's async runtime: {0}")]
    Runtime(#[source] std::io::Error),

    /// One of [`ana_lockfile::SolveRequest::channels`]'s names didn't parse
    /// as a channel.
    #[error("invalid channel {name:?}: {source}")]
    Channel {
        name: String,
        #[source]
        source: ParseChannelError,
    },

    /// Building one of the hardcoded `defaults` meta-channel's
    /// `repo.anaconda.com/pkgs/*` URLs failed to parse -- see
    /// `crate::channels`'s own docs for why this is not expected to
    /// happen in practice (the URLs are built from this crate's own
    /// hardcoded subchannel names, never from external input), but is
    /// still a typed, propagated error rather than an `unwrap`.
    #[error("could not build the defaults channel's URL: {0}")]
    DefaultsChannelUrl(#[from] url::ParseError),

    /// Fetching channel repodata failed (network, parsing, a missing
    /// subdir, ...).
    #[error("failed to fetch repodata: {0}")]
    Gateway(#[from] GatewayError),

    /// Detecting the target platform's virtual packages failed.
    #[error("failed to detect virtual packages: {0}")]
    VirtualPackages(#[from] DetectVirtualPackageError),

    /// The solve itself failed (unsatisfiable requirements, an
    /// unrecognized solver operation, ...).
    #[error("{0}")]
    Solve(#[from] SolveError),
}
