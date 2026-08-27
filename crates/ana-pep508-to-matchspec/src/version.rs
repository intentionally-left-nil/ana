//! PEP 440 version specifier(s) -> conda `VersionSpec`.
//!
//! Rust port of reroll's `matchspec_specifier.py` (the Operator conversion
//! table) and `version_format.py` (CEP-33 formatting), adapted to build a
//! typed [`VersionSpec`] value directly instead of formatting a matchspec
//! version *clause string* and handing it back to a parser -- see
//! `investigations/pep508_to_matchspec_api.md`'s headline finding.
//!
//! One string round-trip remains, and is unavoidable: `rattler_conda_types`
//! has no general typed `Version` constructor (only `Version::major(u64)`,
//! good for a single release segment). [`ana_marker_matchspec::format_version`]
//! spells a `uv_pep440::Version` out as its CEP-33 string -- shared with
//! `ana-marker-matchspec`, which needs the identical formatting for its own
//! marker-version leaves, rather than keeping a second, near-verbatim copy
//! (this crate already depends on that one, per
//! `investigations/pep508_to_matchspec_api.md`'s crate layout, so the
//! sharing is a plain function call, not a new dependency edge) -- and
//! [`conda_version`]/[`parse_conda_version`] parse that straight back with
//! `Version::from_str`. That's a small, regular, non-backtracking grammar --
//! unlike reparsing an entire matchspec (or even a whole comma-joined
//! version *clause*), which this module never does: every [`VersionSpec`]
//! variant (`Range`, `StrictRange`, `Exact`, `Group`) is constructed
//! directly, leaf by leaf.

use std::fmt::Write as _;

use rattler_conda_types::{
    EqualityOperator, LogicalOperator, ParseVersionError, RangeOperator, StrictRangeOperator,
    StrictVersion, Version as CondaVersion, VersionSpec,
};
use uv_pep440::{Operator, Version as PypiVersion, VersionSpecifier, VersionSpecifiers};

use crate::ConvertError;

/// The conda `VersionSpec` for every specifier in `specifiers`, joined as an
/// implicit AND (PEP 508's comma-separated specifier list has no OR form),
/// or `None` if `specifiers` is empty -- a bare, unversioned dependency.
///
/// `allow_pre` governs whether a pre-release specifier version is accepted;
/// see [`reject_unsupported_version`].
///
/// Specifiers are visited in reroll's own canonical order (lower bounds,
/// then upper bounds, then pins, then exclusions; ties broken by the
/// specifier's own string spelling) purely for deterministic, human-legible
/// output -- `VersionSpec::Group`'s `,`-joined clauses are logically an
/// AND, so this has no effect on which versions match.
pub(crate) fn version_spec(
    specifiers: &VersionSpecifiers,
    allow_pre: bool,
) -> Result<Option<VersionSpec>, ConvertError> {
    if specifiers.is_empty() {
        return Ok(None);
    }

    let mut ordered: Vec<&VersionSpecifier> = specifiers.iter().collect();
    // `sort_by` (not `sort_by_key`): the tie-break spelling is only ever
    // needed when two specifiers share a rank, so comparing lazily avoids
    // formatting a `String` for every specifier up front -- the common
    // case is a single specifier (no sort needed at all) or a handful of
    // specifiers with distinct ranks (no tie-break ever reached). See
    // `cmp_specifiers`'s docs.
    ordered.sort_by(cmp_specifiers);

    let mut leaves = Vec::with_capacity(ordered.len());
    for specifier in ordered {
        leaves.extend(convert_specifier(specifier, allow_pre)?);
    }

    Ok(Some(match leaves.len() {
        1 => match leaves.into_iter().next() {
            Some(leaf) => leaf,
            // `len() == 1` was just checked above.
            None => unreachable!("a length-1 Vec always yields one element"),
        },
        _ => VersionSpec::Group(LogicalOperator::And, leaves),
    }))
}

/// Reroll's canonical clause order: lower bounds, then upper bounds, then
/// pins, then exclusions ([`operator_rank`]); ties within the same rank
/// broken by the specifier's own PEP 440 text (e.g. `">=9.0"`), which is
/// what reroll sorts ties by -- not our own CEP-33 spelling, so e.g.
/// `>=9.0` sorts before `>=10.0` lexicographically (`"1" < "9"` puts
/// `">=10.0"` first), matching `same_operator_ties_sort_lexicographically`.
///
/// The tie-break spelling is computed with `.to_string()` only when the
/// ranks are actually equal -- `sort_by`'s comparator runs lazily during
/// comparisons, unlike `sort_by_key`'s upfront per-element decoration, so
/// a specifier list with no rank ties (the common case: most requirements
/// have zero or one specifier) never allocates a tie-break string at all.
fn cmp_specifiers(a: &&VersionSpecifier, b: &&VersionSpecifier) -> std::cmp::Ordering {
    operator_rank(*a.operator())
        .cmp(&operator_rank(*b.operator()))
        .then_with(|| a.to_string().cmp(&b.to_string()))
}

/// Lower bounds, then upper bounds, then pins, then exclusions -- written
/// as an exhaustive match with no wildcard arm so a future `uv-pep440` bump
/// adding an eleventh [`Operator`] variant is a compile error here, not a
/// silent gap (mirrors the same requirement on `ana-marker-matchspec`'s
/// `MarkerExpression` match, once that crate exists).
fn operator_rank(operator: Operator) -> u8 {
    match operator {
        Operator::GreaterThanEqual | Operator::GreaterThan => 0,
        Operator::LessThanEqual | Operator::LessThan => 1,
        Operator::Equal | Operator::EqualStar | Operator::ExactEqual | Operator::TildeEqual => 2,
        Operator::NotEqual | Operator::NotEqualStar => 3,
    }
}

/// One [`VersionSpecifier`]'s contribution to a `VersionSpec` -- one leaf
/// for every operator except `~=` (always two: the expanded range) and `>`
/// without a `.post`/dev suffix already spelled out for the exclusion.
///
/// Exhaustive over [`Operator`]'s ten variants, no wildcard arm -- see
/// [`operator_rank`]'s docs for why that's deliberate.
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
            // `===`'s value is `==`'s from here on -- reroll's own
            // `operator = "==" if specifier.operator == "===" else
            // specifier.operator`. Unlike Python's `packaging`, `uv_pep440`
            // requires `===`'s right-hand side to itself parse as a PEP 440
            // version (confirmed against `uv-pep508` 0.12.6's own parser,
            // not assumed): PEP 440 permits `===` against an arbitrary
            // non-version string, but a requirement using that permission
            // (e.g. `requests===some-weird-string`) fails to parse as a
            // `Requirement` at all before it ever reaches this function,
            // surfacing as a structural parse error one layer up
            // (`ana-pyproject`), not a `ConvertError` here. So the
            // "non-PEP-440 string" fallback reroll's own
            // `_convert_specifier` has is unreachable in this port -- there
            // is no equivalent branch here, deliberately.
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
            // The wildcard grammar only ever pairs a bare release-segment
            // prefix (optionally epoch-prefixed) with `.*` -- never a
            // pre/post/dev/local suffix -- so `reject_unsupported_version`
            // can never actually fire here. Called anyway, uniformly with
            // every other branch, so a future relaxation of that grammar
            // fails safe instead of silently accepting an unsupported
            // version.
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
/// `Range(Less, _)` has no such family exception, so a non-pre-release `V`
/// needs an explicit anchor: `Va0`, an `a0` pre-release tag glued directly
/// onto `V`'s own CEP-33 spelling with no separating dot, which sorts below
/// every pre-release of `V`.
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
/// plain `Range(Greater, _)` has no such exception and post-releases have
/// no fixed upper anchor to glue on, so the exclusion needs its own clause:
/// `Range(Greater, V)` plus `StrictRange(NotStartsWith, "V.post")` -- a
/// glob-style "not equal to any post-release of `V`."
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

/// `~=X.Y.Z`'s expansion into `>=X.Y.Z,<X.(Y+1).0a0` -- CEP 29 deprecates
/// conda's own native `~=` (`StrictRangeOperator::Compatible`), and its
/// exact semantics aren't guaranteed equivalent to PEP 440's compatible
/// release clause, so reroll always expands `~=` into an explicit
/// `>=`/`<` pair rather than relying on that equivalence, and this port
/// keeps that choice.
fn expand_compatible_release(
    specifier: &VersionSpecifier,
    allow_pre: bool,
) -> Result<Vec<VersionSpec>, ConvertError> {
    reject_unsupported_version(specifier, allow_pre)?;
    let version = specifier.version();
    let release = version.release();
    // `uv_pep440::VersionSpecifier::from_version` enforces at least two
    // release segments for `~=` at construction time (its own doc comment:
    // "Invariant: With ~=, there are always at least 2 release segments"),
    // so a `Requirement` handed to us here can never violate it.
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
/// Mirrors reroll's `_reject_unsupported_version`, checked ahead of every
/// per-operator conversion below -- local labels first, unconditionally,
/// even when `allow_pre` would otherwise permit a pre-release boundary.
///
/// The local-label branch is only reachable for `Equal`/`NotEqual`/
/// `ExactEqual` in practice: confirmed directly against `uv-pep440` 0.12.6
/// (not assumed from PEP 440's own text) that `VersionSpecifier::from_str`
/// itself already rejects a local segment paired with every other
/// operator here (`<`/`>`/`~=`/the `.*` glob forms) as
/// `Operator::is_local_compatible` returning `false` for all of them --
/// the same shape as reroll's `test_local_version_label_with_strict_less_or_greater_than_is_rejected_by_packaging`,
/// which observes the equivalent Python behavior. Checked uniformly here
/// anyway: cheap, and it means a future `uv-pep440` relaxing that
/// restriction fails safe (a `ConvertError`) instead of silently building
/// an unrepresentable `VersionSpec`.
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
/// release -- e.g. PEP 440's `1.0.0rc1` becomes `1.0.0.rc1`, and its
/// `1.0-1` shorthand becomes `1.0.post1`. Direct Rust port of reroll's
/// `version_format.format_version`; `PrereleaseKind`'s own `Display`
/// already spells `a`/`b`/`rc`, the same letters `packaging.version`
/// normalizes pre-release kinds to, so no separate letter lookup is
/// needed.
///
/// Never emits a local segment -- callers reject one first, via
/// [`reject_unsupported_version`], same as reroll's original docs note.
///
/// This is [`ana_marker_matchspec::format_version`], not a local copy --
/// see the module docs.
fn format_version(version: &PypiVersion) -> String {
    ana_marker_matchspec::format_version(version)
}

/// [`format_version`] then [`CondaVersion::from_str`] in one step -- the
/// one string round-trip this module can't avoid; see the module docs.
fn conda_version(version: &PypiVersion) -> Result<CondaVersion, ConvertError> {
    parse_conda_version(&format_version(version))
}

/// `CondaVersion::from_str(literal)`, wrapping a parse failure as a
/// [`ConvertError`]. In practice `literal` is always one this module built
/// itself (via [`format_version`], possibly with a `a0`/`.post`/bumped-
/// release suffix appended) from an already-validated `uv_pep440::Version`,
/// so this should never actually fail -- but it's a cheap, regular parse
/// (see the module docs) and propagating `Result` instead of unwrapping
/// costs nothing and keeps a future formatting bug from panicking instead
/// of erroring.
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

    /// `expected` parsed as a conda version-spec bracket value (e.g.
    /// `">=1.0.0,<2.0.0a0"`), for comparing against [`convert`]'s output
    /// without hand-building a `VersionSpec` AST per test -- the same
    /// "compare against the parser's own understanding" approach
    /// `investigations/pep508_to_matchspec_api.md`'s testing-strategy
    /// section recommends, applied to `VersionSpec` instead of a whole
    /// `MatchSpec`.
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

        /// Ported from reroll's `test_matchspec_specifier.py`'s
        /// `test_multiple_specifiers_with_the_same_operator_sort_lexicographically`
        /// -- the `!=` exclusion-category counterpart to
        /// `same_operator_ties_sort_lexicographically` above. Unlike that
        /// test's `>=9.0,>=10.0` (which visibly reorders), `!=1.0.0` and
        /// `!=2.0.0` already sort lexicographically in input order, so this
        /// pins that the lexicographic tiebreak doesn't accidentally
        /// reorder an already-sorted pair.
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

    /// Equivalence oracle, ported from reroll's
    /// `test_version_matchspec_equivalence.py` (via its `tests/version_oracle.py`
    /// helper): checks the [`VersionSpec`] [`version_spec`] produces for a
    /// bare PEP 440 specifier (set) against `uv_pep440`'s own
    /// `VersionSpecifiers::contains` for a sweep of candidate versions, so a
    /// passing test proves actual semantic equivalence for every candidate
    /// rather than agreement with one hand-picked expected string.
    ///
    /// `VersionSpecifiers::contains` is the "pure range" question --
    /// `uv_pep440`'s equivalent of `packaging`'s
    /// `SpecifierSet.contains(candidate, prereleases=True)`, which is what
    /// reroll's own oracle uses -- independent of any separate
    /// default-exclude-prereleases policy, which `VersionSpec::matches` has
    /// no equivalent of at this layer either, so this is the fair
    /// comparison.
    mod equivalence_oracle {
        use super::*;

        /// Whether `uv_pep440` considers `candidate` to satisfy `specifier`.
        fn pip_matches(specifier: &str, candidate: &str) -> bool {
            let specifiers = VersionSpecifiers::from_str(specifier).unwrap();
            let version = PypiVersion::from_str(candidate).unwrap();
            specifiers.contains(&version)
        }

        /// Whether `clause` (already converted from `specifier`) considers
        /// `candidate` (a PyPI-spelled version, reformatted to conda's
        /// CEP-33 spelling here, same as production's own [`conda_version`])
        /// to satisfy it.
        fn matchspec_matches(clause: &VersionSpec, candidate: &str) -> bool {
            let pypi_version = PypiVersion::from_str(candidate).unwrap();
            let conda_version = parse_conda_version(&format_version(&pypi_version)).unwrap();
            clause.matches(&conda_version)
        }

        /// Converts `specifier` to a [`VersionSpec`] once, then asserts that
        /// clause's [`matchspec_matches`] result agrees with [`pip_matches`]
        /// independently for every one of `candidates` -- the equivalence
        /// [`version_spec`] exists to preserve.
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
            /// every one of `1.0.0`'s own pre-release stages (dev, alpha,
            /// beta, rc), `1.0.0` itself, its post-release, the next patch
            /// (and *its* rc), the next minor (and its own alpha), and the
            /// next major (and its own alpha).
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

            /// The literal itself is a pre-release (`1.0.0rc1`) -- exercised
            /// separately from a plain-release literal since
            /// `format_version`'s `.rc1` spelling, not just the comparator,
            /// is what must order correctly against every candidate.
            ///
            /// Excludes `>`: confirmed directly against both `uv_pep440`
            /// 0.12.6's own `VersionSpecifier::contains` source
            /// (`Operator::GreaterThan`'s post-release exclusion fires
            /// whenever the *release digits* match and `self` isn't itself
            /// a post-release -- it has no carve-out for `self` being a
            /// pre-release the way reroll's/`packaging`'s own `>V` logic
            /// does) and against `packaging.specifiers.SpecifierSet`
            /// directly (`SpecifierSet(">1.0.0rc1").contains("1.0.0.post1",
            /// prereleases=True)` is `True`, not `False`) that `uv_pep440`'s
            /// `contains()` disagrees with `packaging` for `>V` when `V` is
            /// a pre-release and the candidate is a post-release of the
            /// same base -- a real gap in `uv_pep440`, not in this crate's
            /// conversion (which never delegates to `uv_pep440::contains`
            /// for its own correctness; see [`convert_exclusive_greater_than`]).
            /// `exclusive_comparator_carve_out`'s
            /// `strict_greater_than_with_a_pre_release_boundary_excludes_its_post_releases`
            /// covers this exact shape with reroll's own curated candidates
            /// instead.
            ///
            /// Re-confirmed still open after the `uv-pep440` 0.9.7 -> 0.12.6
            /// bump (see the workspace `Cargo.toml`'s pin comment): uv#20268
            /// ("Fix exclusive post-release ordering") reworked
            /// `Operator::LessThan`/`GreaterThan` range construction in
            /// `uv-pep440`'s `version_ranges.rs` but did not touch
            /// `VersionSpecifier::contains`'s own separate implementation in
            /// `version_specifier.rs` that this test's oracle calls into, so
            /// this exact `contains()`-vs-`packaging` gap for `>V` with a
            /// pre-release `V` is unchanged -- checked directly against
            /// `uv_pep440` 0.12.6, not assumed from the changelog.
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

            /// Previously excluded `<`: `uv_pep440` 0.9.7's
            /// `Operator::LessThan` excluded any same-release-digits
            /// prerelease candidate whenever `self` itself wasn't a
            /// prerelease, which overreached for a post-release `self` the
            /// way `packaging` does not (`SpecifierSet("<1.0.0.post1").contains("1.0.0.dev0",
            /// prereleases=True)` is `True`, not `False`).
            ///
            /// **Fixed by the `uv-pep440` 0.9.7 -> 0.12.6 bump**: uv#20268
            /// ("Fix exclusive post-release ordering") reworked
            /// `Operator::LessThan`'s range to `< V.dev0` instead of the old
            /// two-piece "below the base version's own pre-releases, union
            /// [base, V)" split, which is exactly the base-pre-release
            /// overreach `packaging` never had. Re-verified directly against
            /// `uv_pep440` 0.12.6 (this test failed with `<` included before
            /// the bump, confirmed by temporarily re-pinning to `0.9.7` and
            /// re-running it -- not assumed from the changelog alone), so
            /// `<` now belongs in the same sweep as every other operator
            /// instead of needing `exclusive_comparator_carve_out`'s
            /// narrower, reroll-curated candidates as a workaround for a gap
            /// that no longer exists. That carve-out test is left in place
            /// regardless: it pins the same *matchspec* behavior via a
            /// different, still-valuable route (reroll's own curated
            /// expectations) that doesn't depend on `uv_pep440::contains`
            /// agreeing with anything.
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

            /// A dev-release literal (`1.0.0.dev1`) sorts *before* every
            /// other pre-release stage of the same base version per PEP 440
            /// -- the boundary most likely to catch a PEP 440 vs. conda
            /// CEP-33 ordering mismatch, if one existed.
            ///
            /// Excludes `>`, for the same `uv_pep440`-vs-`packaging` gap as
            /// `rc_literal_agrees_with_pip_across_every_candidate` (a
            /// dev-release `self` hits the identical overreach). Covered
            /// instead by `exclusive_comparator_carve_out`'s
            /// `strict_greater_than_with_a_dev_release_boundary_is_a_plain_passthrough`.
            /// Still excluded after the `uv-pep440` 0.9.7 -> 0.12.6 bump --
            /// same re-verification as `rc_literal_agrees_with_pip_across_every_candidate`'s
            /// doc: uv#20268 didn't touch `VersionSpecifier::contains`, only
            /// the `Ranges`/`Operator::LessThan` construction path.
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

        /// `~=` expands to a range anchored at `<X.(Y+1).0a0` -- the
        /// anchor's whole purpose is to land a pre-release of that boundary
        /// on the lower side, so the candidate sweep here concentrates on
        /// the boundary itself.
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

        /// PEP 440 gives strict `<`/`>` a carve-out that the other
        /// operators (covered by `plain_operator` above) don't need:
        ///
        /// * `<V` excludes *every* pre-release of `V` itself (dev, alpha,
        ///   beta, rc) -- not just versions mathematically below `V` --
        ///   unless `V` is itself a pre-release.
        /// * `>V` excludes *every* post-release of `V` itself -- unless `V`
        ///   is itself a post-release or dev-release.
        ///
        /// [`convert_exclusive_less_than`] reproduces the `<V` side by
        /// gluing a bare `a0` pre-release tag directly onto `V`'s own
        /// conda-spelled version, with no separating dot (`<V` -> `<Va0`)
        /// -- not the dotted `<V.a0` or a synthetic-zero `<V.0a0` a reader
        /// might expect from the `~=` expansion's anchor. The dot matters:
        /// conda orders a `dev` tag *above* a same-position `a`/`b`/`rc`
        /// tag when they're compared as separate dot-delimited parts, so a
        /// dotted anchor (or one with an inserted zero segment) leaves a
        /// same-shape dev-release of the boundary unexcluded; gluing `a0`
        /// straight onto `V`'s last digit instead folds the comparison
        /// into `V`'s own release digits, which *does* sort below every
        /// pre-release spelling.
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
            /// (`2.0`) has no patch segment of its own, so the anchor is
            /// `<2.0a0`, not `<2.0.0a0`. A dotted or zero-padded anchor
            /// would leave `2.0.dev0` -- a dev-release with the same
            /// two-segment shape as `V` -- unexcluded, since conda would
            /// then compare `dev0` against `a0`/`0a0` as sibling
            /// dot-delimited parts (where `dev` sorts above `a`) rather
            /// than folding into `V`'s own release digits.
            #[test]
            fn strict_less_than_with_a_missing_patch_segment_still_excludes_a_same_shape_dev_release(
            ) {
                assert_agrees_with_pip(
                    "<2.0",
                    &["2.0.dev0", "2.0.0.dev0", "2.0a0", "2.0", "1.9"],
                    true,
                );
            }

            /// `V` itself a pre-release (`1.0.0rc1`) needs no carve-out --
            /// `<1.0.0rc1` already excludes everything at or above
            /// `1.0.0rc1` via ordinary comparison, dev-releases of
            /// `1.0.0rc1` included.
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

            /// `V` a post-release (`1.0.0.post1`, not itself a
            /// pre-release) still gets the carve-out -- it excludes
            /// dev-releases of that specific post-release, which sort
            /// below `V` mathematically but count as `V`'s own
            /// pre-release family.
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

            /// The `>V` fix's `!=V.post*` exclusion clause must be scoped
            /// to `V`'s own epoch even when `V` is a pre-release, not just
            /// when `V` is a plain release (see the test above).
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
