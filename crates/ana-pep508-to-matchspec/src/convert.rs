//! Per-[`Requirement`] orchestration: name + version + extras -> `MatchSpec`.
//!
//! Rust port of reroll's `pep508_to_matchspec()`, scoped down to markerless
//! requirements only -- see the crate's module docs for why. Name mapping
//! is also out of scope for this pass (the identity mapping is used, per
//! `investigations/pep508_to_matchspec_api.md`'s "Deferred: name mapping");
//! swapping it for a real `ana-pypi-conda-map` lookup later is a
//! single-function change at [`conda_name`], not a re-plumbing.

use rattler_conda_types::{MatchSpec, PackageName, PackageNameMatcher, ParseVersionError};
use uv_normalize::ExtraName;
use uv_pep508::{Requirement, VersionOrUrl};

use crate::version::version_spec;

/// CEP-26's package-name length limit, and CEP-29's `extras=[...]` bracket
/// key length limit -- both cap at 64 characters. `uv_pep508`'s
/// `PackageName`/`ExtraName` already guarantee every other part of each
/// grammar's shape (lowercase, alnum-bounded, single-separator-run, no
/// leading `_`) via PEP 503 name normalization, confirmed directly against
/// `uv-normalize` 0.12.6's own normalization routine: it only ever produces
/// `[a-z0-9]+(-[a-z0-9]+)*`, a strict subset of CEP-26's regex modulo
/// length, and PEP 508's own extra-name grammar means the same holds for
/// extras. (`uv-normalize` didn't always guarantee the "one or more"
/// part: uv#19435, landed between this crate's original `0.9.7` pin and
/// its current `0.12.6` one, closed a bug where an empty string silently
/// normalized to itself instead of being rejected -- not reachable through
/// this crate's own `PackageName`/`ExtraName` values today, since a PEP 508
/// requirement string has no syntax for an empty name or extra, but it
/// means this regex claim is now actually enforced end to end rather than
/// true "by accident" for every non-empty input.) Length is the one thing
/// normalization can't bound -- a PyPI
/// name can be arbitrarily long (the real-world "SEO spam name" case
/// reroll's own `conda_package_name.py` calls out), conda's can't -- so
/// it's the one check this module still does itself, and the only reason
/// `rattler_conda_types::PackageName::new_unchecked` (not `TryFrom`) is
/// safe to use at the [`conda_name`] call site: `TryFrom`'s own charset
/// check would be strictly redundant with what normalization already
/// guarantees, and (confirmed directly against `rattler_conda_types`
/// 0.52.0's actual source, not assumed) it doesn't check length at all, so
/// it wouldn't catch the one case that matters anyway.
const MAX_CEP26_NAME_LENGTH: usize = 64;

/// Converts one already-parsed PEP 508 [`Requirement`] into a conda
/// [`MatchSpec`], provided it has no environment marker -- see the crate's
/// module docs for why a marker is rejected outright rather than partially
/// converted.
///
/// `allow_pre` governs whether a pre-release *package* version is accepted
/// (default policy, matching reroll: rejected). It has no bearing on
/// markers, which are entirely out of scope regardless of this flag.
///
/// No string is formatted and reparsed to build the returned `MatchSpec`;
/// every field is constructed directly. See [`crate::version`] for the one
/// unavoidable string round-trip (an individual version literal).
pub fn convert(requirement: &Requirement, allow_pre: bool) -> Result<MatchSpec, ConvertError> {
    if !requirement.marker.is_true() {
        return Err(ConvertError::Marker {
            marker: requirement
                .marker
                .contents()
                .map(|contents| contents.to_string())
                .unwrap_or_default(),
        });
    }

    let version = match &requirement.version_or_url {
        None => None,
        Some(VersionOrUrl::Url(_)) => return Err(ConvertError::DirectUrl),
        Some(VersionOrUrl::VersionSpecifier(specifiers)) => version_spec(specifiers, allow_pre)?,
    };

    let name = conda_name(requirement.name.as_str())?;
    let extras = conda_extras(&requirement.extras)?;

    Ok(MatchSpec {
        name: PackageNameMatcher::Exact(name),
        version,
        extras,
        ..MatchSpec::default()
    })
}

/// Below this many requirements, convert them sequentially instead of
/// handing them to `rayon`. Mirrors `ana-pyproject`'s own
/// `PARALLEL_PARSE_THRESHOLD` (`crates/ana-pyproject/src/project.rs`) and
/// its reasoning: a single [`convert`] call is a handful of cheap checks
/// plus at most one small string round-trip, on the same order of
/// magnitude as (or cheaper than) `Requirement::from_str`; waking a parked
/// `rayon` worker thread is, in the worst case, an OS-scheduler round trip
/// an order of magnitude more expensive than that. Below a few dozen
/// requirements there is no plausible amount of parallelism that pays for
/// entering `rayon` at all -- a starting estimate, not a measured one, see
/// that constant's own docs.
const PARALLEL_CONVERT_THRESHOLD: usize = 64;

/// [`convert`], run over every element of `requirements` -- on `rayon`'s
/// work-stealing pool once there are enough of them to be worth it (see
/// [`PARALLEL_CONVERT_THRESHOLD`]), sequentially otherwise. Index-aligned
/// with `requirements`, so a caller can report every failing requirement
/// in one pass rather than fail-fast on the first `Err` (that doc's "Error
/// model summary").
///
/// Never constructs its own `rayon::ThreadPoolBuilder`: the parallel path
/// calls into the process-global pool via `into_par_iter`, same as
/// `ana-pyproject`'s own parallel requirement parsing, so the two stages
/// share cores instead of competing for them.
pub fn convert_all(
    requirements: &[Requirement],
    allow_pre: bool,
) -> Vec<Result<MatchSpec, ConvertError>> {
    if requirements.len() >= PARALLEL_CONVERT_THRESHOLD {
        use rayon::iter::{IntoParallelIterator, ParallelIterator};

        requirements
            .into_par_iter()
            .map(|requirement| convert(requirement, allow_pre))
            .collect()
    } else {
        requirements
            .iter()
            .map(|requirement| convert(requirement, allow_pre))
            .collect()
    }
}

/// `name` (already PEP 503-normalized by `uv_pep508`) as a conda
/// [`PackageName`] -- the identity mapping; see this module's docs'
/// reference to "Deferred: name mapping." `PackageName::new_unchecked` is
/// safe here specifically because normalization already guarantees every
/// part of CEP-26's shape except length, which this function checks
/// itself -- see [`MAX_CEP26_NAME_LENGTH`]'s docs for why that's the one
/// remaining gap and why `TryFrom` wouldn't close it anyway.
fn conda_name(name: &str) -> Result<PackageName, ConvertError> {
    if name.len() > MAX_CEP26_NAME_LENGTH {
        return Err(ConvertError::NameTooLong {
            name: name.to_string(),
            length: name.len(),
        });
    }
    Ok(PackageName::new_unchecked(name))
}

/// `extras`, deduplicated and sorted -- two distinct PEP 508 extras can
/// normalize to the same conda extra (e.g. `Foo-Bar`/`foo_bar` both ->
/// `foo-bar`; `uv_pep508` normalizes each independently but does not
/// deduplicate across a requirement's own extras list, confirmed directly
/// against its parser rather than assumed) -- and validated against
/// CEP-29's 64-character limit, per [`MAX_CEP26_NAME_LENGTH`]. `None` for
/// an empty (post-dedup) list, so a bracket-less matchspec is produced
/// rather than an explicit, useless `extras=[]` clause.
fn conda_extras(extras: &[ExtraName]) -> Result<Option<Vec<String>>, ConvertError> {
    let mut normalized: Vec<&str> = Vec::with_capacity(extras.len());
    for extra in extras {
        let extra = extra.as_str();
        if extra.len() > MAX_CEP26_NAME_LENGTH {
            return Err(ConvertError::ExtraTooLong {
                extra: extra.to_string(),
                length: extra.len(),
            });
        }
        normalized.push(extra);
    }
    normalized.sort_unstable();
    normalized.dedup();
    if normalized.is_empty() {
        return Ok(None);
    }
    Ok(Some(normalized.into_iter().map(str::to_string).collect()))
}

/// Every way [`convert`] (and [`crate::version`]'s specifier conversion it
/// calls into) can fail for one requirement. Deliberately narrower than
/// reroll's own `UnconvertableRequirementError`/`InvalidRequirementError`
/// split: `ana-pyproject` already rejects anything that fails to parse as
/// a `Requirement` at all (reroll's `InvalidRequirementError` case) before
/// a value ever reaches this crate, so every variant here is a value
/// problem, not a syntax one.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// `requirement.marker` is not `MarkerTree::TRUE` -- markers are out of
    /// scope for this crate; see the crate's module docs. `marker` is the
    /// marker's own rendered text (e.g. `sys_platform == "win32"`), empty
    /// only if `MarkerTree::contents()` itself returned `None` for a
    /// non-true marker, which is not known to happen in practice but isn't
    /// ruled out by `is_true()`'s own documented false-negative behavior.
    #[error(
        "requirement has an environment marker ({marker:?}); markers are not supported by \
         this converter"
    )]
    Marker { marker: String },

    /// A `name @ url` direct URL reference -- no matchspec equivalent.
    #[error("requirement has a direct URL reference, which has no matchspec equivalent")]
    DirectUrl,

    /// A version specifier's version has a local segment (`+local`) --
    /// never representable in a matchspec, regardless of `allow_pre`.
    #[error(
        "specifier {specifier:?} has a local version label, which conda match specs cannot \
         represent"
    )]
    LocalVersionLabel { specifier: String },

    /// A version specifier's version is a pre-release (or dev-release) and
    /// `allow_pre` was not set.
    #[error("specifier {specifier:?} is a pre-release version and allow_pre is unset")]
    Prerelease { specifier: String },

    /// `requirement.name`, once PEP 503-normalized, exceeds CEP-26's
    /// 64-character package-name limit.
    #[error("conda package name {name:?} exceeds {MAX_CEP26_NAME_LENGTH} characters ({length})")]
    NameTooLong { name: String, length: usize },

    /// One of `requirement.extras`, once PEP 503-normalized, exceeds
    /// CEP-29's 64-character extra-name limit.
    #[error(
        "extra {extra:?} exceeds {MAX_CEP26_NAME_LENGTH} characters once normalized ({length})"
    )]
    ExtraTooLong { extra: String, length: usize },

    /// A version literal this crate built itself (via
    /// [`crate::version`]'s CEP-33 formatting) failed to parse as a conda
    /// `Version`. Not expected to happen in practice -- see that module's
    /// docs -- but propagated rather than unwrapped.
    #[error("{literal:?} did not parse as a conda version literal: {source}")]
    InvalidVersionLiteral {
        literal: String,
        #[source]
        source: ParseVersionError,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::str::FromStr;

    use rattler_conda_types::{MatchSpec, ParseMatchSpecOptions};

    use super::*;

    fn req(spec: &str) -> Requirement {
        Requirement::from_str(spec).unwrap()
    }

    /// `expected` parsed as a conda matchspec, for comparing against
    /// [`convert`]'s output without hand-building a `MatchSpec` per test --
    /// same "compare against the parser's own understanding" approach as
    /// `version.rs`'s tests, and the one investigations/pep508_to_matchspec_api.md's
    /// testing-strategy section recommends. `with_extras(true)` because the
    /// `extras=[...]` bracket key is CEP-29/repodata-V3-gated behind that
    /// option -- confirmed directly against `rattler_conda_types` 0.52.0's
    /// own parser tests, not assumed -- even though every `MatchSpec` this
    /// crate itself produces sets `extras` as a plain typed field, never
    /// through this string parser.
    fn expect(expected: &str) -> MatchSpec {
        MatchSpec::from_str(expected, ParseMatchSpecOptions::lenient().with_extras(true)).unwrap()
    }

    mod name {
        use super::*;

        #[test]
        fn bare_name_passes_through_identity_mapped() {
            assert_eq!(
                convert(&req("requests"), false).unwrap(),
                expect("requests")
            );
        }

        #[test]
        fn name_is_normalized() {
            assert_eq!(
                convert(&req("Requests"), false).unwrap(),
                expect("requests")
            );
        }

        #[test]
        fn name_normalizes_separators_too() {
            assert_eq!(
                convert(&req("Foo_Bar.BAZ"), false).unwrap(),
                expect("foo-bar-baz")
            );
        }

        #[test]
        fn versioned_dependency_keeps_the_normalized_name() {
            assert_eq!(
                convert(&req("requests>=2.0.0"), false).unwrap(),
                expect(r#"requests[version=">=2.0.0"]"#)
            );
        }

        #[test]
        fn name_over_64_characters_is_rejected() {
            let name = "a".repeat(65);
            let err = convert(&req(&name), false).unwrap_err();
            assert!(matches!(err, ConvertError::NameTooLong { .. }), "{err:?}");
        }

        #[test]
        fn name_at_exactly_64_characters_is_accepted() {
            let name = "a".repeat(64);
            assert_eq!(convert(&req(&name), false).unwrap(), expect(&name));
        }
    }

    mod version {
        use super::*;

        #[test]
        fn operator_is_passed_through_as_is() {
            for operator in [">=", "<=", "!="] {
                let spec = format!("requests{operator}2.0.0");
                assert_eq!(
                    convert(&req(&spec), false).unwrap(),
                    expect(&format!(r#"requests[version="{operator}2.0.0"]"#))
                );
            }
        }

        #[test]
        fn strict_less_than_gets_the_pre_release_carve_out_anchor() {
            assert_eq!(
                convert(&req("requests<2.0.0"), false).unwrap(),
                expect(r#"requests[version="<2.0.0a0"]"#)
            );
        }

        #[test]
        fn multiple_specifiers_are_joined_in_canonical_order() {
            assert_eq!(
                convert(&req("requests<=2.0.0,!=1.0.1,>=0.9"), false).unwrap(),
                expect(r#"requests[version=">=0.9,<=2.0.0,!=1.0.1"]"#)
            );
        }

        #[test]
        fn compatible_release_expands_to_a_range() {
            assert_eq!(
                convert(&req("requests~=3.13.2"), false).unwrap(),
                expect(r#"requests[version=">=3.13.2,<3.14.0a0"]"#)
            );
        }

        #[test]
        fn rejects_a_local_version_label() {
            let err = convert(&req("requests==1.0.0+local"), false).unwrap_err();
            assert!(
                matches!(err, ConvertError::LocalVersionLabel { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn rejects_a_pre_release_version_by_default() {
            let err = convert(&req("requests==1.0.0rc1"), false).unwrap_err();
            assert!(matches!(err, ConvertError::Prerelease { .. }), "{err:?}");
        }

        #[test]
        fn allow_pre_permits_a_pre_release_version() {
            assert_eq!(
                convert(&req("requests==1.0.0rc1"), true).unwrap(),
                expect(r#"requests[version="==1.0.0.rc1"]"#)
            );
        }
    }

    mod rejections {
        use super::*;

        #[test]
        fn rejects_a_direct_url_reference() {
            let err = convert(
                &req("requests @ https://example.com/requests-1.0.0.whl"),
                false,
            )
            .unwrap_err();
            assert!(matches!(err, ConvertError::DirectUrl), "{err:?}");
        }

        /// Ported from reroll's `test_pep508_to_matchspec.py`'s
        /// `test_local_version_label_with_strict_less_or_greater_than_is_rejected_by_packaging`:
        /// unlike `==`/`!=`/`===`, a local label combined with `<`/`>`
        /// never reaches this crate's own local-version-label check at
        /// all -- `uv_pep508::Requirement::from_str` itself already
        /// rejects the combination (confirmed directly against
        /// `uv-pep440` 0.12.6's `Operator::is_local_compatible`, not
        /// assumed; see [`crate::version::reject_unsupported_version`]'s
        /// docs), so there is no `Requirement` for [`convert`] to reject
        /// in the first place.
        #[test]
        fn local_version_label_with_strict_less_or_greater_than_is_rejected_at_parse_time() {
            for entry in ["requests<1.0.0+local", "requests>1.0.0+local"] {
                let parsed: Result<Requirement, _> = Requirement::from_str(entry);
                assert!(
                    parsed.is_err(),
                    "{entry:?} should fail to parse as a Requirement at all"
                );
            }
        }
    }

    mod markers {
        use super::*;

        #[test]
        fn virtual_package_marker_is_rejected() {
            let err =
                convert(&req(r#"requests>=2.0.0; sys_platform == "win32""#), false).unwrap_err();
            assert!(matches!(err, ConvertError::Marker { .. }), "{err:?}");
        }

        #[test]
        fn extra_marker_is_rejected() {
            let err = convert(&req(r#"requests>=2.0.0; extra == "foo""#), false).unwrap_err();
            assert!(matches!(err, ConvertError::Marker { .. }), "{err:?}");
        }

        #[test]
        fn marker_error_carries_the_marker_text() {
            let err = convert(&req(r#"requests; sys_platform == "win32""#), false).unwrap_err();
            match err {
                ConvertError::Marker { marker } => {
                    assert!(marker.contains("sys_platform"), "{marker:?}");
                }
                other => panic!("expected ConvertError::Marker, got {other:?}"),
            }
        }

        /// Ported from reroll's `test_pep508_to_matchspec.py`'s
        /// `test_extra_marker_rejection_takes_precedence_over_a_direct_url`:
        /// [`convert`] checks `requirement.marker` before it ever inspects
        /// `requirement.version_or_url`, so a requirement with *both* a
        /// direct URL and a marker is rejected as [`ConvertError::Marker`],
        /// not [`ConvertError::DirectUrl`].
        #[test]
        fn marker_rejection_takes_precedence_over_a_direct_url() {
            let entry = r#"requests @ https://example.com/pkg.whl ; extra == "foo""#;
            let err = convert(&req(entry), false).unwrap_err();
            assert!(matches!(err, ConvertError::Marker { .. }), "{err:?}");
        }

        /// The remaining `extra`-clause shapes reroll's
        /// `test_pep508_to_matchspec.py`'s `TestMarkers` dedicates separate
        /// tests to (combined `extra` clauses, `extra` mixed with an
        /// environment condition, a reversed comparison operand order, and
        /// membership tests against `extra`) all collapse to the same
        /// outcome here: *any* non-`MarkerTree::TRUE` marker is rejected
        /// uniformly, regardless of its internal shape -- there is no
        /// per-key or per-operator marker-parsing logic in this crate to
        /// exercise each shape separately against (that logic is reroll's
        /// own `marker_condition`, out of scope per the crate's module
        /// docs). One representative test per input shape, all folding to
        /// the same `ConvertError::Marker` arm, is enough to pin that this
        /// crate's marker check has no gap for any of them.
        ///
        /// Deliberately excludes reroll's `"foo" in extra` and `"3.9" not
        /// in python_version` (reversed `in`/`not in` operand order) --
        /// see [`reversed_in_marker_is_silently_accepted_not_rejected`]
        /// and [`reversed_not_in_marker_is_silently_accepted_not_rejected`]
        /// just below for why those two do *not* belong in this list.
        #[test]
        fn every_other_marker_shape_is_also_rejected_uniformly() {
            for entry in [
                r#"requests>=2.0.0; extra == "foo" or extra == "bar""#,
                r#"requests>=2.0.0; extra == "foo" and python_version >= "3.8""#,
                r#"requests>=2.0.0; "foo" == extra"#,
                r#"requests>=2.0.0; platform_machine == "x86_64""#,
                r#"requests>=2.0.0; os_name != "nt""#,
                r#"requests>=2.0.0; sys_platform >= "linux""#,
                r#"requests>=2.0.0; sys_platform == "cygwin""#,
                r#"requests>=2.0.0; python_version ~= "3.9""#,
                r#"requests>=2.0.0; python_full_version ~= "3.9.0""#,
            ] {
                let err = convert(&req(entry), false).unwrap_err();
                assert!(
                    matches!(err, ConvertError::Marker { .. }),
                    "{entry}: {err:?}"
                );
            }
        }

        /// Reroll's `test_reversed_in_marker_raises` and
        /// `test_reversed_not_in_marker_raises` assert that a reversed-operand
        /// `in`/`not in` marker (literal on the left: `"foo" in extra`,
        /// `"3.9" not in python_version` -- as opposed to the supported
        /// `python_version in "3.9"` shape) raises `UnconvertableMarkerError`
        /// in reroll's own parser.
        ///
        /// **This crate's dependency, `uv_pep508` 0.12.6, does not raise for
        /// either shape and does not preserve them as a real constraint
        /// either**: `Requirement::from_str` parses both successfully, and
        /// the resulting `marker.is_true()` is `true` -- confirmed directly
        /// against `uv_pep508` 0.12.6's own `MarkerTree`, not assumed (and
        /// re-confirmed unchanged across the crate's `0.9.7` -> `0.12.6`
        /// pin bump: same two inputs, same `is_true()` result, checked
        /// against both tags directly). That
        /// means [`convert`] does *not* reject either shape today: the
        /// dependency is silently treated as marker-free and converted as
        /// if the reversed clause were never written, which is a real
        /// divergence from reroll's own (stricter, matching-tested-against-
        /// real-`packaging`) behavior. Pinned here as its own two tests,
        /// deliberately asserting the *current* (surprising) behavior
        /// rather than the desired one, so a future `uv_pep508` upgrade
        /// that starts parsing these shapes into a real, non-trivial
        /// `MarkerTree` turns into a loud, obvious test failure here
        /// instead of a silent behavior change.
        #[test]
        fn reversed_in_marker_is_silently_accepted_not_rejected() {
            let entry = r#"requests>=2.0.0; "foo" in extra"#;
            assert!(
                convert(&req(entry), false).is_ok(),
                "if this now fails, uv_pep508 has started parsing reversed `in` as a real \
                 constraint -- update this test and consider whether ConvertError::Marker \
                 should fire instead"
            );
        }

        #[test]
        fn reversed_not_in_marker_is_silently_accepted_not_rejected() {
            let entry = r#"requests>=2.0.0; "3.9" not in python_version"#;
            assert!(
                convert(&req(entry), false).is_ok(),
                "if this now fails, uv_pep508 has started parsing reversed `not in` as a real \
                 constraint -- update this test and consider whether ConvertError::Marker \
                 should fire instead"
            );
        }

        /// A reversed-operand compatible-release comparison against a pure
        /// string marker field (literal on the left: `"posix" ~= os_name`,
        /// as opposed to the never-meaningful-either-way normal-order
        /// `os_name ~= "posix"`) used to reach an `unreachable!()` panic in
        /// `uv_pep508` 0.9.7's marker algebra -- confirmed directly by
        /// pinning this crate's workspace to `uv-pep508` `0.9.7` and
        /// running `Requirement::from_str` on this exact string outside
        /// this crate's own `#[deny(clippy::unwrap_used)]`-guarded code
        /// (parsing happens inside `uv_pep508` itself, so no `unwrap`/
        /// `expect` in this crate's own source could have caught it): the
        /// process aborted with `internal error: entered unreachable code:
        /// string comparisons with ~= are ignored`
        /// (`uv-pep508/src/marker/algebra.rs`), not a `Result::Err`. That
        /// means any `pyproject.toml` containing this exact marker shape
        /// crashed the whole process on the old pin -- a real
        /// denial-of-service bug for `ana-pyproject`'s explicit
        /// never-panic-on-untrusted-input contract (see that crate's
        /// `project.rs` module docs), not merely a `ConvertError::Marker`
        /// case this crate declines to convert.
        ///
        /// **Fixed by the `uv-pep508` 0.9.7 -> 0.12.6 bump**: uv#19782
        /// ("Ignore reversed string compatible-release markers") applies
        /// the same "`~=` is not meaningful for strings, ignore it" guard
        /// already in place for the normal-order form to the reversed one
        /// too. Post-fix, `Requirement::from_str` parses this string
        /// successfully and `marker.is_true()` is `true` -- confirmed
        /// directly against `uv_pep508` 0.12.6, not assumed -- so this
        /// falls into the exact same "silently accepted, not rejected"
        /// category as [`reversed_in_marker_is_silently_accepted_not_rejected`]
        /// above, once parsing gets far enough for [`convert`] to see it at
        /// all. Pinned here the same way, so a future `uv_pep508` upgrade
        /// that starts treating this shape as a real constraint (or
        /// reintroduces the panic) is a loud test failure, not a silent
        /// regression.
        #[test]
        fn reversed_compatible_release_string_marker_is_silently_accepted_not_rejected() {
            let entry = r#"requests>=2.0.0; "posix" ~= os_name"#;
            assert!(
                convert(&req(entry), false).is_ok(),
                "if this now fails, uv_pep508 has started treating a reversed string `~=` \
                 marker as a real constraint (or panics again) -- update this test and \
                 consider whether ConvertError::Marker should fire instead"
            );
        }
    }

    /// Reroll's marker *conversion* tests (the `when=` clause this crate
    /// deliberately does not build -- see the crate's module docs and
    /// [`convert`]'s own docs for why every marker, not just an
    /// unconvertible one, is rejected outright for now) ported as
    /// `#[ignore]`d placeholders rather than dropped outright, so the
    /// exact reroll behavior each one pins is on record for whichever
    /// future `ana-marker-matchspec` integration replaces the blanket
    /// [`ConvertError::Marker`] rejection with real conversion.
    ///
    /// Each ignore reason names the source: reroll's
    /// `test_pep508_to_matchspec.py::TestMarkers`. The equivalence-oracle
    /// tests from reroll's `test_marker_matchspec_equivalence.py` (all of
    /// it marker-conversion output, none of it rejection) are the same
    /// shape and are omitted here rather than stubbed one-by-one; port
    /// those from `tests/marker_oracle.py`'s `assert_matchspec_agrees_with_pip`
    /// the same way `version.rs`'s `equivalence_oracle` module ports
    /// `tests/version_oracle.py`, once marker conversion exists to test.
    #[cfg(test)]
    #[allow(dead_code, unused_imports, clippy::unwrap_used)]
    mod markers_deferred {
        use super::*;

        const DEFERRED: &str =
            "marker conversion (a `when=` clause) is out of scope for this crate; \
             every marker is rejected outright for now -- see ana-marker-matchspec";

        #[test]
        #[ignore = "DEFERRED: reroll test_virtual_package_marker_becomes_a_when_clause"]
        fn virtual_package_marker_becomes_a_when_clause() {
            // `requests>=2.0.0; sys_platform == "win32"` -> `requests >=2.0.0[when="__win"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_bare_name_with_marker_has_no_version_outside_the_brackets"]
        fn bare_name_with_marker_has_no_version_outside_the_brackets() {
            // `requests; sys_platform == "win32"` -> `requests[when="__win"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_python_version_marker_converts_per_the_table"]
        fn python_version_marker_converts_per_the_table() {
            // `requests; python_version >= "3.9"` -> `requests[when="python>=3.9.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_python_version_equality_marker_produces_a_rattler_valid_when_clause"]
        fn python_version_equality_marker_produces_a_rattler_valid_when_clause() {
            // `requests; python_version == "3.9"` ->
            // `requests[when="python>=3.9.0a0,<3.10.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_python_version_inequality_marker_produces_a_rattler_valid_when_clause"]
        fn python_version_inequality_marker_produces_a_rattler_valid_when_clause() {
            // `requests; python_version != "3.9"` -> `requests[when="python!=3.9.*"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_combined_marker_preserves_and_or_structure"]
        fn combined_marker_preserves_and_or_structure() {
            // `requests; sys_platform == "win32" and python_version >= "3.9"` ->
            // `requests[when="__win and python>=3.9.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_three_term_and_chain_is_fully_parenthesized"]
        fn three_term_and_chain_is_fully_parenthesized() {
            // `requests; sys_platform == "win32" and os_name == "posix" and
            // python_version >= "3.9"` ->
            // `requests[when="(__win and __unix) and python>=3.9.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_mixed_or_and_precedence_parenthesizes_the_tighter_and_group"]
        fn mixed_or_and_precedence_parenthesizes_the_tighter_and_group() {
            // `requests; sys_platform == "win32" or os_name == "posix" and
            // python_version >= "3.9"` ->
            // `requests[when="__win or (__unix and python>=3.9.0a0)"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_explicit_parens_around_an_or_group_are_preserved"]
        fn explicit_parens_around_an_or_group_are_preserved() {
            // `requests; (sys_platform == "win32" or os_name == "posix") and
            // python_version >= "3.9"` ->
            // `requests[when="(__win or __unix) and python>=3.9.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_reversed_comparison_operand_order_still_converts"]
        fn reversed_comparison_operand_order_still_converts() {
            // `requests; "3.9" <= python_version` -> `requests[when="python>=3.9.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_reversed_virtual_package_operand_order_still_converts"]
        fn reversed_virtual_package_operand_order_still_converts() {
            // `requests; "win32" == sys_platform` -> `requests[when="__win"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre"]
        fn prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre() {
            // `requests; python_full_version >= "3.9.0rc1"` ->
            // `requests[when="python>=3.9.0.rc1"]`, with `allow_pre=false` --
            // `allow_pre` governs the *package* version only, never markers.
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_full_version_glob_marker_produces_a_rattler_valid_when_clause"]
        fn full_version_glob_marker_produces_a_rattler_valid_when_clause() {
            // `numpy<1.25.0,>=1.24.0; python_full_version == "3.8.*"` ->
            // `numpy >=1.24.0,<1.25.0a0[when="python=3.8"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_extras_and_marker_combine_in_one_bracket"]
        fn extras_and_marker_combine_in_one_bracket() {
            // `fastapi[all]>=1.0; sys_platform == "win32"` ->
            // `fastapi >=1.0[extras=[all],when="__win"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_python_version_literal_with_only_a_major_segment_converts"]
        fn python_version_literal_with_only_a_major_segment_converts() {
            // `requests>=2.0.0; python_version == "3"` ->
            // `requests >=2.0.0[when="python>=3.0.0a0,<3.1.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_in_marker_converts_via_the_membership_rewrite"]
        fn in_marker_converts_via_the_membership_rewrite() {
            // `requests>=2.0.0; python_version in "3.9"` (abi3_upper_bound="3.9") ->
            // `requests >=2.0.0[when="python>=3.9.0a0,<3.10.0a0"]`
            unimplemented!("{DEFERRED}");
        }

        #[test]
        #[ignore = "DEFERRED: reroll test_not_in_marker_converts_via_the_membership_rewrite"]
        fn not_in_marker_converts_via_the_membership_rewrite() {
            // `requests>=2.0.0; python_version not in "3.9"` (abi3_upper_bound="3.9") ->
            // `requests >=2.0.0[when="python!=3.9.*"]`
            unimplemented!("{DEFERRED}");
        }

        /// Reroll's `test_marker_matchspec_equivalence.py` in full (270
        /// lines, `python_version`/`python_full_version`/
        /// `implementation_version` marker conversion checked against
        /// pip/uv's own marker evaluation via `tests/marker_oracle.py`) --
        /// stubbed as one placeholder for the whole file rather than one
        /// per case, since none of it is testable until marker conversion
        /// exists at all. Port it the same way `version.rs`'s
        /// `equivalence_oracle` module ports `tests/version_oracle.py`
        /// once it does.
        #[test]
        #[ignore = "DEFERRED: reroll test_marker_matchspec_equivalence.py (whole file, 270 lines)"]
        fn marker_equivalence_oracle_suite() {
            unimplemented!("{DEFERRED}");
        }
    }

    mod extras {
        use super::*;

        #[test]
        fn bare_extra_becomes_an_extras_bracket() {
            assert_eq!(
                convert(&req("fastapi[all]"), false).unwrap(),
                expect("fastapi[extras=[all]]")
            );
        }

        #[test]
        fn extras_come_after_the_version() {
            assert_eq!(
                convert(&req("fastapi[all]>=1.0"), false).unwrap(),
                expect(r#"fastapi[version=">=1.0",extras=[all]]"#)
            );
        }

        #[test]
        fn multiple_extras_are_normalized_and_sorted() {
            assert_eq!(
                convert(&req("fastapi[Standard,ALL]"), false).unwrap(),
                expect("fastapi[extras=[all,standard]]")
            );
        }

        #[test]
        fn extra_name_is_normalized() {
            assert_eq!(
                convert(&req("fastapi[Some_Extra.Name]"), false).unwrap(),
                expect("fastapi[extras=[some-extra-name]]")
            );
        }

        #[test]
        fn extra_name_over_64_characters_is_rejected() {
            let entry = format!("fastapi[{}]", "a".repeat(65));
            let err = convert(&req(&entry), false).unwrap_err();
            assert!(matches!(err, ConvertError::ExtraTooLong { .. }), "{err:?}");
        }

        #[test]
        fn extra_name_at_exactly_64_characters_is_accepted() {
            let extra = "a".repeat(64);
            let entry = format!("fastapi[{extra}]");
            assert_eq!(
                convert(&req(&entry), false).unwrap(),
                expect(&format!("fastapi[extras=[{extra}]]"))
            );
        }

        #[test]
        fn empty_extras_brackets_produce_no_extras_clause() {
            assert_eq!(
                convert(&req("fastapi[]"), false).unwrap(),
                expect("fastapi")
            );
        }

        #[test]
        fn duplicate_extras_after_normalization_are_deduplicated() {
            assert_eq!(
                convert(&req("fastapi[Foo-Bar,foo_bar]"), false).unwrap(),
                expect("fastapi[extras=[foo-bar]]")
            );
        }

        #[test]
        fn any_invalid_extra_length_raises_even_when_others_are_valid() {
            let entry = format!("fastapi[valid,{}]", "a".repeat(65));
            let err = convert(&req(&entry), false).unwrap_err();
            assert!(matches!(err, ConvertError::ExtraTooLong { .. }), "{err:?}");
        }
    }

    mod integration {
        use super::*;

        #[test]
        fn name_version_and_extras_all_combine() {
            let entry = "Foo_Bar.BAZ[Extra1,extra_2]~=1.2.3rc1";
            assert_eq!(
                convert(&req(entry), true).unwrap(),
                expect(r#"foo-bar-baz[version=">=1.2.3.rc1,<1.3.0a0",extras=[extra-2,extra1]]"#)
            );
        }
    }

    mod convert_all_batch {
        use super::*;

        #[test]
        fn is_index_aligned_with_its_input() {
            let requirements = vec![req("requests"), req("requests @ https://example.com/x.whl")];
            let results = convert_all(&requirements, false);

            assert_eq!(results.len(), 2);
            assert!(results[0].is_ok());
            assert!(matches!(results[1], Err(ConvertError::DirectUrl)));
        }

        /// Same assertion as [`is_index_aligned_with_its_input`], but with
        /// enough requirements to cross [`PARALLEL_CONVERT_THRESHOLD`] and
        /// take the `rayon` path instead of the sequential one.
        #[test]
        fn is_index_aligned_with_its_input_above_the_parallel_threshold() {
            let mut requirements: Vec<Requirement> = (0..PARALLEL_CONVERT_THRESHOLD)
                .map(|i| req(&format!("requests{i}")))
                .collect();
            requirements.push(req("requests @ https://example.com/x.whl"));

            let results = convert_all(&requirements, false);

            assert_eq!(results.len(), PARALLEL_CONVERT_THRESHOLD + 1);
            assert!(results[..PARALLEL_CONVERT_THRESHOLD]
                .iter()
                .all(Result::is_ok));
            assert!(matches!(
                results[PARALLEL_CONVERT_THRESHOLD],
                Err(ConvertError::DirectUrl)
            ));
        }
    }
}
