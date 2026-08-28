//! `requirements.txt` dependency parsing for `ana`, plus ana's own
//! `# ana-matchspec: <spec>` directive comment for declaring a conda
//! `MatchSpec` dependency (conda `MatchSpec` syntax isn't valid PEP 508,
//! so it can't appear as an ordinary requirement line):
//!
//! ```text
//! numpy>=1.20
//! # ana-matchspec: mkl
//! ruff
//! ```
//!
//! - [`RequirementsTxt::parse`] is the front end: source text in, a
//!   [`RequirementsTxt`] of [`RequirementEntry`]s out. Every invalid or
//!   unsupported line is collected into one [`RequirementsTxtError`]
//!   rather than stopping at the first.
//! - [`lines`] joins backslash-continued physical lines, strips
//!   comments, and classifies `# ana-matchspec:` lines separately from
//!   ordinary requirement lines.
//!
//! This crate does not support recursive includes (`-r`/`-c`), editable/
//! VCS/local-path/URL requirements, or hash pins (`--hash`): none of
//! these have a conda `MatchSpec` equivalent, and includes would also
//! require disk I/O this crate deliberately does not perform. A line
//! using one of them is reported as a [`LineErrorKind`] rather than
//! silently ignored.
//!
//! `requirements.txt` content is untrusted input, so this crate never
//! `unwrap`/`expect`s its way past a failure outside of tests.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod document;
mod lines;

pub use document::{
    Dependency, LineError, LineErrorKind, RequirementEntry, RequirementsTxt, RequirementsTxtError,
};
