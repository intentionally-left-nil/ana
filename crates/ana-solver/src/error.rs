//! [`crate::RattlerSolver`]'s own error type.
//!
//! Never seen as a typed value by `ana-lockfile` itself:
//! [`crate::RattlerSolver::solve`] boxes every variant into the
//! `Box<dyn std::error::Error + Send + Sync>` `ana_lockfile::Solver::solve`
//! returns.

use rattler_repodata_gateway::GatewayError;
use rattler_solve::SolveError;
use rattler_virtual_packages::DetectVirtualPackageError;

/// Everything that can go wrong building or running a [`crate::RattlerSolver`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
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

    /// The solve was unsatisfiable *and* at least one direct spec's
    /// package name had no candidates in any searched channel's repodata
    /// (already fetched for the solve itself, so classifying costs no
    /// extra I/O). `missing` carries those specs, `channels` the base
    /// URLs searched, so a caller can say what was looked for and where.
    /// An unsatisfiable solve where every direct spec's name *has*
    /// candidates (a real version conflict) stays [`Error::Solve`].
    #[error(
        "no candidates were found for {} on any searched channel ({})",
        .missing.iter().map(|m| m.spec.as_str()).collect::<Vec<_>>().join(", "),
        .channels.join(", ")
    )]
    Unsolvable {
        missing: Vec<MissingSpec>,
        channels: Vec<String>,
        source: SolveError,
    },
}

/// One direct requirement the solve found no candidates for at all: the
/// exact (normalized) package name, and the canonical matchspec string
/// for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSpec {
    pub name: String,
    pub spec: String,
}

impl Error {
    /// Whether the request was unsatisfiable at all -- e.g. the package
    /// isn't published for the target platform.
    pub fn is_unsolvable(&self) -> bool {
        matches!(
            self,
            Error::Solve(SolveError::Unsolvable(_)) | Error::Unsolvable { .. }
        )
    }
}
