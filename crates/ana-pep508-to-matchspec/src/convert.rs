//! Per-[`Requirement`] orchestration: name + version + extras + marker ->
//! `MatchSpec`. See the crate's module docs for the three-outcome return
//! shape and why `extra` clauses are this crate's own concern rather than
//! `ana-marker-matchspec`'s.
//!
//! Name mapping ([`conda_name`]) consults the `pypi_name -> conda_name`
//! lookup table every caller supplies; a name absent from the table keeps
//! the identity mapping.

use ana_marker_matchspec::{Applicability, Unconvertible};
use ana_pypi_conda_map::MappingHandle;
use rattler_conda_types::{MatchSpec, PackageName, PackageNameMatcher, ParseVersionError};
use uv_normalize::ExtraName;
use uv_pep508::{MarkerTree, Requirement, VersionOrUrl};

#[cfg(test)]
use rattler_conda_types::MatchSpecCondition;

use crate::version::version_spec;

/// CEP-26's package-name length limit, and CEP-29's `extras=[...]` bracket
/// key length limit -- both cap at 64 characters. PEP 503 normalization
/// (`uv_pep508`'s `PackageName`/`ExtraName`, or [`MappingHandle::get`] for
/// a mapped name) already guarantees every other part of the shape; length
/// is the one thing it can't bound, since a PyPI name can be arbitrarily
/// long, so it's the one check this module does itself before using
/// `PackageName::new_unchecked`.
const MAX_CEP26_NAME_LENGTH: usize = 64;

/// Converts one already-parsed PEP 508 [`Requirement`] into a conda
/// [`MatchSpec`], or `None` if `requirement`'s marker can never hold on
/// Converts one already-parsed PEP 508 [`Requirement`] into a conda
/// [`MatchSpec`], or `None` if `requirement`'s marker can never hold on
/// the machine `assumption` describes -- see the crate's module docs for
/// this three-outcome shape and why it isn't `Result<MatchSpec, _>`.
///
/// `allow_pre` governs whether a pre-release *package* version is
/// accepted; it has no bearing on markers, which have no `allow_pre`
/// concept.
///
/// `assumption` is [`ana_marker_matchspec::known_values_assumption`]'s
/// output for the subdir being installed onto, built once by the caller
/// and reused across every [`convert`]/[`convert_all`] call.
///
/// `pypi_to_conda_map` is the lookup table [`conda_name`] consults; always
/// a real handle, never optional. Tests that don't care about mapping
/// behavior use `MappingHandle::from_map(HashMap::new())`, identical to
/// every name being absent from a real table.
///
/// A marker containing an `extra == "..."` clause is checked for, and
/// rejected with [`ConvertError::Marker`], before ever calling into
/// `ana-marker-matchspec`: this crate has no notion of which extras are
/// "active" for the current install.
///
/// No string is formatted and reparsed to build the returned `MatchSpec`;
/// every field is constructed directly except an individual version
/// literal (see [`crate::version`]).
pub fn convert(
    requirement: &Requirement,
    allow_pre: bool,
    assumption: MarkerTree,
    pypi_to_conda_map: &MappingHandle,
) -> Result<Option<MatchSpec>, ConvertError> {
    if marker_has_extra_clause(requirement.marker) {
        return Err(ConvertError::Marker {
            marker: requirement
                .marker
                .contents()
                .map(|contents| contents.to_string())
                .unwrap_or_default(),
        });
    }

    let condition =
        match ana_marker_matchspec::to_matchspec_condition(requirement.marker, assumption) {
            Ok(Applicability::Never) => return Ok(None),
            Ok(Applicability::Always) => None,
            Ok(Applicability::Conditionally(condition)) => Some(condition),
            Err(unconvertible) => return Err(ConvertError::UnconvertibleMarker(unconvertible)),
        };

    let version = match &requirement.version_or_url {
        None => None,
        Some(VersionOrUrl::Url(_)) => return Err(ConvertError::DirectUrl),
        Some(VersionOrUrl::VersionSpecifier(specifiers)) => version_spec(specifiers, allow_pre)?,
    };

    let name = conda_name(requirement.name.as_str(), pypi_to_conda_map)?;
    let extras = conda_extras(&requirement.extras)?;

    Ok(Some(MatchSpec {
        name: PackageNameMatcher::Exact(name),
        version,
        extras,
        condition,
        ..MatchSpec::default()
    }))
}

/// Whether `marker` contains an `extra == "..."`/`extra != "..."` clause
/// anywhere in its structure, regardless of `and`/`or` nesting -- see
/// [`convert`]'s docs for why any such clause is rejected outright.
fn marker_has_extra_clause(marker: MarkerTree) -> bool {
    let mut found = false;
    marker.visit_extras(|_operator, _extra| found = true);
    found
}

/// Below this many requirements, convert them sequentially instead of
/// handing them to `rayon`: waking a parked worker thread costs roughly an
/// order of magnitude more than a single [`convert`] call, so parallelism
/// only pays off once there are several dozen requirements.
const PARALLEL_CONVERT_THRESHOLD: usize = 64;

/// [`convert`], run over every element of `requirements` -- on `rayon`'s
/// work-stealing pool once there are enough of them to be worth it (see
/// [`PARALLEL_CONVERT_THRESHOLD`]), sequentially otherwise. Index-aligned
/// with `requirements`, so a caller can report every failing requirement
/// in one pass rather than fail-fast on the first `Err`.
///
/// Uses the process-global `rayon` pool via `into_par_iter` rather than
/// its own `ThreadPoolBuilder`.
///
/// Generic over borrowed or owned requirements (`&[Requirement]` and
/// `&[&Requirement]` both work), so callers holding requirements inside a
/// larger struct don't have to deep-clone them into a slice first.
pub fn convert_all<R: std::borrow::Borrow<Requirement> + Sync>(
    requirements: &[R],
    allow_pre: bool,
    assumption: MarkerTree,
    pypi_to_conda_map: &MappingHandle,
) -> Vec<Result<Option<MatchSpec>, ConvertError>> {
    if requirements.len() >= PARALLEL_CONVERT_THRESHOLD {
        use rayon::iter::{IntoParallelIterator, ParallelIterator};

        requirements
            .into_par_iter()
            .map(|requirement| {
                convert(
                    requirement.borrow(),
                    allow_pre,
                    assumption,
                    pypi_to_conda_map,
                )
            })
            .collect()
    } else {
        requirements
            .iter()
            .map(|requirement| {
                convert(
                    requirement.borrow(),
                    allow_pre,
                    assumption,
                    pypi_to_conda_map,
                )
            })
            .collect()
    }
}

/// `name` (already PEP 503-normalized by `uv_pep508`), mapped through
/// `pypi_to_conda_map` if it has a valid-shaped entry for it, or kept
/// unchanged otherwise. A mapped value with an invalid shape surfaces as
/// [`ConvertError::InvalidMappedName`] rather than reaching
/// `PackageName::new_unchecked` unchecked.
fn conda_name(name: &str, pypi_to_conda_map: &MappingHandle) -> Result<PackageName, ConvertError> {
    let name = pypi_to_conda_map.get(name)?;
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
/// `foo-bar`), and validated against CEP-29's 64-character limit, per
/// [`MAX_CEP26_NAME_LENGTH`]. `None` for an empty (post-dedup) list, so a
/// bracket-less matchspec is produced rather than an explicit `extras=[]`.
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
/// calls into) can fail for one requirement. Every variant here is a
/// value problem, not a syntax one -- `ana-pyproject` already rejects
/// anything that fails to parse as a `Requirement` before it reaches this
/// crate.
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// `requirement.marker` contains an `extra == "..."` clause -- this
    /// crate's own, permanent scope boundary; see [`convert`]'s docs.
    /// `marker` is the marker's rendered text, empty only if
    /// `MarkerTree::contents()` itself returned `None`.
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

    /// [`MappingHandle::get`] found an entry for `requirement.name` but
    /// its mapped value doesn't have a valid conda-package-name shape.
    #[error("invalid pypi-to-conda mapping entry: {0}")]
    InvalidMappedName(#[from] ana_pypi_conda_map::InvalidMappedName),

    /// A version literal this crate built itself (via [`crate::version`]'s
    /// CEP-33 formatting) failed to parse as a conda `Version`. Not
    /// expected to happen in practice, but propagated rather than
    /// unwrapped.
    #[error("{literal:?} did not parse as a conda version literal: {source}")]
    InvalidVersionLiteral {
        literal: String,
        #[source]
        source: ParseVersionError,
    },

    /// `ana-marker-matchspec` couldn't represent `requirement.marker` as a
    /// matchspec condition (a key with no matchspec equivalent, or a
    /// genuinely unsupported comparator) -- propagated rather than
    /// re-wrapped, since `Unconvertible`'s own variants are already
    /// specific about the problem.
    #[error("marker could not be converted to a matchspec condition: {0}")]
    UnconvertibleMarker(#[from] Unconvertible),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use rattler_conda_types::{MatchSpec, ParseMatchSpecOptions};

    use super::*;

    fn req(spec: &str) -> Requirement {
        Requirement::from_str(spec).unwrap()
    }

    /// A `MappingHandle` with no entries, for tests that don't care about
    /// name mapping. Behaves identically to every name being absent from a
    /// real table.
    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    /// The fixed, deterministic test target -- `linux-64`, regardless of
    /// whatever platform runs these tests. The `markers` module uses a
    /// different subdir explicitly where that matters.
    fn assumption() -> MarkerTree {
        ana_marker_matchspec::known_values_assumption(rattler_conda_types::Platform::Linux64)
            .unwrap()
    }

    /// [`convert`] against [`assumption`], asserting the requirement
    /// applies on `linux-64` (not `Applicability::Never`) and unwrapping
    /// the rest.
    fn convert_ok(requirement: &Requirement, allow_pre: bool) -> MatchSpec {
        convert(requirement, allow_pre, assumption(), &no_mapping())
            .unwrap()
            .expect("expected the requirement to apply on linux-64, not Applicability::Never")
    }

    /// [`convert`] against [`assumption`], asserting failure.
    fn convert_err(requirement: &Requirement, allow_pre: bool) -> ConvertError {
        convert(requirement, allow_pre, assumption(), &no_mapping()).unwrap_err()
    }

    /// `expected` parsed as a conda matchspec, for comparing against
    /// [`convert`]'s output without hand-building a `MatchSpec` per test.
    /// `with_extras(true)` because the `extras=[...]` bracket key is gated
    /// behind that option in the string parser.
    fn expect(expected: &str) -> MatchSpec {
        MatchSpec::from_str(expected, ParseMatchSpecOptions::lenient().with_extras(true)).unwrap()
    }

    mod name {
        use super::*;

        #[test]
        fn bare_name_passes_through_identity_mapped() {
            assert_eq!(convert_ok(&req("requests"), false), expect("requests"));
        }

        #[test]
        fn name_is_normalized() {
            assert_eq!(convert_ok(&req("Requests"), false), expect("requests"));
        }

        #[test]
        fn name_normalizes_separators_too() {
            assert_eq!(
                convert_ok(&req("Foo_Bar.BAZ"), false),
                expect("foo-bar-baz")
            );
        }

        #[test]
        fn versioned_dependency_keeps_the_normalized_name() {
            assert_eq!(
                convert_ok(&req("requests>=2.0.0"), false),
                expect(r#"requests[version=">=2.0.0"]"#)
            );
        }

        #[test]
        fn name_over_64_characters_is_rejected() {
            let name = "a".repeat(65);
            let err = convert_err(&req(&name), false);
            assert!(matches!(err, ConvertError::NameTooLong { .. }), "{err:?}");
        }

        #[test]
        fn name_at_exactly_64_characters_is_accepted() {
            let name = "a".repeat(64);
            assert_eq!(convert_ok(&req(&name), false), expect(&name));
        }

        #[test]
        fn mapped_name_is_replaced() {
            let handle = MappingHandle::from_map(HashMap::from([(
                "opencv-python".to_string(),
                "py-opencv".to_string(),
            )]));
            let result = convert(&req("opencv-python"), false, assumption(), &handle)
                .unwrap()
                .unwrap();
            assert_eq!(result, expect("py-opencv"));
        }

        #[test]
        fn unmapped_name_with_a_nonempty_table_still_passes_through_identity_mapped() {
            let handle = MappingHandle::from_map(HashMap::from([(
                "opencv-python".to_string(),
                "py-opencv".to_string(),
            )]));
            let result = convert(&req("requests"), false, assumption(), &handle)
                .unwrap()
                .unwrap();
            assert_eq!(result, expect("requests"));
        }

        #[test]
        fn empty_table_is_identity_mapped() {
            let result = convert(&req("opencv-python"), false, assumption(), &no_mapping())
                .unwrap()
                .unwrap();
            assert_eq!(result, expect("opencv-python"));
        }

        /// The length check applies to the *mapped* name, not the
        /// original PyPI one.
        #[test]
        fn mapped_name_over_64_characters_is_rejected() {
            let long_name = "a".repeat(65);
            let handle =
                MappingHandle::from_map(HashMap::from([("short-pkg".to_string(), long_name)]));
            let err = convert(&req("short-pkg"), false, assumption(), &handle).unwrap_err();
            assert!(matches!(err, ConvertError::NameTooLong { .. }), "{err:?}");
        }

        /// A mapping entry whose value doesn't have a valid conda
        /// package-name shape is rejected with a specific error rather
        /// than reaching `PackageName::new_unchecked` unchecked.
        #[test]
        fn mapped_name_with_an_invalid_shape_is_rejected() {
            let handle = MappingHandle::from_map(HashMap::from([(
                "some-pkg".to_string(),
                "not a valid name".to_string(),
            )]));
            let err = convert(&req("some-pkg"), false, assumption(), &handle).unwrap_err();
            assert!(matches!(err, ConvertError::InvalidMappedName(_)), "{err:?}");
        }
    }

    mod version {
        use super::*;

        #[test]
        fn operator_is_passed_through_as_is() {
            for operator in [">=", "<=", "!="] {
                let spec = format!("requests{operator}2.0.0");
                assert_eq!(
                    convert_ok(&req(&spec), false),
                    expect(&format!(r#"requests[version="{operator}2.0.0"]"#))
                );
            }
        }

        #[test]
        fn strict_less_than_gets_the_pre_release_carve_out_anchor() {
            assert_eq!(
                convert_ok(&req("requests<2.0.0"), false),
                expect(r#"requests[version="<2.0.0a0"]"#)
            );
        }

        #[test]
        fn multiple_specifiers_are_joined_in_canonical_order() {
            assert_eq!(
                convert_ok(&req("requests<=2.0.0,!=1.0.1,>=0.9"), false),
                expect(r#"requests[version=">=0.9,<=2.0.0,!=1.0.1"]"#)
            );
        }

        #[test]
        fn compatible_release_expands_to_a_range() {
            assert_eq!(
                convert_ok(&req("requests~=3.13.2"), false),
                expect(r#"requests[version=">=3.13.2,<3.14.0a0"]"#)
            );
        }

        #[test]
        fn rejects_a_local_version_label() {
            let err = convert_err(&req("requests==1.0.0+local"), false);
            assert!(
                matches!(err, ConvertError::LocalVersionLabel { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn rejects_a_pre_release_version_by_default() {
            let err = convert_err(&req("requests==1.0.0rc1"), false);
            assert!(matches!(err, ConvertError::Prerelease { .. }), "{err:?}");
        }

        #[test]
        fn allow_pre_permits_a_pre_release_version() {
            assert_eq!(
                convert_ok(&req("requests==1.0.0rc1"), true),
                expect(r#"requests[version="==1.0.0.rc1"]"#)
            );
        }
    }

    mod rejections {
        use super::*;

        #[test]
        fn rejects_a_direct_url_reference() {
            let err = convert_err(
                &req("requests @ https://example.com/requests-1.0.0.whl"),
                false,
            );
            assert!(matches!(err, ConvertError::DirectUrl), "{err:?}");
        }

        /// A local label combined with `<`/`>` never reaches this crate's
        /// own local-version-label check: `uv_pep508::Requirement::from_str`
        /// already rejects the combination (see
        /// [`crate::version::reject_unsupported_version`]'s docs).
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

        /// A marker referencing a *known* key resolves via `restrict()`,
        /// either to `Applicability::Always` (this test) or
        /// `Applicability::Never`. `sys_platform == "win32"` is false on
        /// `linux-64`, so the dependency doesn't apply -- not an error.
        #[test]
        fn virtual_package_marker_makes_the_dependency_never_apply() {
            let result = convert(
                &req(r#"requests>=2.0.0; sys_platform == "win32""#),
                false,
                assumption(),
                &no_mapping(),
            )
            .unwrap();
            assert_eq!(result, None);
        }

        #[test]
        fn extra_marker_is_rejected() {
            let err = convert_err(&req(r#"requests>=2.0.0; extra == "foo""#), false);
            assert!(matches!(err, ConvertError::Marker { .. }), "{err:?}");
        }

        #[test]
        fn marker_error_carries_the_marker_text() {
            let err = convert_err(&req(r#"requests; extra == "foo""#), false);
            match err {
                ConvertError::Marker { marker } => {
                    assert!(marker.contains("extra"), "{marker:?}");
                }
                other => panic!("expected ConvertError::Marker, got {other:?}"),
            }
        }

        /// [`convert`] checks for an `extra` clause before inspecting
        /// `requirement.version_or_url`, so a requirement with both a
        /// direct URL and an `extra` marker is rejected as
        /// [`ConvertError::Marker`], not [`ConvertError::DirectUrl`].
        #[test]
        fn marker_rejection_takes_precedence_over_a_direct_url() {
            let entry = r#"requests @ https://example.com/pkg.whl ; extra == "foo""#;
            let err = convert_err(&req(entry), false);
            assert!(matches!(err, ConvertError::Marker { .. }), "{err:?}");
        }

        /// Any marker containing an `extra` clause is rejected uniformly,
        /// regardless of shape (combined clauses, mixed with an
        /// environment condition, reversed operand order) -- see
        /// [`convert`]'s docs for why.
        #[test]
        fn every_extra_containing_marker_shape_is_rejected_uniformly() {
            for entry in [
                r#"requests>=2.0.0; extra == "foo" or extra == "bar""#,
                r#"requests>=2.0.0; extra == "foo" and python_version >= "3.8""#,
                r#"requests>=2.0.0; "foo" == extra"#,
            ] {
                let err = convert_err(&req(entry), false);
                assert!(
                    matches!(err, ConvertError::Marker { .. }),
                    "{entry}: {err:?}"
                );
            }
        }

        /// Each marker references a known key with a value that holds on
        /// `linux-64`, resolving via `restrict()` to
        /// `Applicability::Always`.
        #[test]
        fn known_key_markers_with_a_holding_value_resolve_to_always() {
            for entry in [
                r#"requests>=2.0.0; platform_machine == "x86_64""#,
                r#"requests>=2.0.0; os_name != "nt""#,
                r#"requests>=2.0.0; sys_platform >= "linux""#,
            ] {
                assert_eq!(
                    convert_ok(&req(entry), false),
                    expect("requests>=2.0.0"),
                    "{entry}"
                );
            }
        }

        /// `sys_platform == "cygwin"` is a known key whose value never
        /// holds on `linux-64` -- `Applicability::Never`, so the
        /// dependency is dropped (`Ok(None)`), not an error.
        #[test]
        fn known_key_marker_with_a_never_holding_value_resolves_to_never() {
            let result = convert(
                &req(r#"requests>=2.0.0; sys_platform == "cygwin""#),
                false,
                assumption(),
                &no_mapping(),
            )
            .unwrap();
            assert_eq!(result, None);
        }

        /// `~=` against a free-variable key (`python_version`/
        /// `python_full_version`) converts successfully: `uv_pep508`
        /// pre-expands `~=` into a plain range before this crate ever
        /// sees an operator.
        #[test]
        fn tilde_equal_on_a_free_variable_key_now_converts_successfully() {
            for entry in [
                r#"requests>=2.0.0; python_version ~= "3.9""#,
                r#"requests>=2.0.0; python_full_version ~= "3.9.0""#,
            ] {
                let result = convert(&req(entry), false, assumption(), &no_mapping()).unwrap();
                assert!(
                    matches!(
                        result,
                        Some(MatchSpec {
                            condition: Some(_),
                            ..
                        })
                    ),
                    "{entry}: {result:?}"
                );
            }
        }

        /// A reversed-operand `in`/`not in` marker (literal on the left:
        /// `"foo" in extra`) is not preserved as a real constraint by
        /// `uv_pep508`: `Requirement::from_str` parses it successfully and
        /// `marker.is_true()` is `true`, so [`convert`] silently treats
        /// the dependency as marker-free rather than rejecting it. Pinned
        /// here as the current behavior, so a future `uv_pep508` upgrade
        /// that starts parsing these shapes into a real `MarkerTree`
        /// turns into a loud test failure instead of a silent change.
        #[test]
        fn reversed_in_marker_is_silently_accepted_not_rejected() {
            let entry = r#"requests>=2.0.0; "foo" in extra"#;
            assert!(
                convert(&req(entry), false, assumption(), &no_mapping()).is_ok(),
                "if this now fails, uv_pep508 has started parsing reversed `in` as a real \
                 constraint -- update this test and consider whether ConvertError::Marker \
                 should fire instead"
            );
        }

        #[test]
        fn reversed_not_in_marker_is_silently_accepted_not_rejected() {
            let entry = r#"requests>=2.0.0; "3.9" not in python_version"#;
            assert!(
                convert(&req(entry), false, assumption(), &no_mapping()).is_ok(),
                "if this now fails, uv_pep508 has started parsing reversed `not in` as a real \
                 constraint -- update this test and consider whether ConvertError::Marker \
                 should fire instead"
            );
        }

        /// A reversed-operand compatible-release comparison against a
        /// string marker field (`"posix" ~= os_name`) is silently
        /// accepted, not rejected, same as the reversed `in`/`not in`
        /// shapes above: `uv_pep508` treats it as marker-free. Pinned here
        /// so a future `uv_pep508` upgrade that starts treating this as a
        /// real constraint (or panics) is a loud test failure.
        #[test]
        fn reversed_compatible_release_string_marker_is_silently_accepted_not_rejected() {
            let entry = r#"requests>=2.0.0; "posix" ~= os_name"#;
            assert!(
                convert(&req(entry), false, assumption(), &no_mapping()).is_ok(),
                "if this now fails, uv_pep508 has started treating a reversed string `~=` \
                 marker as a real constraint (or panics again) -- update this test and \
                 consider whether ConvertError::Marker should fire instead"
            );
        }

        /// A marker key with no matchspec equivalent even once known
        /// values are restricted away surfaces as
        /// [`ConvertError::UnconvertibleMarker`], not
        /// [`ConvertError::Marker`].
        #[test]
        fn a_key_with_no_matchspec_equivalent_is_unconvertible_not_a_marker_error() {
            let err = convert_err(
                &req(r#"requests>=2.0.0; platform_release == "5.10.0""#),
                false,
            );
            assert!(
                matches!(err, ConvertError::UnconvertibleMarker(_)),
                "{err:?}"
            );
        }

        /// `sys_platform == "linux"` resolves away (true on `linux-64`),
        /// leaving just the `python_version` residual as the matchspec
        /// `condition`.
        #[test]
        fn a_known_and_free_marker_produces_a_condition() {
            let matchspec = convert_ok(
                &req(r#"requests>=2.0.0; sys_platform == "linux" and python_version >= "3.9""#),
                false,
            );
            assert_eq!(matchspec.name, expect("requests").name);
            assert!(matchspec.condition.is_some(), "{matchspec:?}");
        }
    }

    /// Marker *conversion* tests: unlike `sys_platform`/`os_name`/
    /// `platform_system`/`platform_machine`, which are always fully
    /// resolved by `restrict()` before a matchspec `condition` is ever
    /// built (see the `markers` module above), `python_version`/
    /// `python_full_version` leaves remain as real `condition` residuals
    /// for these tests to check.
    mod markers_deferred {
        use rattler_conda_types::{ParseStrictness, Version, VersionSpec};

        use super::*;

        /// A leaf `MatchSpecCondition` for `python<version_spec>`.
        fn python(version_spec: &str) -> MatchSpecCondition {
            MatchSpecCondition::MatchSpec(Box::new(MatchSpec {
                name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
                version: Some(
                    VersionSpec::from_str(version_spec, ParseStrictness::Lenient).unwrap(),
                ),
                ..MatchSpec::default()
            }))
        }

        /// The boundary is anchored at `.0a0` so a pre-release build of
        /// the boundary minor (`python==3.9.0a0`) is correctly included.
        #[test]
        fn python_version_marker_converts_per_the_table() {
            let matchspec = convert_ok(&req(r#"requests; python_version >= "3.9""#), false);
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// matchspec's fuzzy-equals syntax is deprecated, so this crate
        /// never emits it for a `python_version` equality boundary.
        #[test]
        fn python_version_equality_marker_produces_a_rattler_valid_when_clause() {
            let matchspec = convert_ok(&req(r#"requests; python_version == "3.9""#), false);
            assert_eq!(
                matchspec.condition,
                Some(MatchSpecCondition::And(
                    Box::new(python(">=3.9.0a0")),
                    Box::new(python("<3.10.0a0")),
                ))
            );
        }

        #[test]
        fn python_version_inequality_marker_produces_a_rattler_valid_when_clause() {
            let matchspec = convert_ok(&req(r#"requests; python_version != "3.9""#), false);
            assert_eq!(matchspec.condition, Some(python("!=3.9.*")));
        }

        /// Uses `win-64`, not this module's usual `linux-64`:
        /// `sys_platform == "win32"` would resolve away on `linux-64`,
        /// dropping one side of the `and` instead of leaving it in the
        /// residual.
        #[test]
        fn combined_marker_preserves_and_or_structure() {
            let win_assumption =
                ana_marker_matchspec::known_values_assumption(rattler_conda_types::Platform::Win64)
                    .unwrap();
            let matchspec = convert(
                &req(r#"requests; sys_platform == "win32" and python_version >= "3.9""#),
                false,
                win_assumption,
                &no_mapping(),
            )
            .unwrap()
            .expect("sys_platform == \"win32\" holds on win-64");
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        #[test]
        fn reversed_comparison_operand_order_still_converts() {
            let matchspec = convert_ok(&req(r#"requests; "3.9" <= python_version"#), false);
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// With no virtual-package `condition` in this design, the
        /// reversed operand order is exercised through `restrict()`'s own
        /// resolution: `"win32" == sys_platform` still resolves to
        /// `Applicability::Never` on `linux-64` regardless of which side
        /// of `==` the literal is on.
        #[test]
        fn reversed_virtual_package_operand_order_still_converts() {
            let result = convert(
                &req(r#"requests; "win32" == sys_platform"#),
                false,
                assumption(),
                &no_mapping(),
            )
            .unwrap();
            assert_eq!(result, None);
        }

        /// `uv_pep508` drops the `rc1` pre-release segment during marker
        /// parsing, and the resulting 2-segment literal is then
        /// indistinguishable from a `python_version` boundary, so it gets
        /// that boundary's `.0a0` anchor too.
        #[test]
        fn prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre() {
            let matchspec = convert_ok(
                &req(r#"requests; python_full_version >= "3.9.0rc1""#),
                false,
            );
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// The package-version part (`numpy<1.25.0,>=1.24.0`) and the
        /// marker part convert independently and combine in the same
        /// `MatchSpec`. The glob literal collapses to a 2-segment
        /// `python_full_version` boundary, indistinguishable here from a
        /// `python_version` boundary at the same precision, so it gets
        /// the anchored two-clause range.
        #[test]
        fn full_version_glob_marker_produces_a_rattler_valid_when_clause() {
            let matchspec = convert_ok(
                &req(r#"numpy<1.25.0,>=1.24.0; python_full_version == "3.8.*""#),
                false,
            );
            assert_eq!(matchspec.version, expect("numpy>=1.24.0,<1.25.0a0").version);
            assert_eq!(
                matchspec.condition,
                Some(MatchSpecCondition::And(
                    Box::new(python(">=3.8.0a0")),
                    Box::new(python("<3.9.0a0")),
                ))
            );
        }

        /// Uses a free-variable marker rather than `sys_platform ==
        /// "win32"` (which would resolve away to `Applicability::Never`
        /// on `linux-64`) to keep both extras and a real `condition`
        /// present together.
        #[test]
        fn extras_and_marker_combine_in_one_bracket() {
            let matchspec =
                convert_ok(&req(r#"fastapi[all]>=1.0; python_version >= "3.9""#), false);
            assert_eq!(matchspec.extras, expect("fastapi[extras=[all]]").extras);
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// A bare major literal normalizes to minor `0`, so the upper
        /// bound is the next minor (`3.1`), not the next major.
        #[test]
        fn python_version_literal_with_only_a_major_segment_converts() {
            let matchspec = convert_ok(&req(r#"requests>=2.0.0; python_version == "3""#), false);
            assert_eq!(
                matchspec.condition,
                Some(MatchSpecCondition::And(
                    Box::new(python(">=3.0.0a0")),
                    Box::new(python("<3.1.0a0")),
                ))
            );
        }

        /// `uv_pep508` expands the literal's bounds directly, with no
        /// network fetch needed. Both boundaries get the `.0a0` anchor.
        #[test]
        fn in_marker_converts_via_the_membership_rewrite() {
            let matchspec = convert_ok(
                &req(r#"requests>=2.0.0; python_version in "3.9 3.10""#),
                false,
            );
            assert_eq!(
                matchspec.condition,
                Some(MatchSpecCondition::And(
                    Box::new(python(">=3.9.0a0")),
                    Box::new(python("<3.11.0a0")),
                ))
            );
        }

        /// Both boundaries get the `.0a0` anchor.
        #[test]
        fn not_in_marker_converts_via_the_membership_rewrite() {
            let matchspec = convert_ok(
                &req(r#"requests>=2.0.0; python_version not in "3.9 3.10""#),
                false,
            );
            assert_eq!(
                matchspec.condition,
                Some(MatchSpecCondition::Or(
                    Box::new(python("<3.9.0a0")),
                    Box::new(python(">=3.11.0a0")),
                ))
            );
        }

        /// Smoke test confirming this crate's own public `convert()`
        /// surfaces the same oracle-verified behavior
        /// `ana-marker-matchspec` checks exhaustively, including the
        /// `.0a0` pre-release anchor: a pre-release build of the boundary
        /// minor must satisfy `python_version >= "3.9"`, and a version
        /// below the boundary minor entirely must not.
        #[test]
        fn marker_equivalence_oracle_suite() {
            let matchspec = convert_ok(&req(r#"requests; python_version >= "3.9""#), false);
            let condition = matchspec
                .condition
                .expect(r#"python_version >= "3.9" is conditional, not Always/Never"#);
            let MatchSpecCondition::MatchSpec(spec) = &condition else {
                panic!("expected a single leaf condition, got {condition:?}");
            };
            let version_spec = spec
                .version
                .as_ref()
                .expect("the python leaf always carries a version");
            assert!(
                version_spec.matches(&Version::from_str("3.9.0a0").unwrap()),
                "a pre-release build of the boundary minor must still satisfy >=3.9: {version_spec}"
            );
            assert!(
                !version_spec.matches(&Version::from_str("3.8.9").unwrap()),
                "a version below the boundary minor entirely must not satisfy >=3.9: {version_spec}"
            );
        }
    }

    mod extras {
        use super::*;

        #[test]
        fn bare_extra_becomes_an_extras_bracket() {
            assert_eq!(
                convert_ok(&req("fastapi[all]"), false),
                expect("fastapi[extras=[all]]")
            );
        }

        #[test]
        fn extras_come_after_the_version() {
            assert_eq!(
                convert_ok(&req("fastapi[all]>=1.0"), false),
                expect(r#"fastapi[version=">=1.0",extras=[all]]"#)
            );
        }

        #[test]
        fn multiple_extras_are_normalized_and_sorted() {
            assert_eq!(
                convert_ok(&req("fastapi[Standard,ALL]"), false),
                expect("fastapi[extras=[all,standard]]")
            );
        }

        #[test]
        fn extra_name_is_normalized() {
            assert_eq!(
                convert_ok(&req("fastapi[Some_Extra.Name]"), false),
                expect("fastapi[extras=[some-extra-name]]")
            );
        }

        #[test]
        fn extra_name_over_64_characters_is_rejected() {
            let entry = format!("fastapi[{}]", "a".repeat(65));
            let err = convert_err(&req(&entry), false);
            assert!(matches!(err, ConvertError::ExtraTooLong { .. }), "{err:?}");
        }

        #[test]
        fn extra_name_at_exactly_64_characters_is_accepted() {
            let extra = "a".repeat(64);
            let entry = format!("fastapi[{extra}]");
            assert_eq!(
                convert_ok(&req(&entry), false),
                expect(&format!("fastapi[extras=[{extra}]]"))
            );
        }

        #[test]
        fn empty_extras_brackets_produce_no_extras_clause() {
            assert_eq!(convert_ok(&req("fastapi[]"), false), expect("fastapi"));
        }

        #[test]
        fn duplicate_extras_after_normalization_are_deduplicated() {
            assert_eq!(
                convert_ok(&req("fastapi[Foo-Bar,foo_bar]"), false),
                expect("fastapi[extras=[foo-bar]]")
            );
        }

        #[test]
        fn any_invalid_extra_length_raises_even_when_others_are_valid() {
            let entry = format!("fastapi[valid,{}]", "a".repeat(65));
            let err = convert_err(&req(&entry), false);
            assert!(matches!(err, ConvertError::ExtraTooLong { .. }), "{err:?}");
        }
    }

    mod integration {
        use super::*;

        #[test]
        fn name_version_and_extras_all_combine() {
            let entry = "Foo_Bar.BAZ[Extra1,extra_2]~=1.2.3rc1";
            assert_eq!(
                convert_ok(&req(entry), true),
                expect(r#"foo-bar-baz[version=">=1.2.3.rc1,<1.3.0a0",extras=[extra-2,extra1]]"#)
            );
        }
    }

    mod convert_all_batch {
        use super::*;

        #[test]
        fn is_index_aligned_with_its_input() {
            let requirements = vec![req("requests"), req("requests @ https://example.com/x.whl")];
            let results = convert_all(&requirements, false, assumption(), &no_mapping());

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

            let results = convert_all(&requirements, false, assumption(), &no_mapping());

            assert_eq!(results.len(), PARALLEL_CONVERT_THRESHOLD + 1);
            assert!(results[..PARALLEL_CONVERT_THRESHOLD]
                .iter()
                .all(Result::is_ok));
            assert!(matches!(
                results[PARALLEL_CONVERT_THRESHOLD],
                Err(ConvertError::DirectUrl)
            ));
        }

        /// `pypi_to_conda_map` reaches every element on both the
        /// sequential and `rayon` paths -- exercised at the
        /// [`convert_all`] level so a future change forwarding it to only
        /// one dispatch branch would show up here.
        #[test]
        fn mapping_is_forwarded_on_both_the_sequential_and_parallel_paths() {
            let handle = MappingHandle::from_map(HashMap::from([(
                "opencv-python".to_string(),
                "py-opencv".to_string(),
            )]));

            let sequential = vec![req("opencv-python")];
            let sequential_results = convert_all(&sequential, false, assumption(), &handle);
            assert_eq!(
                sequential_results[0]
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .name,
                PackageNameMatcher::Exact(PackageName::new_unchecked("py-opencv"))
            );

            let mut parallel: Vec<Requirement> = (0..PARALLEL_CONVERT_THRESHOLD)
                .map(|i| req(&format!("requests{i}")))
                .collect();
            parallel.push(req("opencv-python"));
            let parallel_results = convert_all(&parallel, false, assumption(), &handle);
            let last = parallel_results
                .last()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap();
            assert_eq!(
                last.name,
                PackageNameMatcher::Exact(PackageName::new_unchecked("py-opencv"))
            );
        }
    }
}
