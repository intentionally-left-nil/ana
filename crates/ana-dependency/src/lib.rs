//! The dependency shape shared by `ana`'s two source-file front ends,
//! `ana-pyproject` (`pyproject.toml`) and `ana-requirements-txt`
//! (`requirements.txt`): a declared dependency is either a PEP 508
//! requirement or a conda `MatchSpec` (via each format's own
//! `ana-matchspec` extension syntax, since `MatchSpec` has no PEP 508
//! spelling). This crate owns the [`Dependency`] type and the
//! [`parse_matchspec`] rule both front ends reuse. A `MatchSpec` may set
//! an explicit channel or url; whether that override is actually
//! permitted is a solve-time policy question owned by `ana-lockfile`, not
//! something this crate checks.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use rattler_conda_types::{MatchSpec, ParseMatchSpecError, ParseMatchSpecOptions};
use uv_pep508::Requirement;

/// A dependency declared in `pyproject.toml` or `requirements.txt`: a
/// PEP 508 requirement, or a conda `MatchSpec` declared via `ana`'s own
/// extension syntax.
///
/// `ana_pyproject::Dependency` and `ana_requirements_txt::Dependency`
/// are re-exports of this exact type, so values from either front end
/// are interchangeable downstream with no per-format conversion.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Dependency {
    /// A PEP 508 requirement.
    Pep508(Requirement),
    /// A conda `MatchSpec`, boxed since it is considerably larger than
    /// a `Requirement`.
    Matchspec(Box<MatchSpec>),
}

/// The [`ParseMatchSpecOptions`] every `ana-matchspec` string is parsed
/// with: lenient strictness with bracket `extras=[...]` syntax allowed.
pub fn matchspec_parse_options() -> ParseMatchSpecOptions {
    ParseMatchSpecOptions::lenient().with_extras(true)
}

/// Parses one `ana-matchspec` string, a pure syntax check delegating
/// entirely to [`MatchSpec::from_str`]. An explicit channel or url on
/// the resulting spec is left untouched for the caller.
pub fn parse_matchspec(text: &str) -> Result<MatchSpec, ParseMatchSpecError> {
    MatchSpec::from_str(text, matchspec_parse_options())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn accepts_a_plain_matchspec() {
        let spec = parse_matchspec("numpy >=1.20").unwrap();
        assert_eq!(spec.to_string(), "numpy >=1.20");
    }

    #[test]
    fn accepts_an_explicit_channel() {
        let spec = parse_matchspec("conda-forge::numpy").unwrap();
        assert!(spec.channel.is_some());
    }

    #[test]
    fn accepts_an_explicit_url() {
        let spec = parse_matchspec("https://example.com/numpy-1.0-0.conda").unwrap();
        assert!(spec.url.is_some());
    }

    #[test]
    fn rejects_invalid_syntax() {
        assert!(parse_matchspec("!!!not a matchspec!!!").is_err());
    }
}
