//! PEP 508 environment marker -> conda `MatchSpec` condition conversion,
//! single-target: `ana` installs a dependency onto the machine it's
//! currently running on, not a portable environment that has to remain
//! valid on every subdir `ana` supports. See
//! `investigations/pep508_to_matchspec_api.md`'s "Slow path, take 2" for
//! the full design this crate implements.
//!
//! Every non-python-version marker key this crate needs is *known* for
//! the lifetime of the process -- either a fixed CPython policy constant
//! or a pure function of the subdir being installed onto (see
//! [`known_values_assumption`]) -- except `python_version`/
//! `python_full_version`/`implementation_version`, which stay free: that's
//! the conda solver's job, not this crate's. `platform_release`/
//! `platform_version` are deliberately excluded from what's known (see
//! [`known_values_assumption`]'s docs); a marker referencing either
//! becomes [`Unconvertible`].
//!
//! [`to_matchspec_condition`] is the single entry point: it calls
//! `uv_pep508::MarkerTree::restrict` with [`known_values_assumption`]'s
//! output, then converts whatever's left (only ever the free
//! `python_version` family, or a deliberately-excluded key) via a small
//! fast-path leaf table ported from reroll's `marker_conversion.py`. No
//! `Environment`/partial-solve machinery is hand-rolled here the way
//! `markerpry` builds one from scratch -- `uv_pep508`'s own `restrict()`
//! *is* that machinery, already canonical and polynomial-time; see
//! [`assumption`]'s and [`condition`]'s module docs for the two halves.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod assumption;
mod condition;

pub use assumption::{known_values_assumption, UnsupportedPlatform};
pub use condition::{format_version, to_matchspec_condition, Applicability, Unconvertible};
