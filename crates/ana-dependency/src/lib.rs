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

use std::str::FromStr;

use rattler_conda_types::{MatchSpec, ParseMatchSpecError, ParseMatchSpecOptions};
use uv_pep508::{Pep508Error, Requirement};

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

/// One [`Dependency`] selected for a solve, with its provenance. Borrows
/// the dependency out of whatever declaration (`RequirementSet`, a CLI
/// invocation, ...) it was selected from, rather than cloning it.
#[derive(Debug, Clone)]
pub struct SelectedRequirement<'a> {
    pub dependency: &'a Dependency,
    /// `"runtime"` or `"group:<name>"` -- recorded in the lock for
    /// readability, never compared for staleness.
    pub source: String,
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

/// Either half of [`parse_specifier`]'s dispatch failing.
#[derive(Debug, thiserror::Error)]
pub enum ParseSpecifierError {
    #[error(transparent)]
    Pep508(#[from] Pep508Error),
    #[error(transparent)]
    Matchspec(#[from] ParseMatchSpecError),
}

/// Parses one CLI-declared specifier (`ana run`'s `<primary>` under
/// `-g`, and every `-i`/`--include` value): `::` anywhere selects a
/// conda `MatchSpec` (`::node`, `conda-forge::node`); anything else is
/// parsed as a PEP 508 requirement.
pub fn parse_specifier(text: &str) -> Result<Dependency, ParseSpecifierError> {
    if text.contains("::") {
        Ok(Dependency::Matchspec(Box::new(parse_matchspec(text)?)))
    } else {
        Ok(Dependency::Pep508(Requirement::from_str(text)?))
    }
}

/// The bare package name a `Dependency` names, if it names exactly one:
/// a PEP 508 requirement's distribution name, or a `MatchSpec`'s exact
/// name matcher. `None` for a `MatchSpec` whose name matcher isn't
/// `Exact` (glob/regex) -- never produced by [`parse_matchspec`]'s own
/// options, which require an exact name, but still a real state of the
/// underlying `PackageNameMatcher` type.
pub fn bare_name(dependency: &Dependency) -> Option<String> {
    match dependency {
        Dependency::Pep508(req) => Some(req.name.as_str().to_string()),
        Dependency::Matchspec(spec) => spec
            .name
            .as_exact()
            .map(|name| name.as_normalized().to_string()),
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

    #[test]
    fn parse_specifier_dispatches_bare_name_to_pep508() {
        let dep = parse_specifier("requests").unwrap();
        assert!(matches!(dep, Dependency::Pep508(_)));
    }

    #[test]
    fn parse_specifier_dispatches_channelled_spec_to_matchspec() {
        let dep = parse_specifier("conda-forge::node").unwrap();
        assert!(matches!(dep, Dependency::Matchspec(_)));
    }

    #[test]
    fn parse_specifier_dispatches_bare_channel_marker_to_matchspec() {
        let dep = parse_specifier("::node").unwrap();
        assert!(matches!(dep, Dependency::Matchspec(_)));
    }

    #[test]
    fn parse_specifier_dispatches_versioned_pep508_to_pep508() {
        let dep = parse_specifier("requests>=2.8.1").unwrap();
        assert!(matches!(dep, Dependency::Pep508(_)));
    }

    #[test]
    fn parse_specifier_dispatches_extras_to_pep508() {
        let dep = parse_specifier("fastapi[standard]").unwrap();
        assert!(matches!(dep, Dependency::Pep508(_)));
    }

    #[test]
    fn parse_specifier_rejects_invalid_pep508() {
        assert!(parse_specifier("!!!not a requirement!!!").is_err());
    }

    #[test]
    fn bare_name_of_pep508() {
        let dep = parse_specifier("fastapi[standard]").unwrap();
        assert_eq!(bare_name(&dep), Some("fastapi".to_string()));
    }

    #[test]
    fn bare_name_of_matchspec() {
        let dep = parse_specifier("::python==3.14").unwrap();
        assert_eq!(bare_name(&dep), Some("python".to_string()));
    }

    #[test]
    fn bare_name_of_url_matchspec_is_derived_from_its_filename() {
        // Conda's own filename convention (`<name>-<version>-<build>.<ext>`)
        // gives a URL-only matchspec an exact name too -- `bare_name` is
        // `None` only for a matchspec whose name matcher isn't `Exact`
        // (a glob/regex), which `ana`'s own matchspec parsing options
        // never produce.
        let spec = parse_matchspec("https://example.com/numpy-1.0-0.conda").unwrap();
        let dep = Dependency::Matchspec(Box::new(spec));
        assert_eq!(bare_name(&dep), Some("numpy".to_string()));
    }

    #[test]
    fn bare_name_is_none_for_a_matchspec_with_a_non_exact_name_matcher() {
        // `parse_matchspec`'s fixed options (`exact_names_only: true`)
        // never produce this -- a glob/regex name is rejected as a parse
        // error before `MatchSpec` construction -- but `PackageNameMatcher`
        // itself is a real three-variant type, built here directly
        // (bypassing `parse_matchspec`) to prove `bare_name`'s `None`
        // case is a real, reachable state of the type it matches on,
        // not a theoretical one.
        let spec = MatchSpec {
            name: rattler_conda_types::PackageNameMatcher::from_str("numpy*").unwrap(),
            ..MatchSpec::default()
        };
        let dep = Dependency::Matchspec(Box::new(spec));
        assert_eq!(bare_name(&dep), None);
    }
}
