//! PEP 621 / PEP 735 `pyproject.toml` dependency resolution for `ana`.
//!
//! - [`Pyproject::parse`] is the front end: `pyproject.toml` source text
//!   in, typed [`Pyproject`] out (project name + requirements). Returns
//!   the first structural problem found, or every invalid PEP 508
//!   requirement string once the document's shape checks out -- see
//!   [`PyprojectError`] and `src/project.rs`'s test module for the
//!   contract.
//! - [`resolution`] is the `include-group`/self-referential-extra/cycle-
//!   detection algorithm for `[project.optional-dependencies]` and
//!   `[dependency-groups]`, adapted from the `pyproject-toml` crate -- see
//!   that module's docs and this crate's `README.md` for provenance and
//!   what changed.
//!
//! `pyproject.toml` content is untrusted input, so this crate never
//! `unwrap`/`expect`s its way past a failure outside of tests (where the
//! input is controlled and a panic-on-failure is exactly what's wanted).
//! Enforced by the compiler, not just convention: both lints below are
//! `clippy::restriction` lints (allow-by-default upstream), promoted to
//! `deny` here so a stray `.unwrap()`/`.expect()` added to production code
//! fails `cargo clippy` instead of shipping as a latent panic. Test modules
//! opt back in locally with `#[allow(...)]`.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod project;
pub mod resolution;

pub use project::{InvalidField, ProjectRequirements, Pyproject, PyprojectError};
pub use resolution::{
    resolve, DependencyGroupSpecifier, Item, ResolveError, ResolvedDependencies, Section,
};
