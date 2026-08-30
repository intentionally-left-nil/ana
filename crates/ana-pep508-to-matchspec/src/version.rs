//! PEP 440 version specifier(s) -> conda `VersionSpec`.
//!
//! Builds a typed [`VersionSpec`] directly rather than formatting a
//! matchspec version clause string and reparsing it. The one unavoidable
//! string round-trip is an individual version literal: `rattler_conda_types`
//! has no general typed `Version` constructor, so [`conda_version`]/
//! [`parse_conda_version`] go through [`ana_marker_matchspec::format_version`]
//! (shared with `ana-marker-matchspec`) and `Version::from_str`.

use std::fmt::Write as _;

use rattler_conda_types::{
    EqualityOperator, LogicalOperator, ParseVersionError, RangeOperator, StrictRangeOperator,
    StrictVersion, Version as CondaVersion, VersionSpec,
};
use uv_pep440::{Operator, Version as PypiVersion, VersionSpecifier, VersionSpecifiers};

use crate::ConvertError;

/// The conda `VersionSpec` for every specifier in `specifiers`, joined as an
/// implicit AND, or `None` if `specifiers` is empty.
///
/// `allow_pre` governs whether a pre-release specifier version is accepted;
/// see [`reject_unsupported_version`].
///
/// Specifiers are visited in a canonical order (lower bounds, then upper
/// bounds, then pins, then exclusions; ties broken by the specifier's own
/// string spelling) purely for deterministic output; this has no effect on
/// which versions match.
///
/// Public: also used by `ana_lockfile`'s conversion pipeline to build the
/// `python` matchspec version constraint from `requires-python`.
pub fn version_spec(
    specifiers: &VersionSpecifiers,
    allow_pre: bool,
) -> Result<Option<VersionSpec>, ConvertError> {
    if specifiers.is_empty() {
        return Ok(None);
    }

    let mut ordered: Vec<&VersionSpecifier> = specifiers.iter().collect();
    ordered.sort_by(cmp_specifiers);

    let mut leaves = Vec::with_capacity(ordered.len());
    for specifier in ordered {
        leaves.extend(convert_specifier(specifier, allow_pre)?);
    }

    Ok(Some(match leaves.len() {
        1 => match leaves.into_iter().next() {
            Some(leaf) => leaf,
            None => unreachable!("a length-1 Vec always yields one element"),
        },
        _ => VersionSpec::Group(LogicalOperator::And, leaves),
    }))
}

/// Sort order: lower bounds, then upper bounds, then pins, then exclusions
/// ([`operator_rank`]); ties broken by the specifier's own PEP 440 text
/// (not our own CEP-33 spelling), so `>=9.0` sorts before `>=10.0`
/// lexicographically.
fn cmp_specifiers(a: &&VersionSpecifier, b: &&VersionSpecifier) -> std::cmp::Ordering {
    operator_rank(*a.operator())
        .cmp(&operator_rank(*b.operator()))
        .then_with(|| a.to_string().cmp(&b.to_string()))
}

/// Lower bounds, then upper bounds, then pins, then exclusions. Exhaustive
/// match with no wildcard arm, so a new [`Operator`] variant is a compile
/// error here rather than a silent gap.
fn operator_rank(operator: Operator) -> u8 {
    match operator {
        Operator::GreaterThanEqual | Operator::GreaterThan => 0,
        Operator::LessThanEqual | Operator::LessThan => 1,
        Operator::Equal | Operator::EqualStar | Operator::ExactEqual | Operator::TildeEqual => 2,
        Operator::NotEqual | Operator::NotEqualStar => 3,
    }
}

/// One [`VersionSpecifier`]'s contribution to a `VersionSpec` -- one leaf
/// for every operator except `~=` (two, for the expanded range) and `>`
/// without a `.post`/dev suffix already spelled out (two, for the
/// exclusion).
///
/// Exhaustive over [`Operator`], no wildcard arm -- see [`operator_rank`].
fn convert_specifier(
    specifier: &VersionSpecifier,
    allow_pre: bool,
) -> Result<Vec<VersionSpec>, ConvertError> {
    let version = specifier.version();
    match specifier.operator() {
        Operator::GreaterThanEqual => {
            reject_unsupported_version(specifier, allow_pre)?;
            Ok(vec![VersionSpec::Range(
                RangeOperator::GreaterEquals,
                conda_version(version)?,
            )])
        }
        Operator::LessThanEqual => {
            reject_unsupported_version(specifier, allow_pre)?;
            Ok(vec![VersionSpec::Range(
                RangeOperator::LessEquals,
                conda_version(version)?,
            )])
        }
        Operator::GreaterThan => convert_exclusive_greater_than(specifier, allow_pre),
        Operator::LessThan => convert_exclusive_less_than(specifier, allow_pre),
        Operator::Equal | Operator::ExactEqual => {
            // `===`'s value is treated the same as `==`'s. Unlike Python's
            // `packaging`, `uv_pep440` requires `===`'s right-hand side to
            // itself parse as a PEP 440 version, so the arbitrary
            // non-version string PEP 440 permits for `===` never reaches
            // this function.
            reject_unsupported_version(specifier, allow_pre)?;
            Ok(vec![VersionSpec::Exact(
                EqualityOperator::Equals,
                conda_version(version)?,
            )])
        }
        Operator::NotEqual => {
            reject_unsupported_version(specifier, allow_pre)?;
            Ok(vec![VersionSpec::Exact(
                EqualityOperator::NotEquals,
                conda_version(version)?,
            )])
        }
        Operator::EqualStar => {
            // The wildcard grammar never pairs a pre/post/dev/local suffix
            // with `.*`, so this can never actually reject anything --
            // called anyway so a future grammar relaxation fails safe.
            reject_unsupported_version(specifier, allow_pre)?;
            Ok(vec![VersionSpec::StrictRange(
                StrictRangeOperator::StartsWith,
                StrictVersion::from(conda_version(version)?),
            )])
        }
        Operator::NotEqualStar => {
            reject_unsupported_version(specifier, allow_pre)?;
            Ok(vec![VersionSpec::StrictRange(
                StrictRangeOperator::NotStartsWith,
                StrictVersion::from(conda_version(version)?),
            )])
        }
        Operator::TildeEqual => expand_compatible_release(specifier, allow_pre),
    }
}

/// `<V`'s PEP 440 exclusive-comparison carve-out: excludes every
/// pre-release of `V` unless `V` is itself a pre-release. Conda's plain
/// `Range(Less, _)` has no such exception, so a non-pre-release `V` gets an
/// explicit anchor: `a0` glued directly onto `V`'s spelling with no
/// separating dot, which sorts below every pre-release of `V`.
fn convert_exclusive_less_than(
    specifier: &VersionSpecifier,
    allow_pre: bool,
) -> Result<Vec<VersionSpec>, ConvertError> {
    reject_unsupported_version(specifier, allow_pre)?;
    let version = specifier.version();
    let formatted = if version.any_prerelease() {
        format_version(version)
    } else {
        format!("{}a0", format_version(version))
    };
    Ok(vec![VersionSpec::Range(
        RangeOperator::Less,
        parse_conda_version(&formatted)?,
    )])
}

/// `>V`'s PEP 440 exclusive-comparison carve-out: excludes every
/// post-release of `V` unless `V` is itself a post- or dev-release. Conda's
/// plain `Range(Greater, _)` has no such exception, so the exclusion needs
/// its own clause: `Range(Greater, V)` plus a glob-style "not equal to any
/// post-release of `V`".
fn convert_exclusive_greater_than(
    specifier: &VersionSpecifier,
    allow_pre: bool,
) -> Result<Vec<VersionSpec>, ConvertError> {
    reject_unsupported_version(specifier, allow_pre)?;
    let version = specifier.version();
    let lower = VersionSpec::Range(RangeOperator::Greater, conda_version(version)?);
    if version.dev().is_some() || version.post().is_some() {
        return Ok(vec![lower]);
    }
    let post_glob = format!("{}.post", format_version(version));
    Ok(vec![
        lower,
        VersionSpec::StrictRange(
            StrictRangeOperator::NotStartsWith,
            StrictVersion::from(parse_conda_version(&post_glob)?),
        ),
    ])
}

/// `~=X.Y.Z`'s expansion into `>=X.Y.Z,<X.(Y+1).0a0`. Conda's native `~=`
/// (`StrictRangeOperator::Compatible`) is deprecated by CEP 29, so this
/// always expands into an explicit `>=`/`<` pair instead.
fn expand_compatible_release(
    specifier: &VersionSpecifier,
    allow_pre: bool,
) -> Result<Vec<VersionSpec>, ConvertError> {
    reject_unsupported_version(specifier, allow_pre)?;
    let version = specifier.version();
    let release = version.release();
    debug_assert!(
        release.len() >= 2,
        "uv_pep440 guarantees >= 2 release segments for ~="
    );
    let prefix_len = release.len().saturating_sub(1);
    let mut bumped: Vec<u64> = release[..prefix_len].to_vec();
    if let Some(last) = bumped.last_mut() {
        *last = last.saturating_add(1);
    }

    let lower = conda_version(version)?;
    let epoch = version.epoch();
    let epoch_prefix = if epoch != 0 {
        format!("{epoch}!")
    } else {
        String::new()
    };
    let mut upper_release = String::new();
    for (index, segment) in bumped.iter().enumerate() {
        if index > 0 {
            upper_release.push('.');
        }
        let _ = write!(upper_release, "{segment}");
    }
    let upper = parse_conda_version(&format!("{epoch_prefix}{upper_release}.0a0"))?;

    Ok(vec![
        VersionSpec::Range(RangeOperator::GreaterEquals, lower),
        VersionSpec::Range(RangeOperator::Less, upper),
    ])
}

/// Rejects `specifier`'s version if it has a local version label (no
/// matchspec equivalent, ever) or is a pre-release without `allow_pre`.
/// Local labels are checked first, unconditionally, even when `allow_pre`
/// would otherwise permit a pre-release boundary.
///
/// The local-label branch is only reachable in practice for
/// `Equal`/`NotEqual`/`ExactEqual`: `VersionSpecifier::from_str` already
/// rejects a local segment paired with every other operator here.
fn reject_unsupported_version(
    specifier: &VersionSpecifier,
    allow_pre: bool,
) -> Result<(), ConvertError> {
    let version = specifier.version();
    if version.is_local() {
        return Err(ConvertError::LocalVersionLabel {
            specifier: specifier.to_string(),
        });
    }
    if version.any_prerelease() && !allow_pre {
        return Err(ConvertError::Prerelease {
            specifier: specifier.to_string(),
        });
    }
    Ok(())
}

/// `version`'s CEP-33 spelling: epoch (if any) prefixed with `!`, release
/// segments dot-joined, then `.{letter}{number}` for a pre-release,
/// `.post{number}` for a post-release, and `.dev{number}` for a dev
/// release -- e.g. `1.0.0rc1` becomes `1.0.0.rc1`.
///
/// Never emits a local segment -- callers reject one first, via
/// [`reject_unsupported_version`].
fn format_version(version: &PypiVersion) -> String {
    ana_marker_matchspec::format_version(version)
}

/// [`format_version`] then [`CondaVersion::from_str`] in one step -- the
/// one string round-trip this module can't avoid; see the module docs.
fn conda_version(version: &PypiVersion) -> Result<CondaVersion, ConvertError> {
    parse_conda_version(&format_version(version))
}

/// `CondaVersion::from_str(literal)`, wrapping a parse failure as a
/// [`ConvertError`]. In practice `literal` is always built by this module
/// from an already-validated [`PypiVersion`], so this should never
/// actually fail, but the error is propagated rather than unwrapped.
fn parse_conda_version(literal: &str) -> Result<CondaVersion, ConvertError> {
    literal.parse().map_err(
        |source: ParseVersionError| ConvertError::InvalidVersionLiteral {
            literal: literal.to_string(),
            source,
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::str::FromStr;

    use rattler_conda_types::{ParseStrictness, VersionSpec};
    use uv_pep440::VersionSpecifiers;

    use super::*;

    /// Parses `specifiers` as PEP 440 (e.g. `">=1.0.0,<2.0.0"`) and returns
    /// the `VersionSpec` [`version_spec`] converts it to.
    fn convert(specifiers: &str, allow_pre: bool) -> Result<Option<VersionSpec>, ConvertError> {
        let specifiers = VersionSpecifiers::from_str(specifiers).unwrap();
        version_spec(&specifiers, allow_pre)
    }

    /// `expected` parsed as a conda version-spec bracket value, for
    /// comparing against [`convert`]'s output without hand-building a
    /// `VersionSpec` AST per test.
    fn expect(expected: &str) -> VersionSpec {
        VersionSpec::from_str(expected, ParseStrictness::Lenient).unwrap()
    }

    mod basics {
        use super::*;

        #[test]
        fn empty_specifier_set_has_no_version_spec() {
            assert_eq!(convert("", false).unwrap(), None);
        }

        #[test]
        fn operators_pass_through_as_is() {
            for operator in [">=", "<=", "!=", "=="] {
                let specifiers = format!("{operator}2.0.0");
                assert_eq!(
                    convert(&specifiers, false).unwrap(),
                    Some(expect(&specifiers)),
                    "operator {operator}"
                );
            }
        }

        #[test]
        fn multiple_specifiers_join_in_canonical_order() {
            assert_eq!(
                convert(">=1.0.0,<2.0.0", false).unwrap(),
                Some(expect(">=1.0.0,<2.0.0a0"))
            );
        }

        #[test]
        fn canonical_order_spans_every_operator_category() {
            assert_eq!(
                convert("!=5.0.0,==3.0.0,<4.0.0,>=1.0.0", false).unwrap(),
                Some(expect(">=1.0.0,<4.0.0a0,==3.0.0,!=5.0.0"))
            );
        }

        #[test]
        fn same_operator_ties_sort_lexicographically() {
            assert_eq!(
                convert(">=9.0,>=10.0", false).unwrap(),
                Some(expect(">=10.0,>=9.0"))
            );
        }

        /// The `!=` exclusion-category counterpart to
        /// `same_operator_ties_sort_lexicographically`: these two already
        /// sort lexicographically in input order, pinning that the
        /// tiebreak doesn't accidentally reorder an already-sorted pair.
        #[test]
        fn same_operator_ties_sort_lexicographically_for_exclusions() {
            assert_eq!(
                convert("!=1.0.0,!=2.0.0", false).unwrap(),
                Some(expect("!=1.0.0,!=2.0.0"))
            );
        }
    }

    mod exclusive_comparators {
        use super::*;

        #[test]
        fn strict_less_than_gets_the_pre_release_carve_out_anchor() {
            assert_eq!(convert("<2.0.0", false).unwrap(), Some(expect("<2.0.0a0")));
        }

        #[test]
        fn strict_less_than_carve_out_anchor_with_a_missing_patch_segment() {
            assert_eq!(convert("<2.0", false).unwrap(), Some(expect("<2.0a0")));
        }

        #[test]
        fn strict_less_than_of_a_pre_release_has_no_anchor() {
            assert_eq!(
                convert("<2.0.0rc1", true).unwrap(),
                Some(expect("<2.0.0.rc1"))
            );
        }

        #[test]
        fn strict_greater_than_gets_the_post_release_carve_out_exclusion() {
            assert_eq!(
                convert(">1.0.0", false).unwrap(),
                Some(expect(">1.0.0,!=1.0.0.post*"))
            );
        }

        #[test]
        fn strict_greater_than_of_a_post_release_has_no_exclusion() {
            assert_eq!(
                convert(">1.0.0.post1", false).unwrap(),
                Some(expect(">1.0.0.post1"))
            );
        }

        #[test]
        fn strict_greater_than_of_a_dev_release_has_no_exclusion() {
            assert_eq!(
                convert(">1.0.0.dev1", true).unwrap(),
                Some(expect(">1.0.0.dev1"))
            );
        }

        #[test]
        fn strict_comparators_reject_a_pre_release_by_default() {
            for operator in ["<", ">"] {
                let specifiers = format!("{operator}2.0.0rc1");
                let err = convert(&specifiers, false).unwrap_err();
                assert!(matches!(err, ConvertError::Prerelease { .. }), "{err:?}");
            }
        }
    }

    mod globs {
        use super::*;

        #[test]
        fn equals_glob_is_rewritten_to_the_canonical_fuzzy_form() {
            assert_eq!(convert("==1.0.*", false).unwrap(), Some(expect("=1.0")));
        }

        #[test]
        fn not_equals_glob_passes_through_unchanged() {
            assert_eq!(convert("!=1.0.*", false).unwrap(), Some(expect("!=1.0.*")));
        }

        #[test]
        fn equals_glob_with_an_epoch_is_rewritten_to_the_canonical_fuzzy_form() {
            assert_eq!(convert("==1!2.0.*", false).unwrap(), Some(expect("=1!2.0")));
        }
    }

    mod compatible_release {
        use super::*;

        #[test]
        fn expands_to_a_range() {
            assert_eq!(
                convert("~=1.4.2", false).unwrap(),
                Some(expect(">=1.4.2,<1.5.0a0"))
            );
        }

        #[test]
        fn two_segments_drops_the_major() {
            assert_eq!(
                convert("~=1.4", false).unwrap(),
                Some(expect(">=1.4,<2.0a0"))
            );
        }

        #[test]
        fn four_segments_only_bumps_the_last() {
            assert_eq!(
                convert("~=1.2.3.4", false).unwrap(),
                Some(expect(">=1.2.3.4,<1.2.4.0a0"))
            );
        }

        #[test]
        fn preserves_the_epoch_in_both_bounds() {
            assert_eq!(
                convert("~=1!3.13.2", false).unwrap(),
                Some(expect(">=1!3.13.2,<1!3.14.0a0"))
            );
        }

        #[test]
        fn combines_with_another_specifier_in_canonical_order() {
            assert_eq!(
                convert("~=3.13.2,!=3.13.5", false).unwrap(),
                Some(expect(">=3.13.2,<3.14.0a0,!=3.13.5"))
            );
        }

        #[test]
        fn rejects_a_pre_release_by_default() {
            let err = convert("~=3.13.2rc1", false).unwrap_err();
            assert!(matches!(err, ConvertError::Prerelease { .. }), "{err:?}");
        }

        #[test]
        fn allow_pre_permits_a_pre_release() {
            assert_eq!(
                convert("~=3.13.2rc1", true).unwrap(),
                Some(expect(">=3.13.2.rc1,<3.14.0a0"))
            );
        }
    }

    mod arbitrary_equality {
        use super::*;

        #[test]
        fn is_converted_to_double_equals() {
            assert_eq!(convert("===1.0.0", false).unwrap(), Some(expect("==1.0.0")));
        }
    }

    mod version_formatting {
        use super::*;

        #[test]
        fn epoch_is_preserved() {
            assert_eq!(
                convert(">=1!1.0.0", false).unwrap(),
                Some(expect(">=1!1.0.0"))
            );
        }

        #[test]
        fn post_release_is_accepted() {
            assert_eq!(
                convert(">=1.0.0.post1", false).unwrap(),
                Some(expect(">=1.0.0.post1"))
            );
        }

        #[test]
        fn post_release_shorthand_is_normalized() {
            assert_eq!(
                convert(">=1.0-1", false).unwrap(),
                Some(expect(">=1.0.post1"))
            );
        }

        #[test]
        fn pre_release_is_dotted_when_allowed() {
            assert_eq!(
                convert("==1.0.0rc1", true).unwrap(),
                Some(expect("==1.0.0.rc1"))
            );
        }

        #[test]
        fn many_release_segments_are_all_preserved() {
            assert_eq!(
                convert(">=1.2.3.4", false).unwrap(),
                Some(expect(">=1.2.3.4"))
            );
        }

        #[test]
        fn pre_post_and_dev_releases_all_combine_in_order() {
            assert_eq!(
                convert("==1.0.0a1.post2.dev3", true).unwrap(),
                Some(expect("==1.0.0.a1.post2.dev3"))
            );
        }

        #[test]
        fn v_prefix_is_normalized_away() {
            assert_eq!(convert(">=v1.0", false).unwrap(), Some(expect(">=1.0")));
        }
    }

    mod rejections {
        use super::*;

        #[test]
        fn rejects_a_local_version_label() {
            for specifiers in ["==1.0.0+local", "!=1.0.0+local", "===1.0.0+local"] {
                let err = convert(specifiers, false).unwrap_err();
                assert!(
                    matches!(err, ConvertError::LocalVersionLabel { .. }),
                    "{specifiers}: {err:?}"
                );
            }
        }

        #[test]
        fn rejects_a_pre_release_version_by_default() {
            for specifiers in ["==1.0.0dev1", "==1.0.0a1", "==1.0.0b1", "==1.0.0rc1"] {
                let err = convert(specifiers, false).unwrap_err();
                assert!(
                    matches!(err, ConvertError::Prerelease { .. }),
                    "{specifiers}: {err:?}"
                );
            }
        }

        #[test]
        fn dev_release_combined_with_a_post_release_is_still_a_pre_release() {
            let err = convert("==1.0.0.post1.dev1", false).unwrap_err();
            assert!(matches!(err, ConvertError::Prerelease { .. }), "{err:?}");
        }

        #[test]
        fn local_version_label_is_rejected_before_pre_release_even_with_allow_pre() {
            let err = convert("==1.0.0rc1+local", true).unwrap_err();
            assert!(
                matches!(err, ConvertError::LocalVersionLabel { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn allow_pre_still_rejects_a_local_version_label() {
            let err = convert("==1.0.0+local", true).unwrap_err();
            assert!(
                matches!(err, ConvertError::LocalVersionLabel { .. }),
                "{err:?}"
            );
        }
    }

    /// Equivalence oracle: checks the [`VersionSpec`] [`version_spec`]
    /// produces for a bare PEP 440 specifier against `uv_pep440`'s own
    /// `VersionSpecifiers::contains` for a sweep of candidate versions, so
    /// a passing test proves semantic equivalence for every candidate
    /// rather than agreement with one hand-picked expected string.
    mod equivalence_oracle {
        use super::*;

        /// Whether `uv_pep440` considers `candidate` to satisfy `specifier`.
        fn pip_matches(specifier: &str, candidate: &str) -> bool {
            let specifiers = VersionSpecifiers::from_str(specifier).unwrap();
            let version = PypiVersion::from_str(candidate).unwrap();
            specifiers.contains(&version)
        }

        /// Whether `clause` (already converted from `specifier`) considers
        /// `candidate` (reformatted to conda's CEP-33 spelling, same as
        /// production's own [`conda_version`]) to satisfy it.
        fn matchspec_matches(clause: &VersionSpec, candidate: &str) -> bool {
            let pypi_version = PypiVersion::from_str(candidate).unwrap();
            let conda_version = parse_conda_version(&format_version(&pypi_version)).unwrap();
            clause.matches(&conda_version)
        }

        /// Converts `specifier` to a [`VersionSpec`] once, then asserts its
        /// [`matchspec_matches`] result agrees with [`pip_matches`]
        /// independently for every one of `candidates`.
        fn assert_agrees_with_pip(specifier: &str, candidates: &[&str], allow_pre: bool) {
            let clause = convert(specifier, allow_pre).unwrap().unwrap();
            for candidate in candidates {
                let pip_result = pip_matches(specifier, candidate);
                let matchspec_result = matchspec_matches(&clause, candidate);
                assert_eq!(
                    pip_result, matchspec_result,
                    "specifier {specifier:?} candidate {candidate:?}: pip says {pip_result}, \
                     matchspec says {matchspec_result}"
                );
            }
        }

        mod plain_operator {
            use super::*;

            /// A version sweep crossing every boundary this module and
            /// `exclusive_comparator_carve_out` care about: below `1.0.0`,
            /// each of its pre-release stages, `1.0.0` itself, its
            /// post-release, the next patch (and its rc), the next minor
            /// (and its alpha), and the next major (and its alpha).
            const VERSION_CANDIDATES: [&str; 16] = [
                "0.9.9",
                "1.0.0.dev0",
                "1.0.0.dev1",
                "1.0.0a0",
                "1.0.0a1",
                "1.0.0b1",
                "1.0.0rc1",
                "1.0.0rc2",
                "1.0.0",
                "1.0.0.post1",
                "1.0.1",
                "1.0.1rc1",
                "1.1.0",
                "1.1.0a0",
                "2.0.0",
                "2.0.0a0",
            ];

            #[test]
            fn plain_release_literal_agrees_with_pip_across_every_candidate() {
                for comparator in ["==", "!=", ">=", "<=", ">", "<"] {
                    assert_agrees_with_pip(
                        &format!("{comparator}1.0.0"),
                        &VERSION_CANDIDATES,
                        false,
                    );
                }
            }

            /// The literal itself is a pre-release, exercised separately
            /// from a plain release since `format_version`'s `.rc1`
            /// spelling must order correctly against every candidate.
            ///
            /// Excludes `>`: `uv_pep440`'s post-release exclusion for `>V`
            /// fires whenever release digits match regardless of `V` being
            /// a pre-release, unlike `packaging` -- a real `uv_pep440` gap,
            /// covered separately by `exclusive_comparator_carve_out`.
            #[test]
            fn rc_literal_agrees_with_pip_across_every_candidate() {
                for comparator in ["==", "!=", ">=", "<=", "<"] {
                    assert_agrees_with_pip(
                        &format!("{comparator}1.0.0rc1"),
                        &VERSION_CANDIDATES,
                        true,
                    );
                }
            }

            /// `<` is included here (unlike the `rc`/`dev` sweeps below,
            /// which exclude `>`): `uv_pep440`'s `Operator::LessThan`
            /// excludes a same-release-digits prerelease candidate only
            /// when `self` isn't itself a prerelease, matching `packaging`.
            #[test]
            fn post_release_literal_agrees_with_pip_across_every_candidate() {
                for comparator in ["==", "!=", ">=", "<=", "<"] {
                    assert_agrees_with_pip(
                        &format!("{comparator}1.0.0.post1"),
                        &VERSION_CANDIDATES,
                        false,
                    );
                }
            }

            /// A dev-release literal sorts before every other pre-release
            /// stage of the same base version -- the boundary most likely
            /// to catch a PEP 440 vs. conda CEP-33 ordering mismatch.
            ///
            /// Excludes `>`, for the same `uv_pep440`-vs-`packaging` gap as
            /// `rc_literal_agrees_with_pip_across_every_candidate`.
            #[test]
            fn dev_release_literal_agrees_with_pip_across_every_candidate() {
                for comparator in ["==", "!=", ">=", "<=", "<"] {
                    assert_agrees_with_pip(
                        &format!("{comparator}1.0.0.dev1"),
                        &VERSION_CANDIDATES,
                        true,
                    );
                }
            }

            #[test]
            fn inclusive_range_agrees_with_pip_across_every_candidate() {
                assert_agrees_with_pip(">=1.0.0,<=1.1.0", &VERSION_CANDIDATES, false);
            }
        }

        /// `~=` expands to a range anchored at `<X.(Y+1).0a0`; the
        /// candidate sweep concentrates on that boundary.
        mod compatible_release {
            use super::*;

            #[test]
            fn three_segment_base_agrees_with_pip_across_the_boundary() {
                let candidates = [
                    "3.12.9",
                    "3.13.1",
                    "3.13.2",
                    "3.13.2.post1",
                    "3.13.99",
                    "3.14.0.dev0",
                    "3.14.0a0",
                    "3.14.0a1",
                    "3.14.0b1",
                    "3.14.0rc1",
                    "3.14.0",
                    "3.15.0",
                ];
                assert_agrees_with_pip("~=3.13.2", &candidates, false);
            }

            /// `~=3.13` bumps the *major* segment (there's no minor left to
            /// bump), so the boundary moves to `4.0.0a0` instead.
            #[test]
            fn two_segment_base_agrees_with_pip_across_the_boundary() {
                let candidates = [
                    "3.0.0",
                    "3.13.0",
                    "3.99.0",
                    "4.0.0.dev0",
                    "4.0.0a0",
                    "4.0.0a1",
                    "4.0.0",
                    "5.0.0",
                ];
                assert_agrees_with_pip("~=3.13", &candidates, false);
            }

            /// `~=1.2.3.4` only bumps the last segment (`<1.2.4.0a0`), not
            /// the third -- the boundary sits one segment deeper than the
            /// three-segment case.
            #[test]
            fn four_segment_base_agrees_with_pip_across_the_boundary() {
                let candidates = [
                    "1.2.3.3",
                    "1.2.3.4",
                    "1.2.3.99",
                    "1.2.4.0.dev0",
                    "1.2.4.0a0",
                    "1.2.4.0a1",
                    "1.2.4.0",
                    "1.2.5.0",
                ];
                assert_agrees_with_pip("~=1.2.3.4", &candidates, false);
            }

            #[test]
            fn epoch_is_preserved_on_both_bounds() {
                let candidates = [
                    "1!3.12.9",
                    "1!3.13.2",
                    "1!3.13.99",
                    "1!3.14.0a0",
                    "1!3.14.0",
                    "3.13.2",   // no epoch -- epoch 0, below every `1!...` candidate
                    "2!3.13.2", // a higher epoch, above every `1!...` candidate
                ];
                assert_agrees_with_pip("~=1!3.13.2", &candidates, false);
            }

            #[test]
            fn pre_release_base_with_allow_pre_agrees_with_pip_across_the_boundary() {
                let candidates = [
                    "3.13.1",
                    "3.13.2.dev0",
                    "3.13.2a0",
                    "3.13.2a1",
                    "3.13.2b1",
                    "3.13.2rc1",
                    "3.13.2",
                    "3.14.0a0",
                ];
                assert_agrees_with_pip("~=3.13.2rc1", &candidates, true);
            }
        }

        /// `==X.Y.*` rewrites to the fuzzy `=X.Y` form and `!=X.Y.*` passes
        /// through unchanged -- equivalence matters most exactly at the
        /// minor-version boundary a glob straddles.
        mod globs {
            use super::*;

            const BOUNDARY_CANDIDATES: [&str; 11] = [
                "0.9.9",
                "1.0.0.dev0",
                "1.0.0a0",
                "1.0.0",
                "1.0.0.post1",
                "1.0.5",
                "1.0.99",
                "1.1.0.dev0",
                "1.1.0a0",
                "1.1.0",
                "1.10.0", // shares the "1.1" string prefix but is a different minor
            ];

            #[test]
            fn equality_glob_agrees_with_pip_across_the_boundary() {
                assert_agrees_with_pip("==1.0.*", &BOUNDARY_CANDIDATES, false);
            }

            #[test]
            fn inequality_glob_agrees_with_pip_across_the_boundary() {
                assert_agrees_with_pip("!=1.0.*", &BOUNDARY_CANDIDATES, false);
            }

            #[test]
            fn single_segment_glob_agrees_with_pip_across_the_boundary() {
                let candidates = [
                    "0.9.9",
                    "1.0.0.dev0",
                    "1.0.0a0",
                    "1.0.0",
                    "1.99.0",
                    "2.0.0.dev0",
                    "2.0.0a0",
                    "2.0.0",
                ];
                assert_agrees_with_pip("==1.*", &candidates, false);
            }

            #[test]
            fn glob_with_an_epoch_agrees_with_pip_across_the_boundary() {
                let candidates = [
                    "1.0.5", "1!0.9.9", "1!1.0.0", "1!1.0.5", "1!1.1.0", "2!1.0.0",
                ];
                assert_agrees_with_pip("==1!1.0.*", &candidates, false);
            }
        }

        mod epoch {
            use super::*;

            const CANDIDATES: [&str; 6] =
                ["0.5.0", "1.0.0", "1!0.5.0", "1!1.0.0", "1!2.0.0", "2!0.1.0"];

            /// Every comparator, including strict `<`/`>`, is safe here:
            /// none of `CANDIDATES` shares the literal's epoch *and*
            /// trimmed release, which is the only situation
            /// `exclusive_comparator_carve_out` shows disagreement in.
            #[test]
            fn agrees_with_pip_across_every_candidate() {
                for comparator in ["==", "!=", ">=", ">", "<=", "<"] {
                    assert_agrees_with_pip(&format!("{comparator}1!1.0.0"), &CANDIDATES, false);
                }
            }
        }

        /// PEP 440 gives strict `<`/`>` a carve-out other operators don't
        /// need: `<V` excludes every pre-release of `V` itself unless `V`
        /// is a pre-release, and `>V` excludes every post-release of `V`
        /// itself unless `V` is a post- or dev-release.
        ///
        /// [`convert_exclusive_less_than`] reproduces the `<V` side by
        /// gluing `a0` onto `V`'s spelling with no separating dot (`<V` ->
        /// `<Va0`), not `<V.a0`: a dotted anchor would leave a same-shape
        /// dev-release of `V` unexcluded, since `dev` sorts above `a` when
        /// compared as sibling dot-delimited parts.
        mod exclusive_comparator_carve_out {
            use super::*;

            #[test]
            fn strict_less_than_excludes_a_dev_release_of_the_boundary() {
                assert_agrees_with_pip("<1.0.0", &["1.0.0.dev0"], true);
            }

            #[test]
            fn strict_less_than_excludes_an_rc_release_of_the_boundary() {
                assert_agrees_with_pip("<1.0.0", &["1.0.0rc1"], true);
            }

            #[test]
            fn strict_less_than_still_includes_versions_below_the_boundary() {
                assert_agrees_with_pip("<1.0.0", &["0.9.9", "1.0.0a0", "0.9.9.post1"], false);
            }

            /// The motivating case for gluing `a0` with no dot at all: `V`
            /// (`2.0`) has no patch segment, so the anchor is `<2.0a0`, not
            /// `<2.0.0a0` -- a dotted anchor would leave `2.0.dev0`
            /// unexcluded.
            #[test]
            fn strict_less_than_with_a_missing_patch_segment_still_excludes_a_same_shape_dev_release(
            ) {
                assert_agrees_with_pip(
                    "<2.0",
                    &["2.0.dev0", "2.0.0.dev0", "2.0a0", "2.0", "1.9"],
                    true,
                );
            }

            /// `V` itself a pre-release needs no carve-out -- `<1.0.0rc1`
            /// already excludes everything at or above it via ordinary
            /// comparison.
            #[test]
            fn strict_less_than_with_a_pre_release_boundary_is_a_plain_passthrough() {
                assert_agrees_with_pip(
                    "<1.0.0rc1",
                    &[
                        "1.0.0.dev0",
                        "1.0.0a0",
                        "1.0.0rc1.dev0",
                        "1.0.0rc1",
                        "1.0.0",
                    ],
                    true,
                );
            }

            /// `V` a post-release (not itself a pre-release) still gets the
            /// carve-out -- it excludes dev-releases of that post-release,
            /// which count as `V`'s own pre-release family.
            #[test]
            fn strict_less_than_with_a_post_release_boundary_excludes_its_dev_releases() {
                assert_agrees_with_pip(
                    "<1.0.0.post1",
                    &["1.0.0.post1.dev0", "1.0.0.post0", "1.0.0", "1.0.0.post1"],
                    true,
                );
            }

            #[test]
            fn strict_greater_than_excludes_a_post_release_of_the_boundary() {
                assert_agrees_with_pip(">1.0.0", &["1.0.0.post1", "1.0.0.post999999"], false);
            }

            #[test]
            fn strict_greater_than_still_includes_versions_above_the_boundary() {
                assert_agrees_with_pip(">1.0.0", &["1.0.1", "1.0.1a0", "2.0.0"], true);
            }

            #[test]
            fn strict_greater_than_with_a_pre_release_boundary_excludes_its_post_releases() {
                assert_agrees_with_pip(
                    ">1.0.0rc1",
                    &["1.0.0rc1.post0", "1.0.0rc1", "1.0.0rc2", "1.0.0"],
                    true,
                );
            }

            #[test]
            fn strict_greater_than_with_a_dev_release_boundary_is_a_plain_passthrough() {
                assert_agrees_with_pip(
                    ">1.0.0.dev1",
                    &["1.0.0.dev0", "1.0.0.dev2", "1.0.0a0", "1.0.0"],
                    true,
                );
            }

            #[test]
            fn strict_greater_than_with_a_post_release_boundary_is_a_plain_passthrough() {
                assert_agrees_with_pip(
                    ">1.0.0.post1",
                    &["1.0.0.post0", "1.0.0.post2", "1.0.1"],
                    false,
                );
            }

            #[test]
            fn strict_greater_than_carve_out_respects_the_epoch() {
                assert_agrees_with_pip(
                    ">1!1.0.0",
                    &["1!1.0.0.post1", "1.0.0.post1", "2!1.0.0.post1", "1!1.0.1"],
                    false,
                );
            }

            /// The `!=V.post*` exclusion must be scoped to `V`'s own epoch
            /// even when `V` is a pre-release, not just a plain release
            /// (see the test above).
            #[test]
            fn strict_greater_than_carve_out_respects_the_epoch_with_a_pre_release_boundary() {
                assert_agrees_with_pip(
                    ">1!1.0.0rc1",
                    &[
                        "1!1.0.0rc1.post0", // same epoch, V's own post family -- excluded
                        "1.0.0rc1.post0",   // epoch 0 -- already excluded by the epoch itself
                        "2!1.0.0rc1.post0", // higher epoch -- not part of V's family
                        "1!1.0.0rc2",
                        "1!1.0.0",
                    ],
                    true,
                );
            }

            /// Classic PEP 440 gotcha: `>=X.rc1,<X` matches nothing in
            /// pip, since `<X` excludes `X`'s whole pre-release family
            /// regardless of the lower bound.
            #[test]
            fn rc_lower_bound_with_a_plain_upper_bound_is_an_empty_range_in_pip() {
                assert_agrees_with_pip(">=1.0.0rc1,<1.0.0", &["1.0.0rc1"], true);
            }
        }
    }
}
