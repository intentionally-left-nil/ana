//! `requirements.txt` dependency parsing for `ana`, including ana's own
//! `# ana-matchspec: <spec>` (a conda `MatchSpec`, not valid PEP 508)
//! and file-level `# ana-channels: <list>` directive comments:
//!
//! ```text
//! # ana-channels: conda-forge, bioconda
//! numpy>=1.20
//! # ana-matchspec: mkl
//! ruff
//! ```
//!
//! - [`RequirementsTxt::parse`]: source text in, a [`RequirementsTxt`]
//!   of [`RequirementEntry`]s out. Every invalid or unsupported line is
//!   collected into one [`RequirementsTxtError`] rather than stopping
//!   at the first.
//! - [`lines`]: joins backslash-continued lines, strips comments, and
//!   classifies directive lines from ordinary requirement lines.
//!
//! Recursive includes (`-r`/`-c`), editable/VCS/local-path/URL
//! requirements, and hash pins (`--hash`) are not supported -- none
//! have a conda `MatchSpec` equivalent, and includes would require
//! disk I/O this crate doesn't perform. Such a line is reported as a
//! [`LineErrorKind`] rather than ignored.
//!
//! `requirements.txt` content is untrusted, so this crate never
//! `unwrap`/`expect`s outside of tests.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod document;
mod lines;

pub use document::{
    Dependency, LineError, LineErrorKind, RequirementEntry, RequirementsTxt, RequirementsTxtError,
};
