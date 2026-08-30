//! PEP 621 / PEP 735 `pyproject.toml` dependency resolution for `ana`,
//! plus ana's own `[tool.ana]` matchspec-dependency extension.
//!
//! - [`Pyproject::parse`] is the front end: `pyproject.toml` source text
//!   in, typed [`Pyproject`] out. See [`PyprojectError`] for the error
//!   contract.
//! - [`resolution`] resolves `[project.optional-dependencies]` and
//!   `[dependency-groups]` (merged with
//!   `[tool.ana.matchspec-dependency-groups]`, see [`resolution::Dependency`])
//!   into flat lists, expanding `include-group`/self-referential-extra
//!   references.
//!
//! `pyproject.toml` content is untrusted input, so this crate never
//! `unwrap`/`expect`s outside of tests -- enforced by the lints below.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod project;
pub mod resolution;

pub use project::{InvalidField, ProjectRequirements, Pyproject, PyprojectError};
pub use resolution::{
    resolve, Dependency, DependencyGroupSpecifier, Item, ResolveError, ResolvedDependencies,
    Section,
};
