//! PEP 621 / PEP 735 `pyproject.toml` dependency resolution for `ana`.
//!
//! Today this crate contains exactly one thing: [`resolution`], the
//! `include-group`/self-referential-extra/cycle-detection algorithm for
//! `[project.optional-dependencies]` and `[dependency-groups]`, adapted
//! from the `pyproject-toml` crate -- see that module's docs and this
//! crate's `README.md` for provenance and what changed.
//!
//! The `toml_edit`-based structural parsing that walks a real
//! `pyproject.toml` file and produces this module's inputs
//! (`IndexMap<ExtraName, Vec<Requirement>>` and
//! `IndexMap<GroupName, Vec<DependencyGroupSpecifier>>`) is not implemented
//! yet -- see `investigations/pep508_to_matchspec_api.md`.

pub mod resolution;

pub use resolution::{resolve, DependencyGroupSpecifier, Item, ResolveError, ResolvedDependencies};
