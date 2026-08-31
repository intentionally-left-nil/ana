//! The solver seam.
//!
//! No solver crate (`rattler_solve` or equivalent) is in the workspace yet,
//! so the algorithm is written against this trait and tested with fakes.
//!
//! `specs` carries the entire solve input as ordinary matchspecs, including
//! the `python` constraint derived from `requires-python`: conda has no
//! separate notion of "the interpreter constraint", so a [`Solver`] never
//! needs to distinguish it from any other matchspec.
//!
//! `preferred` borrows the previous section's [`RepoDataRecord`]s rather
//! than owning them, since the caller already holds them for the solve's
//! duration and a full package list is too expensive to clone.

use rattler_conda_types::{Channel, MatchSpec, Platform, RepoDataRecord};

/// Everything one platform's solve needs.
#[derive(Debug)]
pub struct SolveRequest<'a> {
    /// The platform being solved for. Not necessarily `Platform::current()`
    /// -- cross-platform mode solves foreign subdirs from any host.
    pub platform: Platform,
    /// The canonical matchspecs to solve, including `python` derived from
    /// `requires-python` if any.
    pub specs: Vec<MatchSpec>,
    /// The previous lock section's packages, as solve preferences. Empty
    /// for a first solve.
    pub preferred: &'a [RepoDataRecord],
    /// The channels to solve against, already resolved to real channel
    /// URLs by `ana_channels::ChannelPolicy` -- never bare names, so a
    /// [`Solver`] impl never has any channel-alias resolution of its own
    /// to do.
    pub channels: Vec<Channel>,
}

/// A conda solver. Implementations do the network-bound work; everything
/// before (conversion) and after (splicing the result into `ana.lock`) is
/// this crate's.
pub trait Solver {
    /// Solve `request` into a full set of package records for
    /// `request.platform`.
    fn solve(
        &self,
        request: SolveRequest<'_>,
    ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>>;
}
