//! Resolves an `ana` invocation to an [`Environment`]: which requirement
//! declaration it targets (a project directory's `pyproject.toml`/
//! `requirements.txt`/`environment.yml`, or CLI-declared specifiers),
//! its group selection, and the filesystem paths that selection maps to.
//!
//! [`resolve`] is the whole crate's job in one call: parse/build the
//! declaration for the [`RequirementInput`], validate `--group`s against
//! it, derive its [`ana_paths::EnvironmentKey`], and discover the paths
//! that key maps to (see [`ana_paths::discover`]). The result -- an
//! [`Environment`] -- is the one value every downstream crate
//! (`ana-lockfile`, `ana-installer`) needs: its `(declaration, paths)`
//! pair can never disagree, because both were derived from the same
//! resolved groups in the same call.
//!
//! [`RequirementOrigin`] is diagnostic and policy metadata (error
//! messages, which origins have a group concept at all) -- not a
//! dispatch mechanism.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod environment;
mod error;
mod origin;
mod project_file;

pub use environment::{resolve, Environment, EnvironmentRequest, RequirementInput};
pub use error::Error;
pub use origin::RequirementOrigin;
pub use project_file::{project_file_exists, MAX_PROJECT_FILE_SIZE};
