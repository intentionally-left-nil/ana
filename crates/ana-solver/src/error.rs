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
}

impl Error {
    /// Whether the request was unsatisfiable at all -- e.g. the package
    /// isn't published for the target platform.
    pub fn is_unsolvable(&self) -> bool {
        matches!(self, Error::Solve(SolveError::Unsolvable(_)))
    }
}
