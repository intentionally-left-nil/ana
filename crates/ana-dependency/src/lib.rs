//! The dependency shape shared by `ana`'s two source-file front ends,
//! `ana-pyproject` (`pyproject.toml`) and `ana-requirements-txt`
//! (`requirements.txt`): a declared dependency is either a PEP 508
//! requirement or a conda `MatchSpec` (via each format's own
//! `ana-matchspec` extension syntax, since `MatchSpec` has no PEP 508
//! spelling). This crate owns the [`Dependency`] type and the
//! [`parse_matchspec`] rule both front ends reuse, so a dependency is
//! represented and validated identically regardless of which file
//! declared it.
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

/// Everything that can go wrong parsing a non-empty `ana-matchspec`
/// string.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MatchspecError {
    /// The spec is syntactically valid but sets an explicit channel or
    /// url, which is not allowed for a dependency declaration.
    #[error("matchspec entries may not set an explicit channel or url")]
    ExplicitChannelOrUrl,
    /// The spec is not syntactically valid.
    #[error("{0}")]
    Invalid(#[source] ParseMatchSpecError),
}

/// Parses and validates one `ana-matchspec` string, rejecting an
/// otherwise-valid `MatchSpec` that sets an explicit channel or url
/// (see [`MatchspecError::ExplicitChannelOrUrl`]).
pub fn parse_matchspec(text: &str) -> Result<MatchSpec, MatchspecError> {
    match MatchSpec::from_str(text, matchspec_parse_options()) {
        Ok(spec) if spec.channel.is_some() || spec.url.is_some() => {
            Err(MatchspecError::ExplicitChannelOrUrl)
        }
        Ok(spec) => Ok(spec),
        Err(err) => Err(MatchspecError::Invalid(err)),
    }
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
    fn rejects_an_explicit_channel() {
        assert_eq!(
            parse_matchspec("conda-forge::numpy"),
            Err(MatchspecError::ExplicitChannelOrUrl)
        );
    }

    #[test]
    fn rejects_an_explicit_url() {
        assert!(matches!(
            parse_matchspec("https://example.com/numpy-1.0-0.conda"),
            Err(MatchspecError::ExplicitChannelOrUrl) | Err(MatchspecError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_invalid_syntax() {
        assert!(matches!(
            parse_matchspec("!!!not a matchspec!!!"),
            Err(MatchspecError::Invalid(_))
        ));
    }
}
