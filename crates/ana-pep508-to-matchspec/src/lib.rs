//! PEP 508 requirement -> conda `MatchSpec` conversion for `ana`, restricted
//! to markerless requirements: the fast path per
//! `investigations/pep508_to_matchspec_api.md`, essentially a Rust port of
//! reroll's `pep508_to_matchspec()` with marker handling (that doc's
//! two-pronged marker conversion, `ana-marker-matchspec`) deliberately out
//! of scope for this pass -- performance and accuracy on the markerless
//! case first, since per that investigation the vast majority of
//! real-world requirements have no marker at all. A requirement whose
//! marker isn't `MarkerTree::TRUE` -- anything beyond a bare requirement
//! string with no trailing `; ...` clause -- is rejected with
//! [`ConvertError::Marker`] rather than partially converted; see
//! [`convert`]'s docs.
//!
//! No string is ever formatted and reparsed to build a `MatchSpec`: `name`,
//! `version`, and `extras` are each constructed as a typed value directly,
//! per that investigation's headline finding. The one exception is an
//! individual version literal (`Version::from_str`), which has no general
//! typed constructor in `rattler_conda_types` -- see [`version`]'s module
//! docs.
//!
//! Name mapping is also out of scope: the conda `PackageName` in every
//! produced `MatchSpec` is `requirement.name` unchanged (already PEP
//! 503-normalized), the identity mapping the investigation deliberately
//! builds against first. See [`convert`]'s module (`convert.rs`) for where
//! a real `ana-pypi-conda-map` lookup slots in later.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod convert;
mod version;

pub use convert::{convert, convert_all, ConvertError};
