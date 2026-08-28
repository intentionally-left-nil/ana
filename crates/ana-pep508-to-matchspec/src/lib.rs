//! PEP 508 requirement -> conda `MatchSpec` conversion for `ana`.
//!
//! Per-[`Requirement`] orchestration: name + version + extras + marker ->
//! `MatchSpec`, a Rust port of reroll's `pep508_to_matchspec()`. Marker
//! handling delegates to `ana-marker-matchspec` for every marker shape
//! except one that's this crate's own concern: a marker containing any
//! `extra == "..."` clause is rejected outright with
//! [`ConvertError::Marker`], since this crate has no notion of which
//! extras are "active" for the current install (a future, separate pass).
//! `ana-marker-matchspec` doesn't handle it either -- an `extra` clause
//! reaching its conversion is
//! [`ana_marker_matchspec::Unconvertible::ExtraMarker`], a marker-shape
//! error, not an active-extras evaluation.
//!
//! [`convert`] takes an `assumption` (see
//! [`ana_marker_matchspec::known_values_assumption`]) built once by the
//! caller from the machine's subdir and reused across every call, so it
//! never needs to know `rattler_conda_types::Platform::current()` itself.
//!
//! Three outcomes per requirement: `Ok(Some(matchspec))` (the marker
//! holds, unconditionally or via a `when=`-equivalent `condition`),
//! `Ok(None)` (the marker can never hold on this machine -- e.g.
//! `sys_platform == "win32"` while installing on Linux -- so the caller
//! should drop the dependency, not treat it as an error), or
//! `Err(ConvertError)` (an unrepresentable version, name, or marker
//! shape).
//!
//! No string is ever formatted and reparsed to build a `MatchSpec`:
//! `name`, `version`, `extras`, and `condition` are each constructed as a
//! typed value directly. The one exception is an individual version
//! literal (`Version::from_str`), which has no general typed constructor
//! in `rattler_conda_types` -- see [`version`]'s module docs.
//!
//! Name mapping is out of scope: the conda `PackageName` in every
//! produced `MatchSpec` is `requirement.name` unchanged (already PEP
//! 503-normalized). See [`convert`]'s module (`convert.rs`) for where a
//! real `ana-pypi-conda-map` lookup slots in later.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod convert;
mod version;

pub use convert::{convert, convert_all, ConvertError};
pub use version::version_spec;
