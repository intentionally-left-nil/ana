//! The solver seam.
//!
//! No solver crate (`rattler_solve` or equivalent) is in the workspace yet,
//! so the algorithm is written against this trait and tested with fakes.
//! Wiring in the real solver is a caller-side change (provide an impl),
//! not a change to this crate.
//!
//! The request shape is deliberate:
//!
//! - `specs` already carries the *entire* solve input as ordinary
//!   matchspecs -- including the `python` constraint
//!   [`crate::matchspec::convert_for_platform`] derives from
//!   `pyproject.toml`'s `requires-python`, if any. There is no separate
//!   `requires_python` field: conda has no notion of "the interpreter
//!   constraint" distinct from any other package constraint, so a
//!   [`Solver`] implementation never needs to know that one particular
//!   matchspec happened to come from `requires-python` rather than
//!   `[project.dependencies]`.
//! - `preferred` *borrows* the previous section's full
//!   [`RepoDataRecord`]s as bias hints, so a re-resolve tends to
//!   reproduce the previous answer wherever it's still legal -- the
//!   reason full records (not partial snapshots) are stored in the lock
//!   at all: installing a resolved lock needs each record's
//!   `url`/`channel`/`identifier` to actually fetch or verify it, and a
//!   wheel-origin record's `url` isn't derivable from name/version/build
//!   the way a conda-native archive's filename is. A borrow, not an owned
//!   `Vec`: the caller ([`crate::algorithm::solve_section`]) already has
//!   the previous section's `Vec<RepoDataRecord>` sitting in a local
//!   variable for the whole duration of the solve, and a full
//!   environment's package list is too expensive to clone just to
//!   satisfy a struct that only ever reads it back.
//! - `channels` is hardcoded to `["defaults"]` by the algorithm
//!   ([`DEFAULT_CHANNELS`]) -- real channel configuration is explicitly
//!   out of scope for now.

use rattler_conda_types::{MatchSpec, Platform, RepoDataRecord};

/// The only channel set the algorithm ever requests.
pub const DEFAULT_CHANNELS: &[&str] = &["defaults"];

/// Everything one platform's solve needs.
#[derive(Debug)]
pub struct SolveRequest<'a> {
    /// The platform being solved for -- not necessarily
    /// `Platform::current()` (cross-platform mode solves foreign subdirs
    /// from any host, given network access to that subdir's repodata).
    pub platform: Platform,
    /// The canonical matchspecs to solve, from
    /// [`crate::matchspec::convert_for_platform`] -- every requirement
    /// the project declares, `python` (from `requires-python`) included,
    /// as ordinary matchspecs with no distinction between them.
    pub specs: Vec<MatchSpec>,
    /// The previous lock section's packages, as solve preferences,
    /// borrowed from the caller's own copy. Empty for a first solve.
    pub preferred: &'a [RepoDataRecord],
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
        request: SolveRequest<'_>,
    ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>>;
}
