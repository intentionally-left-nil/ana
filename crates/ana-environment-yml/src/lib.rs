//! `environment.yml` (conda) dependency declaration parsing for `ana`:
//! [`EnvironmentYml::parse`] extracts `channels` and `dependencies`
//! (including each entry's `pip:` subkey), rejecting anything outside
//! that shape as an [`EnvironmentYmlError`]. `name`, `variables`, and
//! other unrecognized top-level keys are ignored, not rejected. See
//! [`EnvironmentYml::parse`]'s docs for the full shape this crate
//! understands.
//!
//! `environment.yml` content is untrusted, so this crate never
//! `unwrap`/`expect`s outside of tests.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod document;

pub use document::{Dependency, EnvironmentYml, EnvironmentYmlError, InvalidField};
