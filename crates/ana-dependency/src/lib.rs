//! The dependency shape shared by `ana`'s two source-file front ends,
//! `ana-pyproject` (`pyproject.toml`) and `ana-requirements-txt`
//! (`requirements.txt`): a declared dependency is either a PEP 508
//! requirement or a conda `MatchSpec` (via each format's own
//! `ana-matchspec` extension syntax, since `MatchSpec` has no PEP 508
//! spelling). This crate owns the [`Dependency`] type and the
//! [`parse_matchspec`] rule both front ends reuse.
//!
//! A `MatchSpec`'s `channel::`/`channel=` qualifier is lifted off the
//! spec at parse time into [`MatchspecDependency::qualifier`]: whether
//! that qualifier (or a `url=` override, which stays on `spec.url`) is
//! actually permitted is a solve-time policy question owned by
//! `ana-channels`/`ana-lockfile`, not something this crate checks --
//! but the *type* seen downstream never carries a channel a policy check
//! could forget to consult, since `spec.channel` is always `None`.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;

use rattler_conda_types::{
    Channel, ChannelConfig, MatchSpec, ParseMatchSpecError, ParseMatchSpecOptions,
};
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
    /// A conda `MatchSpec` plus its lifted-off channel qualifier, boxed
    /// since the pair is considerably larger than a `Requirement`.
    Matchspec(Box<MatchspecDependency>),
}

/// A parsed `ana-matchspec` string: the `MatchSpec` itself (`channel`
/// always `None` -- see the module docs) and the qualifier text that was
/// stripped off it, if any.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MatchspecDependency {
    pub spec: MatchSpec,
    /// The channel qualifier as the user wrote it: a bare alias
    /// (`"main"`) if `spec.channel`'s `name` round-tripped back to the
    /// same `base_url` through `Channel::from_str`, otherwise the
    /// qualifier's full URL. `None` for a matchspec with no `channel::`/
    /// `channel=` qualifier at all -- distinct from `Some("defaults")`,
    /// which is a real (if likely invalid) qualifier a caller must still
    /// resolve to find out it's illegal.
    pub qualifier: Option<String>,
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

/// Parses one `ana-matchspec` string via [`MatchSpec::from_str`], then
/// lifts its `channel::`/`channel=` qualifier (if any) off `spec.channel`
/// into [`MatchspecDependency::qualifier`] -- see the module docs for
/// why. `spec.url` (a `url=` override) and `spec.subdir` are left
/// untouched: `subdir` is orthogonal to channel authorization, and a
/// `url=` override is checked against the allow-set by prefix, not by
/// qualifier text.
pub fn parse_matchspec(text: &str) -> Result<MatchspecDependency, ParseMatchSpecError> {
    let mut spec = MatchSpec::from_str(text, matchspec_parse_options())?;
    let qualifier = spec
        .channel
        .take()
        .map(|channel| recover_qualifier(&channel));
    Ok(MatchspecDependency { spec, qualifier })
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
        Dependency::Matchspec(dep) => dep
            .spec
            .name
            .as_exact()
            .map(|name| name.as_normalized().to_string()),
    }
}

/// The already-parsed `channel::`/`channel=` qualifier's text, as the
/// user wrote it: `channel.name` (the alias `MatchSpec::from_str`
/// resolved it from) if resolving that same name again -- against the
/// same generic `ChannelConfig` `MatchSpec::from_str` itself uses,
/// per rattler's own hardcoded-`ChannelConfig` parsing (see this
/// crate's module docs on why the text is recovered rather than
/// re-derived from the input string) -- lands back on the identical
/// `base_url`; otherwise the qualifier was URL-shaped, so its resolved
/// `base_url` is the text. Never re-splits the original input on `::`:
/// that would have to special-case the bracket form
/// (`conda[channel=main]`) and risk misreading a `::` that appears
/// inside brackets or a `when` condition.
fn recover_qualifier(channel: &Channel) -> String {
    if let Some(name) = channel.name.as_deref() {
        let config = ChannelConfig::default_with_root_dir(std::path::PathBuf::new());
        if let Ok(resolved) = Channel::from_str(name, &config) {
            if resolved.base_url == channel.base_url {
                return name.to_string();
            }
        }
    }
    channel.base_url.as_str().to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn accepts_a_plain_matchspec() {
        let dep = parse_matchspec("numpy >=1.20").unwrap();
        assert_eq!(dep.spec.to_string(), "numpy >=1.20");
        assert_eq!(dep.qualifier, None);
    }

    #[test]
    fn accepts_an_explicit_channel() {
        let dep = parse_matchspec("conda-forge::numpy").unwrap();
        assert!(
            dep.spec.channel.is_none(),
            "the channel is lifted off the spec"
        );
        assert_eq!(dep.qualifier, Some("conda-forge".to_string()));
    }

    #[test]
    fn accepts_an_explicit_url() {
        let dep = parse_matchspec("https://example.com/numpy-1.0-0.conda").unwrap();
        assert!(dep.spec.url.is_some());
        assert_eq!(dep.qualifier, None);
    }

    #[test]
    fn rejects_invalid_syntax() {
        assert!(parse_matchspec("!!!not a matchspec!!!").is_err());
    }

    #[test]
    fn bare_channel_marker_parses_to_no_channel_and_no_qualifier() {
        let dep = parse_matchspec("::conda").unwrap();
        assert!(dep.spec.channel.is_none());
        assert_eq!(dep.qualifier, None);
    }

    #[test]
    fn a_bare_name_with_no_channel_marker_has_no_qualifier() {
        let dep = parse_matchspec("conda").unwrap();
        assert_eq!(dep.qualifier, None);
    }

    #[test]
    fn a_bare_alias_qualifier_is_recovered_as_its_short_name() {
        let dep = parse_matchspec("main::conda").unwrap();
        assert_eq!(dep.qualifier, Some("main".to_string()));
        assert!(dep.spec.channel.is_none());
    }

    #[test]
    fn the_bracket_channel_form_recovers_identically_to_the_positional_form() {
        let positional = parse_matchspec("main::conda").unwrap();
        let bracket = parse_matchspec("conda[channel=main]").unwrap();
        assert_eq!(positional.qualifier, bracket.qualifier);
        assert_eq!(
            positional.spec.channel.is_none(),
            bracket.spec.channel.is_none()
        );
    }

    #[test]
    fn a_url_shaped_qualifier_is_recovered_as_that_url() {
        let dep = parse_matchspec("https://repo.anaconda.com/pkgs/main::conda").unwrap();
        assert_eq!(
            dep.qualifier,
            Some("https://repo.anaconda.com/pkgs/main/".to_string())
        );
        assert!(dep.spec.channel.is_none());
    }

    #[test]
    fn a_platform_selector_is_recovered_as_subdir_independent_of_the_qualifier() {
        let dep = parse_matchspec("main/linux-64::conda").unwrap();
        assert_eq!(dep.qualifier, Some("main".to_string()));
        assert_eq!(dep.spec.subdir, Some("linux-64".to_string()));
    }

    #[test]
    fn to_string_never_renders_a_channel_once_lifted_off() {
        for text in [
            "main::conda",
            "conda[channel=main]",
            "https://repo.anaconda.com/pkgs/main::conda",
            "main/linux-64::conda",
            "::conda",
            "conda",
        ] {
            let dep = parse_matchspec(text).unwrap();
            assert!(
                !dep.spec.to_string().contains("::"),
                "{text:?} rendered as {:?}",
                dep.spec.to_string()
            );
        }
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
        let dep = parse_matchspec("https://example.com/numpy-1.0-0.conda").unwrap();
        let dep = Dependency::Matchspec(Box::new(dep));
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
        let dep = Dependency::Matchspec(Box::new(MatchspecDependency {
            spec,
            qualifier: None,
        }));
        assert_eq!(bare_name(&dep), None);
    }
}
