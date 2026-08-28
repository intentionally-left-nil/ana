//! PEP 621 / PEP 735 `pyproject.toml` dependency resolution for `ana`,
//! plus ana's own `[tool.ana]` matchspec-dependency extension.
//!
//! - [`Pyproject::parse`] is the front end: `pyproject.toml` source text
//!   in, typed [`Pyproject`] out (project name + requirements). Returns
//!   the first structural problem found, or every invalid PEP 508
//!   requirement string or conda `MatchSpec` string once the document's
//!   shape checks out -- see [`PyprojectError`] and `src/project.rs`'s
//!   test module for the contract.
//! - [`resolution`] is the `include-group`/self-referential-extra/cycle-
//!   detection algorithm for `[project.optional-dependencies]` and
//!   `[dependency-groups]` (merged with
//!   `[tool.ana.matchspec-dependency-groups]`, see [`resolution::Dependency`]),
//!   adapted from the `pyproject-toml` crate -- see that module's docs and
//!   this crate's `README.md` for provenance.
//!
//! `pyproject.toml` content is untrusted input, so this crate never
//! `unwrap`/`expect`s its way past a failure outside of tests. This is
//! enforced by the compiler: both lints below are `clippy::restriction`
//! lints (allow-by-default upstream), promoted to `deny` here so a stray
//! `.unwrap()`/`.expect()` in production code fails `cargo clippy` instead
//! of shipping as a latent panic. Test modules opt back in with
//! `#[allow(...)]`.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod project;
pub mod resolution;

pub use project::{InvalidField, ProjectRequirements, Pyproject, PyprojectError};
pub use resolution::{
    resolve, Dependency, DependencyGroupSpecifier, Item, ResolveError, ResolvedDependencies,
    Section,
};
