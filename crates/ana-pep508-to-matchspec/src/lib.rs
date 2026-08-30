//! PEP 508 requirement -> conda `MatchSpec` conversion for `ana`.
//!
//! [`convert`] turns one already-parsed [`Requirement`] into
//! `Ok(Some(matchspec))`, `Ok(None)` if its marker can never hold on the
//! target machine, or `Err(ConvertError)` for an unrepresentable version,
//! name, or marker shape. [`convert_all`] runs it over a whole requirement
//! list. [`version_spec`] (see [`version`]'s module docs) handles the PEP
//! 440 -> `VersionSpec` piece on its own.
//!
//! A marker containing any `extra == "..."` clause is always rejected with
//! [`ConvertError::Marker`]: this crate has no notion of which extras are
//! "active" for the current install.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod convert;
mod version;

pub use convert::{convert, convert_all, ConvertError};
pub use version::version_spec;
