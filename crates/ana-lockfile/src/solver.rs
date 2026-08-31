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

use std::collections::HashMap;

use ana_channels::ChannelId;
use rattler_conda_types::{MatchSpec, PackageName, Platform, RepoDataRecord};

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
    /// The already-authorized channels to solve against (see
    /// `ana_channels::resolve_channels`) -- every entry here has already
    /// passed policy, so the solver itself never needs to know what
    /// `"defaults"` means.
    pub channels: Vec<ChannelId>,
    /// A per-package channel restriction: a package named here may only
    /// be satisfied by a candidate whose `url` falls under its mapped
    /// [`ChannelId`], even though every channel in `channels` is fetched
    /// and searched. One channel per package (a matchspec qualifier
    /// names exactly one channel); never applies to a restricted
    /// package's own transitive dependencies.
    pub channel_restrictions: HashMap<PackageName, ChannelId>,
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
