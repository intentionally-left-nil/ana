//! The solver seam.
//!
//! No solver crate (`rattler_solve` or equivalent) is in the workspace yet
//! -- that's one of `investigations/lock_generation_algorithm.md`'s open
//! TODOs -- so the algorithm is written against this trait and tested with
//! fakes. Wiring in the real solver is a caller-side change (provide an
//! impl), not a change to this crate.
//!
//! The request shape is deliberate:
//!
//! - `preferred` carries the previous section's full [`PackageRecord`]s
//!   into the solve as bias hints, so a re-resolve tends to reproduce the
//!   previous answer wherever it's still legal -- `lock_file.md`'s
//!   Property 2, the reason full records (not partial snapshots) are
//!   stored in the lock at all.
//! - `channels` is hardcoded to `["defaults"]` by the algorithm
//!   ([`DEFAULT_CHANNELS`]) -- real channel configuration is explicitly
//!   out of scope for now.

use rattler_conda_types::{MatchSpec, PackageRecord, Platform};
use uv_pep440::VersionSpecifiers;

/// The only channel set the algorithm ever requests, per the
/// investigation's "No real channel configuration" decision.
pub const DEFAULT_CHANNELS: &[&str] = &["defaults"];

/// Everything one platform's solve needs.
#[derive(Debug)]
pub struct SolveRequest {
    /// The platform being solved for -- not necessarily
    /// `Platform::current()` (cross-platform mode solves foreign subdirs
    /// from any host, given network access to that subdir's repodata).
    pub platform: Platform,
    /// The canonical matchspecs to solve, from
    /// [`crate::matchspec::convert_for_platform`].
    pub specs: Vec<MatchSpec>,
    /// The project's `requires-python`, if declared. The solver is
    /// expected to turn this into a constraint on the `python` package.
    pub requires_python: Option<VersionSpecifiers>,
    /// The previous lock section's packages, as solve preferences. Empty
    /// for a first solve.
    pub preferred: Vec<PackageRecord>,
    /// Always [`DEFAULT_CHANNELS`] today.
    pub channels: Vec<String>,
}

/// A conda solver. Implementations do the network-bound work; everything
/// before (conversion) and after (splicing the result into `ana.lock`) is
/// this crate's.
pub trait Solver {
    /// Solve `request` into a full set of package records for
    /// `request.platform`.
    fn solve(
        &self,
        request: SolveRequest,
    ) -> Result<Vec<PackageRecord>, Box<dyn std::error::Error + Send + Sync>>;
}
