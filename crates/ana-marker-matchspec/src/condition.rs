//! `MarkerTree` -> `MatchSpecCondition` conversion, single-target.
//!
//! [`to_matchspec_condition`] is the entry point: `restrict()` (see
//! [`crate::assumption`]) narrows away every known key, leaving only the
//! free `python_version`/`python_full_version`/`implementation_version`
//! family (or a key deliberately left out of the assumption, like
//! `platform_release`/`platform_version`), which gets converted leaf by
//! leaf via [`to_dnf`]'s flattened `Or<And<MarkerExpression>>` form.
//!
//! `uv_pep508` 0.12.6 rewrites `python_version` markers onto the *same*
//! BDD dimension as `python_full_version` before `to_dnf()` runs,
//! adjusting the operator/version to preserve minor-precision semantics:
//!
//! ```text
//! python_version >= "3.9"   -> Version { key: PythonFullVersion, specifier: >=3.9 }
//! python_version == "3.9"   -> Version { key: PythonFullVersion, specifier: ==3.9.* }  (EqualStar)
//! python_version != "3.9"   -> Version { key: PythonFullVersion, specifier: !=3.9.* }  (NotEqualStar)
//! python_version <= "3.9"   -> Version { key: PythonFullVersion, specifier: <3.10 }
//! python_version > "3.9"    -> Version { key: PythonFullVersion, specifier: >=3.10 }
//! python_version ~= "3.9"   -> two Version clauses: >=3.9 and <4  (already expanded)
//! python_version in "3.9 3.10" -> two Version clauses: >=3.9 and <3.11 (already expanded)
//! ```
//!
//! So `MarkerValueVersion::PythonVersion` never reaches [`convert_leaf`]
//! (`CanonicalMarkerValueVersion`, the BDD's own dimension enum, has no
//! `PythonVersion` variant), and `~=`/`in`/`not in` never reach it as
//! their own operator either -- both are always pre-expanded into plain
//! ordered comparisons before `to_dnf()` runs. The operator table below
//! still handles both explicitly rather than assuming they're
//! unreachable forever, so a future `uv_pep508` canonicalization change
//! hits a real code path here, not a silent gap.

use rattler_conda_types::{
    EqualityOperator, MatchSpec, MatchSpecCondition, PackageName, PackageNameMatcher,
    ParseVersionError, RangeOperator, StrictRangeOperator, StrictVersion, Version as CondaVersion,
    VersionSpec,
};
use uv_pep440::{Operator, Version as PypiVersion, VersionSpecifier};
use uv_pep508::{MarkerExpression, MarkerTree, MarkerValueVersion};

/// `MarkerExpression::VersionIn`'s `operator` field is typed
/// `uv_pep508::marker::ContainerOperator`, which isn't re-exported
/// anywhere in `uv_pep508`'s public API, so this crate can destructure a
/// value of that type but can't name it in its own signatures or match
/// arms. [`Membership`] is a local, nameable stand-in, built from the
/// unnameable value's own `Display` output (`"in"`/`"not in"`).
enum Membership {
    In,
    NotIn,
}

impl Membership {
    fn from_display(operator: impl std::fmt::Display) -> Result<Self, Unconvertible> {
        match operator.to_string().as_str() {
            "in" => Ok(Self::In),
            "not in" => Ok(Self::NotIn),
            other => Err(Unconvertible::UnsupportedOperator {
                key: "python_full_version".to_string(),
                operator: other.to_string(),
            }),
        }
    }
}

/// A dependency's applicability to the one machine `ana` is installing
/// onto -- distinct from [`Unconvertible`], which means "we don't know
/// how to represent this," not "we know, and the answer is no."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applicability {
    /// The marker holds unconditionally on this machine; no `when=`
    /// clause is needed.
    Always,
    /// The marker holds only when the given condition (over
    /// `python_version`/`python_full_version`/`implementation_version`,
    /// the only keys left free once known values are restricted away)
    /// also holds.
    Conditionally(MatchSpecCondition),
    /// The marker can never hold on this machine (e.g. `sys_platform ==
    /// "win32"` while installing on Linux); the caller should drop the
    /// dependency entirely, not treat this as an error.
    Never,
}

/// Every way [`to_matchspec_condition`] can fail to represent a marker as
/// a matchspec condition, once known values have already been restricted
/// away. Because `restrict()` already turns any marker that's constant
/// given the known values into `Applicability::Always`/`Never` before
/// this error type is ever constructed, a residual reaching here can
/// only be constant if it's *unconditionally* so, independent of the
/// free variable -- which [`to_dnf`]'s own construction rules out for
/// anything that's neither `is_true()` nor `is_false()`.
#[derive(Debug, thiserror::Error)]
pub enum Unconvertible {
    /// A marker key with no matchspec equivalent reached this layer --
    /// expected for `platform_release`/`platform_version` (deliberately
    /// left out of the assumption, see [`crate::known_values_assumption`]),
    /// and a defensive catch-all for any other `String`/`List` key.
    #[error("marker key {key:?} has no matchspec equivalent")]
    NoMatchspecEquivalent { key: String },

    /// `extra == "..."` reached this layer. `extra` is the *current
    /// package's* own extras mechanism, not an environment condition,
    /// and callers should strip `extra` clauses before ever calling into
    /// this crate -- see [`to_matchspec_condition`]'s docs.
    #[error(r#""extra" marker reached marker-condition conversion; strip it before calling"#)]
    ExtraMarker,

    /// A comparator with no matchspec equivalent for a version key --
    /// expected to be unreachable in practice (`uv_pep508` never emits
    /// `TildeEqual`/`ExactEqual` for a `Version`/`VersionIn` marker
    /// expression; see this module's docs), kept as a real, tested error
    /// arm rather than a wildcard so a future `uv_pep508` change that
    /// starts emitting one of these fails loudly here instead of
    /// silently miscompiling a matchspec.
    #[error("comparator {operator:?} is not supported for marker key {key:?}")]
    UnsupportedOperator { key: String, operator: String },

    /// A version literal this crate built itself (via CEP-33 formatting)
    /// failed to parse as a conda `Version`. Not expected to happen in
    /// practice, but propagated rather than unwrapped.
    #[error("{literal:?} did not parse as a conda version literal: {source}")]
    InvalidVersionLiteral {
        literal: String,
        #[source]
        source: ParseVersionError,
    },
}

/// Converts `marker` to an [`Applicability`], given an already-computed
/// `assumption` -- see [`crate::known_values_assumption`] to build one
/// for a subdir.
///
/// Callers must strip (or reject) any `extra == "..."` clause in
/// `marker` first -- `extra` is the current package's own extras
/// mechanism, not an environment condition this crate resolves, and
/// otherwise it surfaces as [`Unconvertible::ExtraMarker`] partway
/// through DNF conversion rather than being caught up front.
pub fn to_matchspec_condition(
    marker: MarkerTree,
    assumption: MarkerTree,
) -> Result<Applicability, Unconvertible> {
    if marker.is_true() {
        return Ok(Applicability::Always);
    }
    let residual = marker.restrict(assumption);
    if residual.is_true() {
        return Ok(Applicability::Always);
    }
    if residual.is_false() {
        return Ok(Applicability::Never);
    }
    try_fast_tree(residual).map(Applicability::Conditionally)
}

/// [`convert_leaf`] lifted from one [`MarkerExpression`] to a whole
/// [`MarkerTree`] via [`MarkerTree::to_dnf`] -- a `Vec<Vec<MarkerExpression>>`
/// that maps directly onto `Or(And(...))`. DNF form has already pushed
/// every negation down to individual leaves, so every leaf reaching
/// [`convert_leaf`] is already in its negated-if-needed form (`!=`
/// instead of `not(==)`, etc.).
fn try_fast_tree(marker: MarkerTree) -> Result<MatchSpecCondition, Unconvertible> {
    let dnf = marker.to_dnf();
    let mut or_arms = Vec::with_capacity(dnf.len());
    for clause in dnf {
        let mut and_leaves = Vec::with_capacity(clause.len());
        for expression in &clause {
            and_leaves.push(convert_leaf(expression)?);
        }
        or_arms.push(and_chain(and_leaves));
    }
    Ok(or_chain(or_arms))
}

/// Folds a non-empty `Vec<MatchSpecCondition>` with `And`. Panics on an
/// empty vec -- `to_dnf()` never produces an empty inner clause for a
/// marker that isn't already `is_true()`/`is_false()` (both handled
/// before [`try_fast_tree`] is ever called).
fn and_chain(mut leaves: Vec<MatchSpecCondition>) -> MatchSpecCondition {
    let mut acc = leaves.remove(0);
    for leaf in leaves {
        acc = MatchSpecCondition::And(Box::new(acc), Box::new(leaf));
    }
    acc
}

/// Folds a non-empty `Vec<MatchSpecCondition>` with `Or`. Same invariant
/// as [`and_chain`]: `to_dnf()` never produces an empty outer list here.
fn or_chain(mut arms: Vec<MatchSpecCondition>) -> MatchSpecCondition {
    let mut acc = arms.remove(0);
    for arm in arms {
        acc = MatchSpecCondition::Or(Box::new(acc), Box::new(arm));
    }
    acc
}

/// One [`MarkerExpression`] leaf -> one [`MatchSpecCondition`]. Exhaustive
/// over all 5 `MarkerExpression` variants, no wildcard arm -- a future
/// `uv_pep508` bump adding a new marker-expression shape (PEP 751 already
/// added `List`) should be a compile error here, not a silent gap.
fn convert_leaf(expression: &MarkerExpression) -> Result<MatchSpecCondition, Unconvertible> {
    match expression {
        MarkerExpression::Version { key, specifier } => version_condition(*key, specifier),
        MarkerExpression::VersionIn {
            key,
            versions,
            operator,
        } => version_in_condition(*key, versions, Membership::from_display(operator)?),
        MarkerExpression::String { key, .. } => Err(Unconvertible::NoMatchspecEquivalent {
            key: key.to_string(),
        }),
        // `pair`'s type isn't re-exported by `uv_pep508` either (see
        // [`Membership`]'s docs); `{pair:?}` is the only thing this crate
        // can do with it from outside the crate.
        MarkerExpression::List { pair, .. } => Err(Unconvertible::NoMatchspecEquivalent {
            key: format!("{pair:?}"),
        }),
        MarkerExpression::Extra { .. } => Err(Unconvertible::ExtraMarker),
    }
}

/// `python_version`/`python_full_version`/`implementation_version` all
/// convert to a condition on conda's own `python` package version --
/// `python_full_version`/`implementation_version` mean "the running
/// CPython's version" (CPython is the only supported interpreter). See
/// the module docs for why `python_version` itself never actually
/// reaches here as its own key.
fn version_condition(
    key: MarkerValueVersion,
    specifier: &VersionSpecifier,
) -> Result<MatchSpecCondition, Unconvertible> {
    let _ = key; // every key converts identically
    convert_specifier(specifier)
}

/// `python_version in "..."`/`not in "..."`. Per the module docs,
/// `uv_pep508` canonicalizes this into a plain bounded range before
/// `to_dnf()` runs, so in practice this function is not reached at all
/// for `python_version` -- implemented for real (not stubbed) in case a
/// future `uv_pep508` version stops doing that, or reaches this shape
/// for a different key.
fn version_in_condition(
    key: MarkerValueVersion,
    versions: &[PypiVersion],
    membership: Membership,
) -> Result<MatchSpecCondition, Unconvertible> {
    let _ = key; // every key converts identically
    let equality = match membership {
        Membership::In => EqualityOperator::Equals,
        Membership::NotIn => EqualityOperator::NotEquals,
    };
    let mut leaves = Vec::with_capacity(versions.len());
    for version in versions {
        leaves.push(python_condition(VersionSpec::Exact(
            equality,
            conda_version(version)?,
        )));
    }
    Ok(match membership {
        // `in` is a disjunction (any candidate matches); `not in` is a
        // conjunction (every candidate must be excluded) -- De Morgan.
        Membership::In => or_chain(leaves),
        Membership::NotIn => and_chain(leaves),
    })
}

/// `version` as a condition on conda's `python` package.
fn python_condition(version: VersionSpec) -> MatchSpecCondition {
    MatchSpecCondition::MatchSpec(Box::new(MatchSpec {
        name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
        version: Some(version),
        ..MatchSpec::default()
    }))
}

/// `version`'s `(major, minor)` if it's at major.minor precision or
/// coarser -- every release segment from index 2 on is zero (or absent)
/// -- and carries no epoch/pre/post/dev segment, else `None`. Signals
/// whether an ordered-comparator or equality boundary needs a `.0a0`
/// pre-release anchor.
///
/// The key is gone by this layer: a `python_version`-derived comparator
/// canonicalizes onto the identical `Version { key: PythonFullVersion,
/// .. }` shape a literal `python_full_version` comparison at the same
/// precision would (e.g. both `python_version >= "3.9"` and
/// `python_full_version >= "3.9"` arrive here as `GreaterThanEqual,
/// "3.9"`), so precision is the only signal left. It's sufficient
/// because a `python_version` origin always lands at this-or-coarser
/// precision -- `uv_pep508` performs the minor/next-minor boundary
/// arithmetic internally before this crate ever sees the specifier. The
/// accepted tradeoff: a genuine `python_full_version`/
/// `implementation_version` literal written at major.minor-or-coarser
/// precision (e.g. `"3.9"`, no patch component) is indistinguishable
/// from a `python_version` origin here and gets the same anchor
/// treatment. Full patch-precision literals (`"3.9.1"`) are `None` here
/// and pass straight through unchanged.
fn minor_precision(version: &PypiVersion) -> Option<(u64, u64)> {
    if version.epoch() != 0
        || version.pre().is_some()
        || version.post().is_some()
        || version.dev().is_some()
    {
        return None;
    }
    let release = version.release();
    if release.len() > 2 && release[2..].iter().any(|&segment| segment != 0) {
        return None;
    }
    Some((
        release.first().copied().unwrap_or(0),
        release.get(1).copied().unwrap_or(0),
    ))
}

/// `{major}.{minor}.0a0` as a conda `Version` -- the pre-release-boundary
/// anchor.
fn anchor(major: u64, minor: u64) -> Result<CondaVersion, Unconvertible> {
    let literal = format!("{major}.{minor}.0a0");
    literal
        .parse()
        .map_err(|source| Unconvertible::InvalidVersionLiteral { literal, source })
}

/// One [`VersionSpecifier`]'s contribution to a [`MatchSpecCondition`].
/// Exhaustive over [`Operator`]'s ten variants -- `TildeEqual`/`ExactEqual`
/// are expected unreachable for a marker (`~=` is always pre-expanded by
/// `uv_pep508`, and `===` has no marker-operator syntax at all), but
/// written as real error arms rather than a wildcard.
///
/// `GreaterThanEqual`/`LessThan` get [`minor_precision`]'s `.0a0` anchor
/// when the boundary is at major.minor-or-coarser precision;
/// `LessThanEqual`/`GreaterThan` never do, because a `python_version`
/// origin never reaches this crate carrying either of those two
/// operators: `python_version <= "V"`/`python_version > "V"` are
/// *always* pre-rewritten onto `LessThan`/`GreaterThanEqual` with an
/// already-bumped next-minor boundary before this crate ever sees them
/// (this module's docs' canonicalization table), so a bare
/// `LessThanEqual`/`GreaterThan` reaching here can only be a genuine
/// `python_full_version`/`implementation_version` literal, for which a
/// plain, uncarved passthrough is already correct.
fn convert_specifier(specifier: &VersionSpecifier) -> Result<MatchSpecCondition, Unconvertible> {
    let version = specifier.version();
    match specifier.operator() {
        Operator::GreaterThanEqual => Ok(python_condition(VersionSpec::Range(
            RangeOperator::GreaterEquals,
            match minor_precision(version) {
                Some((major, minor)) => anchor(major, minor)?,
                None => conda_version(version)?,
            },
        ))),
        Operator::LessThan => Ok(python_condition(VersionSpec::Range(
            RangeOperator::Less,
            match minor_precision(version) {
                Some((major, minor)) => anchor(major, minor)?,
                None => conda_version(version)?,
            },
        ))),
        Operator::LessThanEqual => Ok(python_condition(VersionSpec::Range(
            RangeOperator::LessEquals,
            conda_version(version)?,
        ))),
        Operator::GreaterThan => Ok(python_condition(VersionSpec::Range(
            RangeOperator::Greater,
            conda_version(version)?,
        ))),
        Operator::Equal => Ok(python_condition(VersionSpec::Exact(
            EqualityOperator::Equals,
            conda_version(version)?,
        ))),
        Operator::NotEqual => Ok(python_condition(VersionSpec::Exact(
            EqualityOperator::NotEquals,
            conda_version(version)?,
        ))),
        // Anchored two-clause range instead of conda's fuzzy
        // `StartsWith` match, since matchspec's fuzzy-equals syntax is
        // deprecated.
        Operator::EqualStar => match minor_precision(version) {
            Some((major, minor)) => Ok(MatchSpecCondition::And(
                Box::new(python_condition(VersionSpec::Range(
                    RangeOperator::GreaterEquals,
                    anchor(major, minor)?,
                ))),
                Box::new(python_condition(VersionSpec::Range(
                    RangeOperator::Less,
                    anchor(major, minor + 1)?,
                ))),
            )),
            None => Ok(python_condition(VersionSpec::StrictRange(
                StrictRangeOperator::StartsWith,
                StrictVersion::from(conda_version(version)?),
            ))),
        },
        // Not anchored like `==` -- fuzzy `NotStartsWith` is used
        // directly.
        Operator::NotEqualStar => Ok(python_condition(VersionSpec::StrictRange(
            StrictRangeOperator::NotStartsWith,
            StrictVersion::from(conda_version(version)?),
        ))),
        operator @ (Operator::TildeEqual | Operator::ExactEqual) => {
            Err(Unconvertible::UnsupportedOperator {
                key: "python_full_version".to_string(),
                operator: format!("{operator:?}"),
            })
        }
    }
}

/// `version`'s CEP-33 spelling: epoch (if any) prefixed with `!`, release
/// segments dot-joined, then `.{letter}{number}` for a pre-release,
/// `.post{number}` for a post-release, and `.dev{number}` for a dev
/// release -- e.g. PEP 440's `1.0.0rc1` becomes `1.0.0.rc1`, and its
/// `1.0-1` shorthand becomes `1.0.post1`.
///
/// `pub` so `ana-pep508-to-matchspec::version` can reuse this instead of
/// duplicating CEP-33 formatting.
pub fn format_version(version: &PypiVersion) -> String {
    let mut formatted = String::new();
    let epoch = version.epoch();
    if epoch != 0 {
        use std::fmt::Write as _;
        let _ = write!(formatted, "{epoch}!");
    }
    for (index, segment) in version.release().iter().enumerate() {
        if index > 0 {
            formatted.push('.');
        }
        use std::fmt::Write as _;
        let _ = write!(formatted, "{segment}");
    }
    if let Some(pre) = version.pre() {
        use std::fmt::Write as _;
        let _ = write!(formatted, ".{}{}", pre.kind, pre.number);
    }
    if let Some(post) = version.post() {
        use std::fmt::Write as _;
        let _ = write!(formatted, ".post{post}");
    }
    if let Some(dev) = version.dev() {
        use std::fmt::Write as _;
        let _ = write!(formatted, ".dev{dev}");
    }
    formatted
}

/// [`format_version`] then parsed back as a conda `Version` -- the one
/// unavoidable string round-trip (`rattler_conda_types::Version` has no
/// general typed constructor).
fn conda_version(version: &PypiVersion) -> Result<CondaVersion, Unconvertible> {
    let formatted = format_version(version);
    formatted.parse().map_err(
        |source: ParseVersionError| Unconvertible::InvalidVersionLiteral {
            literal: formatted,
            source,
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::str::FromStr;

    use rattler_conda_types::Platform;
    use uv_pep508::Requirement;

    use super::*;
    use crate::known_values_assumption;

    /// `entry`'s marker converted against `subdir`'s known-values
    /// assumption -- the same call a real caller makes.
    fn convert(entry: &str, subdir: Platform) -> Result<Applicability, Unconvertible> {
        let requirement: Requirement = Requirement::from_str(entry).unwrap();
        let assumption = known_values_assumption(subdir).unwrap();
        to_matchspec_condition(requirement.marker, assumption)
    }

    /// A leaf `MatchSpecCondition` for `python<version_spec>`, built the
    /// same way [`python_condition`] does.
    fn python(version_spec: &str) -> MatchSpecCondition {
        python_condition(
            VersionSpec::from_str(version_spec, rattler_conda_types::ParseStrictness::Lenient)
                .unwrap(),
        )
    }

    /// `MatchSpecCondition::And`, for building expected multi-leaf
    /// values -- each `MarkerExpression` leaf converts independently, so
    /// two clauses on the same key (e.g. a `~=` expansion's `>=`/`<`
    /// pair) stay two nested `MatchSpecCondition::MatchSpec` leaves,
    /// never merged into one `VersionSpec::Group`.
    fn and2(a: MatchSpecCondition, b: MatchSpecCondition) -> MatchSpecCondition {
        MatchSpecCondition::And(Box::new(a), Box::new(b))
    }

    /// [`and2`]'s `Or` counterpart.
    fn or2(a: MatchSpecCondition, b: MatchSpecCondition) -> MatchSpecCondition {
        MatchSpecCondition::Or(Box::new(a), Box::new(b))
    }

    mod applicability {
        use super::*;

        #[test]
        fn a_marker_free_requirement_is_always_applicable() {
            assert_eq!(
                convert("requests", Platform::Linux64).unwrap(),
                Applicability::Always
            );
        }

        #[test]
        fn a_marker_that_holds_given_known_values_is_always_applicable() {
            assert_eq!(
                convert(r#"requests; sys_platform == "linux""#, Platform::Linux64).unwrap(),
                Applicability::Always
            );
        }

        #[test]
        fn a_marker_that_cannot_hold_given_known_values_is_never_applicable() {
            assert_eq!(
                convert(r#"requests; sys_platform == "win32""#, Platform::Linux64).unwrap(),
                Applicability::Never
            );
        }

        #[test]
        fn a_pure_free_variable_marker_is_conditionally_applicable() {
            assert_eq!(
                convert(r#"requests; python_version >= "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        #[test]
        fn a_known_and_free_conjunction_collapses_to_just_the_free_part() {
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "linux" and python_version >= "3.9""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        #[test]
        fn a_known_false_and_free_conjunction_is_never_applicable() {
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "win32" and python_version >= "3.9""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Never
            );
        }

        #[test]
        fn a_known_true_or_free_disjunction_is_always_applicable() {
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "linux" or python_version >= "3.9""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Always
            );
        }
    }

    /// Pins the exact operator/version shape `python_version`
    /// canonicalizes to, per the module docs.
    mod python_version_conversion {
        use super::*;

        #[test]
        fn greater_than_equal_gets_the_pre_release_anchor() {
            assert_eq!(
                convert(r#"requests; python_version >= "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        #[test]
        fn less_than_becomes_next_minor_exclusive() {
            // Anchored at `.0a0` so a pre-release build of 3.9 is
            // excluded too, not just final releases.
            assert_eq!(
                convert(r#"requests; python_version < "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python("<3.9.0a0"))
            );
        }

        #[test]
        fn less_than_equal_becomes_next_minor_exclusive_upper_bound() {
            assert_eq!(
                convert(r#"requests; python_version <= "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python("<3.10.0a0"))
            );
        }

        #[test]
        fn greater_than_becomes_next_minor_inclusive_lower_bound() {
            assert_eq!(
                convert(r#"requests; python_version > "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python(">=3.10.0a0"))
            );
        }

        /// Pins the anchored two-clause range `convert_specifier`'s
        /// `EqualStar` arm produces (matchspec's fuzzy-equals syntax is
        /// deprecated, so this doesn't use conda's `StartsWith` match).
        #[test]
        fn equality_becomes_an_anchored_two_clause_range() {
            assert_eq!(
                convert(r#"requests; python_version == "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.9.0a0"), python("<3.10.0a0")))
            );
        }

        #[test]
        fn inequality_becomes_a_fuzzy_minor_exclusion() {
            assert_eq!(
                convert(r#"requests; python_version != "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python("!=3.9.*"))
            );
        }

        /// A bare major literal normalizes to minor `0`, so the bound is
        /// the *next* minor (`3.1`), not the next major -- unlike the
        /// `3.*` glob case below, which is two independent `Version`
        /// leaves, not one `EqualStar` leaf.
        #[test]
        fn single_major_segment_equality_still_converts() {
            assert_eq!(
                convert(r#"requests; python_version == "3""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.0.0a0"), python("<3.1.0a0")))
            );
        }

        #[test]
        fn reversed_comparison_operand_order_still_converts() {
            assert_eq!(
                convert(r#"requests; "3.9" <= python_version"#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        /// Two separate `Version` clauses in the same DNF `and`-group
        /// convert to two nested leaves, see [`and2`]'s docs; both
        /// boundaries get the `.0a0` anchor.
        #[test]
        fn major_glob_equality_converts_to_a_range() {
            assert_eq!(
                convert(r#"requests; python_version == "3.*""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.0.0a0"), python("<4.0.0a0")))
            );
        }

        /// `~=` is pre-expanded by `uv_pep508` into a plain range before
        /// this crate sees an operator; both boundaries get the `.0a0`
        /// anchor.
        #[test]
        fn compatible_release_is_pre_expanded_and_converts() {
            assert_eq!(
                convert(r#"requests; python_version ~= "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.9.0a0"), python("<4.0.0a0")))
            );
        }

        /// `uv_pep508` expands the literal into a plain range directly;
        /// both boundaries get the `.0a0` anchor.
        #[test]
        fn in_marker_converts_to_a_bounded_range() {
            assert_eq!(
                convert(
                    r#"requests; python_version in "3.9 3.10""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(and2(python(">=3.9.0a0"), python("<3.11.0a0")))
            );
        }

        /// `uv_pep508` expands "not in a range" via De Morgan into an
        /// `Or` of the two excluded tails as independent single-leaf
        /// clauses -- [`version_in_condition`]'s `Membership::NotIn` arm
        /// is never actually reached for this input.
        #[test]
        fn not_in_marker_converts_to_an_excluded_range() {
            assert_eq!(
                convert(
                    r#"requests; python_version not in "3.9 3.10""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(or2(python("<3.9.0a0"), python(">=3.11.0a0")))
            );
        }
    }

    mod full_version_conversion {
        use super::*;

        #[test]
        fn python_full_version_passes_through_directly() {
            assert_eq!(
                convert(
                    r#"requests; python_full_version >= "3.9.5""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(python(">=3.9.5"))
            );
        }

        #[test]
        fn exact_full_version_equality_is_not_fuzzy() {
            // uv_pep508 emits a plain `Equal`, not `EqualStar`, when the
            // literal has no ambiguity to collapse.
            assert_eq!(
                convert(
                    r#"requests; python_full_version == "3.9.5""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(python("==3.9.5"))
            );
        }

        /// `implementation_version` converts identically to
        /// `python_full_version` -- CPython is the only supported
        /// interpreter.
        #[test]
        fn implementation_version_converts_the_same_as_python_full_version() {
            assert_eq!(
                convert(
                    r#"requests; implementation_version >= "3.9.5""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(python(">=3.9.5"))
            );
        }

        /// `uv_pep508` 0.12.6 silently drops a marker version literal's
        /// pre/post/dev segments during parsing (confirmed via `.pre()`/
        /// `.release()`: `python_full_version >= "3.9.0rc1"` parses to
        /// `version()` `3.9.0`, `pre()` `None`) -- this crate never sees
        /// the `rc1` to preserve or reject. Pinned here so a future
        /// `uv_pep508` bump that stops dropping it is a loud test
        /// failure, not a silent behavior change.
        ///
        /// Once dropped, the literal (`release=[3, 9]`) is
        /// indistinguishable from a genuine `python_version` boundary
        /// (see [`minor_precision`]'s docs), so it gets the same `.0a0`
        /// anchor -- an accepted, narrow cost on top of an already-lossy
        /// upstream conversion, since a marker pinning a specific
        /// pre-release of a full version is rare.
        #[test]
        fn prerelease_literal_converts_without_any_allow_pre_concept() {
            assert_eq!(
                convert(
                    r#"requests; python_full_version >= "3.9.0rc1""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }
    }

    /// Equivalence oracle: checks the [`Applicability`]
    /// [`to_matchspec_condition`] produces against an
    /// independently-computed PEP 440/508 ground truth for a sweep of
    /// candidate interpreter versions, proving semantic equivalence
    /// rather than agreement with one hand-picked expected string.
    ///
    /// No network fetch is needed for the `in`/`not in` cases -- see
    /// [`super::version_in_condition`]'s docs.
    mod equivalence_oracle {
        use super::*;

        /// A `python_full_version` sweep crossing every boundary these
        /// tests care about: pre-3.9, 3.9's own pre-/dev-/post-release
        /// stages, 3.9.0 itself, later patches, the 3.10 boundary, a
        /// double-digit minor, and a major bump.
        const CANDIDATES: &[&str] = &[
            "2.7.18",
            "3.7.9",
            "3.8.0",
            "3.8.16",
            "3.9.0.dev0",
            "3.9.0a0",
            "3.9.0a1",
            "3.9.0b1",
            "3.9.0rc1",
            "3.9.0",
            "3.9.0.post1",
            "3.9.1",
            "3.9.20",
            "3.10.0a0",
            "3.10.0",
            "3.10.5",
            "3.13.0",
            "3.13.2",
            "4.0.0a0",
            "4.0.0",
        ];

        /// `full_version`'s `major.minor`, truncated the way a real
        /// interpreter's `python_version` always is relative to its
        /// `python_full_version` (`sys.version_info[:2]`) -- taken on
        /// faith as this oracle's one definitional assumption.
        fn python_version_of(full_version: &str) -> PypiVersion {
            let version = PypiVersion::from_str(full_version).unwrap();
            let release = version.release();
            PypiVersion::from_str(&format!(
                "{}.{}",
                release.first().copied().unwrap_or(0),
                release.get(1).copied().unwrap_or(0)
            ))
            .unwrap()
        }

        /// The independent ground truth an ordered comparator
        /// (`>=`/`>`/`<=`/`<`) against `key` holds for `full_version`,
        /// computed directly from PEP 440 version comparison --
        /// deliberately *not* `uv_pep508::MarkerTree::evaluate()`, which
        /// has this same missing-pre-release-anchor gap for
        /// `python_version` internally (confirmed against Python's own
        /// `packaging.markers.Marker.evaluate`, the PEP 508 reference:
        /// `Marker("python_version == \"3.9\"").evaluate(...)` is `True`
        /// for `python_full_version == "3.9.0.dev0"`, while
        /// `uv_pep508`'s `evaluate()` on the same environment returns
        /// `false`). [`to_matchspec_condition`] (the thing under test)
        /// never calls `evaluate()`, so that upstream gap doesn't affect
        /// production behavior, but it does mean `evaluate()` can't be
        /// trusted as this oracle's ground truth either.
        ///
        /// `python_version`'s own comparison is against
        /// [`python_version_of`]`(full_version)`;
        /// `python_full_version`/`implementation_version` compare
        /// `full_version` directly, untruncated.
        fn pip_ordered_reference(
            key: &str,
            operator: &str,
            literal: &str,
            full_version: &str,
        ) -> bool {
            let literal_version = PypiVersion::from_str(literal).unwrap();
            let subject = if key == "python_version" {
                python_version_of(full_version)
            } else {
                PypiVersion::from_str(full_version).unwrap()
            };
            match operator {
                ">=" => subject >= literal_version,
                ">" => subject > literal_version,
                "<=" => subject <= literal_version,
                "<" => subject < literal_version,
                other => unreachable!("not an ordered comparator: {other:?}"),
            }
        }

        /// Whether `condition` holds for `python`, via a recursive walk
        /// over the typed [`MatchSpecCondition`] tree.
        fn condition_holds(condition: &MatchSpecCondition, python: &CondaVersion) -> bool {
            match condition {
                MatchSpecCondition::And(a, b) => {
                    condition_holds(a, python) && condition_holds(b, python)
                }
                MatchSpecCondition::Or(a, b) => {
                    condition_holds(a, python) || condition_holds(b, python)
                }
                MatchSpecCondition::MatchSpec(spec) => spec
                    .version
                    .as_ref()
                    .is_none_or(|version_spec| version_spec.matches(python)),
            }
        }

        /// [`Applicability`]'s own "does this hold" question, for a given
        /// `python` candidate.
        fn applicability_holds(applicability: &Applicability, python: &CondaVersion) -> bool {
            match applicability {
                Applicability::Always => true,
                Applicability::Never => false,
                Applicability::Conditionally(condition) => condition_holds(condition, python),
            }
        }

        /// Converts `key <op> "literal"` once, then asserts
        /// [`applicability_holds`] agrees with [`pip_ordered_reference`]
        /// for every one of `candidates`.
        fn assert_ordered_comparator_agrees_with_pip(
            key: &str,
            operator: &str,
            literal: &str,
            candidates: &[&str],
        ) {
            let marker = format!(r#"{key} {operator} "{literal}""#);
            let requirement: Requirement =
                Requirement::from_str(&format!("requests; {marker}")).unwrap();
            let assumption = known_values_assumption(Platform::Linux64).unwrap();
            let applicability = to_matchspec_condition(requirement.marker, assumption).unwrap();
            for candidate in candidates {
                let pip_result = pip_ordered_reference(key, operator, literal, candidate);
                let python: CondaVersion = candidate.parse().unwrap();
                let matchspec_result = applicability_holds(&applicability, &python);
                assert_eq!(
                    pip_result, matchspec_result,
                    "marker {marker:?} candidate {candidate:?}: pip says {pip_result}, \
                     matchspec says {matchspec_result}"
                );
            }
        }

        /// The independent ground truth an equality comparator
        /// (`==`/`!=`, including the star-glob form) holds for
        /// `full_version` against `key`'s literal -- computed via
        /// `uv_pep440`'s own [`VersionSpecifier::contains`], not marker
        /// evaluation (see [`pip_ordered_reference`]'s docs) or this
        /// crate's own [`super::convert_specifier`] under test.
        fn pip_equality_reference(
            key: &str,
            operator: &str,
            literal: &str,
            full_version: &str,
        ) -> bool {
            let subject = if key == "python_version" {
                python_version_of(full_version)
            } else {
                PypiVersion::from_str(full_version).unwrap()
            };
            let specifier = VersionSpecifier::from_str(&format!("{operator}{literal}")).unwrap();
            specifier.contains(&subject)
        }

        /// The independent ground truth `python_version in/not in
        /// "<literal>"` holds for `full_version` -- a plain substring
        /// test against `literal`, PEP 508's own definition,
        /// deliberately not the range-arithmetic shape `uv_pep508`'s
        /// canonicalization assumes.
        fn pip_membership_reference(negated: bool, literal: &str, full_version: &str) -> bool {
            let subject = python_version_of(full_version).to_string();
            let contains = literal.contains(&subject);
            if negated {
                !contains
            } else {
                contains
            }
        }

        /// [`assert_ordered_comparator_agrees_with_pip`]'s `==`/`!=`
        /// counterpart.
        fn assert_equality_agrees_with_pip(
            key: &str,
            operator: &str,
            literal: &str,
            candidates: &[&str],
        ) {
            let marker = format!(r#"{key} {operator} "{literal}""#);
            let requirement: Requirement =
                Requirement::from_str(&format!("requests; {marker}")).unwrap();
            let assumption = known_values_assumption(Platform::Linux64).unwrap();
            let applicability = to_matchspec_condition(requirement.marker, assumption).unwrap();
            for candidate in candidates {
                let pip_result = pip_equality_reference(key, operator, literal, candidate);
                let python: CondaVersion = candidate.parse().unwrap();
                let matchspec_result = applicability_holds(&applicability, &python);
                assert_eq!(
                    pip_result, matchspec_result,
                    "marker {marker:?} candidate {candidate:?}: pip says {pip_result}, \
                     matchspec says {matchspec_result}"
                );
            }
        }

        /// [`assert_ordered_comparator_agrees_with_pip`]'s `in`/`not in`
        /// counterpart.
        fn assert_membership_agrees_with_pip(negated: bool, literal: &str, candidates: &[&str]) {
            let keyword = if negated { "not in" } else { "in" };
            let marker = format!(r#"python_version {keyword} "{literal}""#);
            let requirement: Requirement =
                Requirement::from_str(&format!("requests; {marker}")).unwrap();
            let assumption = known_values_assumption(Platform::Linux64).unwrap();
            let applicability = to_matchspec_condition(requirement.marker, assumption).unwrap();
            for candidate in candidates {
                let pip_result = pip_membership_reference(negated, literal, candidate);
                let python: CondaVersion = candidate.parse().unwrap();
                let matchspec_result = applicability_holds(&applicability, &python);
                assert_eq!(
                    pip_result, matchspec_result,
                    "marker {marker:?} candidate {candidate:?}: pip says {pip_result}, \
                     matchspec says {matchspec_result}"
                );
            }
        }

        mod python_version {
            use super::*;

            #[test]
            fn greater_than_equal_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip(
                    "python_version",
                    ">=",
                    "3.9",
                    CANDIDATES,
                );
            }

            #[test]
            fn greater_than_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip("python_version", ">", "3.9", CANDIDATES);
            }

            #[test]
            fn less_than_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip("python_version", "<", "3.9", CANDIDATES);
            }

            #[test]
            fn less_than_equal_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip(
                    "python_version",
                    "<=",
                    "3.9",
                    CANDIDATES,
                );
            }

            #[test]
            fn crossing_the_double_digit_minor_boundary_agrees_with_pip() {
                assert_ordered_comparator_agrees_with_pip(
                    "python_version",
                    ">=",
                    "3.13",
                    CANDIDATES,
                );
            }
        }

        mod full_version {
            use super::*;

            /// `"3.9.1"` has a nonzero patch segment, so
            /// [`minor_precision`]'s anchor gate stays `None` --
            /// validates the plain-passthrough path, not the 2-segment
            /// tradeoff the test below documents.
            #[test]
            fn greater_than_equal_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip(
                    "python_full_version",
                    ">=",
                    "3.9.1",
                    CANDIDATES,
                );
            }

            #[test]
            fn less_than_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip(
                    "python_full_version",
                    "<",
                    "3.9.1",
                    CANDIDATES,
                );
            }

            #[test]
            fn implementation_version_agrees_with_pip_across_every_candidate() {
                assert_ordered_comparator_agrees_with_pip(
                    "implementation_version",
                    ">=",
                    "3.9.1",
                    CANDIDATES,
                );
            }

            /// Documents the known tradeoff [`minor_precision`]'s docs
            /// describe: a 2-segment `python_full_version` literal is
            /// indistinguishable from a `python_version` origin here, so
            /// it gets the same `.0a0` anchor -- which disagrees with
            /// pip for pre-release candidates sitting in the gap between
            /// the anchor and the literal's true boundary. This sweeps
            /// only the candidates outside that gap.
            #[test]
            fn two_segment_literal_agrees_with_pip_outside_the_known_tradeoff_gap() {
                let candidates: Vec<&str> = CANDIDATES
                    .iter()
                    .copied()
                    .filter(|candidate| {
                        !matches!(
                            *candidate,
                            "3.9.0.dev0" | "3.9.0a0" | "3.9.0a1" | "3.9.0b1" | "3.9.0rc1"
                        )
                    })
                    .collect();
                assert_ordered_comparator_agrees_with_pip(
                    "python_full_version",
                    ">=",
                    "3.9",
                    &candidates,
                );
            }
        }

        /// [`equivalence_oracle::python_version`]/[`equivalence_oracle::full_version`]'s
        /// `==`/`!=` counterpart, via [`assert_equality_agrees_with_pip`].
        mod equality {
            use super::*;

            mod python_version {
                use super::*;

                #[test]
                fn equality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip("python_version", "==", "3.9", CANDIDATES);
                }

                #[test]
                fn inequality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip("python_version", "!=", "3.9", CANDIDATES);
                }

                #[test]
                fn major_glob_equality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip("python_version", "==", "3.*", CANDIDATES);
                }

                /// Same, `!=` row.
                #[test]
                fn major_glob_inequality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip("python_version", "!=", "3.*", CANDIDATES);
                }
            }

            mod full_version {
                use super::*;

                #[test]
                fn equality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip(
                        "python_full_version",
                        "==",
                        "3.9.1",
                        CANDIDATES,
                    );
                }

                #[test]
                fn inequality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip(
                        "python_full_version",
                        "!=",
                        "3.9.1",
                        CANDIDATES,
                    );
                }

                /// `"3.9.*"` is, like the 2-segment literal case above,
                /// indistinguishable at `convert_specifier`'s layer from
                /// a `python_version`-precision literal -- inherits the
                /// same accepted tradeoff, filtered the same way.
                #[test]
                fn glob_equality_agrees_with_pip_across_every_candidate() {
                    let candidates: Vec<&str> = CANDIDATES
                        .iter()
                        .copied()
                        .filter(|candidate| {
                            !matches!(
                                *candidate,
                                "3.9.0.dev0" | "3.9.0a0" | "3.9.0a1" | "3.9.0b1" | "3.9.0rc1"
                            )
                        })
                        .collect();
                    assert_equality_agrees_with_pip(
                        "python_full_version",
                        "==",
                        "3.9.*",
                        &candidates,
                    );
                }

                /// Same tradeoff, `!=` row.
                #[test]
                fn glob_inequality_agrees_with_pip_across_every_candidate() {
                    let candidates: Vec<&str> = CANDIDATES
                        .iter()
                        .copied()
                        .filter(|candidate| {
                            !matches!(
                                *candidate,
                                "3.9.0.dev0" | "3.9.0a0" | "3.9.0a1" | "3.9.0b1" | "3.9.0rc1"
                            )
                        })
                        .collect();
                    assert_equality_agrees_with_pip(
                        "python_full_version",
                        "!=",
                        "3.9.*",
                        &candidates,
                    );
                }

                #[test]
                fn implementation_version_equality_agrees_with_pip_across_every_candidate() {
                    assert_equality_agrees_with_pip(
                        "implementation_version",
                        "==",
                        "3.9.1",
                        CANDIDATES,
                    );
                }
            }
        }

        /// Parametrized across the 3 literal separator styles (space,
        /// comma, comma-plus-space) via [`assert_membership_agrees_with_pip`].
        ///
        /// The two comma-bearing styles are `#[ignore]`d, not deleted or
        /// rewritten: the disagreement traces to a confirmed `uv_pep508`
        /// parse-time bug (see below), not an oracle bug.
        mod membership {
            use super::*;

            #[test]
            fn space_separated_in_agrees_with_pip_across_every_candidate() {
                assert_membership_agrees_with_pip(false, "3.8 3.9 3.13", CANDIDATES);
            }

            #[test]
            fn space_separated_not_in_agrees_with_pip_across_every_candidate() {
                assert_membership_agrees_with_pip(true, "3.8 3.9 3.13", CANDIDATES);
            }

            /// Root cause (`uv_pep508`'s `marker/parse.rs`, 0.12.6):
            /// `parse_version_in_expr` tokenizes a `python_version in
            /// "<literal>"` literal by splitting on whitespace only,
            /// then parses each token as a standalone PEP 440 version. A
            /// comma-separated literal with no surrounding whitespace
            /// (`"3.8,3.9,3.13"`) is one token that fails to parse as a
            /// version, so the *entire* clause silently drops to
            /// `MarkerTree::TRUE` before this crate ever sees it --
            /// every candidate's result becomes
            /// `Applicability::Always`, which
            /// [`super::assert_membership_agrees_with_pip`] catches as a
            /// disagreement with pip.
            ///
            /// Reported upstream as
            /// [astral-sh/uv#21310](https://github.com/astral-sh/uv/issues/21310)
            /// (open, fix proposed in
            /// [astral-sh/uv#21311](https://github.com/astral-sh/uv/pull/21311));
            /// unignore once fixed upstream.
            mod comma_separator_silently_drops_the_clause {
                use super::*;

                #[test]
                #[ignore = "known bug, tracked upstream at astral-sh/uv#21310 (open, fix proposed in \
                            astral-sh/uv#21311): uv_pep508 0.12.6 silently drops a \
                            comma-separated `python_version in/not in` literal to \
                            MarkerTree::TRUE at parse time (comma isn't valid PEP 440 version \
                            syntax and the tokenizer only splits on whitespace); unignore once \
                            fixed upstream -- see this module's docs"]
                fn comma_separated_in_agrees_with_pip_across_every_candidate() {
                    assert_membership_agrees_with_pip(false, "3.8,3.9,3.13", CANDIDATES);
                }

                #[test]
                #[ignore = "known bug, tracked upstream at astral-sh/uv#21310 (open, fix proposed in \
                            astral-sh/uv#21311): uv_pep508 0.12.6 silently drops a \
                            comma-separated `python_version in/not in` literal to \
                            MarkerTree::TRUE at parse time (comma isn't valid PEP 440 version \
                            syntax and the tokenizer only splits on whitespace); unignore once \
                            fixed upstream -- see this module's docs"]
                fn comma_space_separated_in_agrees_with_pip_across_every_candidate() {
                    assert_membership_agrees_with_pip(false, "3.8, 3.9, 3.13", CANDIDATES);
                }

                #[test]
                #[ignore = "known bug, tracked upstream at astral-sh/uv#21310 (open, fix proposed in \
                            astral-sh/uv#21311): uv_pep508 0.12.6 silently drops a \
                            comma-separated `python_version in/not in` literal to \
                            MarkerTree::TRUE at parse time (comma isn't valid PEP 440 version \
                            syntax and the tokenizer only splits on whitespace); unignore once \
                            fixed upstream -- see this module's docs"]
                fn comma_separated_not_in_agrees_with_pip_across_every_candidate() {
                    assert_membership_agrees_with_pip(true, "3.8,3.9,3.13", CANDIDATES);
                }

                #[test]
                #[ignore = "known bug, tracked upstream at astral-sh/uv#21310 (open, fix proposed in \
                            astral-sh/uv#21311): uv_pep508 0.12.6 silently drops a \
                            comma-separated `python_version in/not in` literal to \
                            MarkerTree::TRUE at parse time (comma isn't valid PEP 440 version \
                            syntax and the tokenizer only splits on whitespace); unignore once \
                            fixed upstream -- see this module's docs"]
                fn comma_space_separated_not_in_agrees_with_pip_across_every_candidate() {
                    assert_membership_agrees_with_pip(true, "3.8, 3.9, 3.13", CANDIDATES);
                }
            }
        }
    }

    /// `and`/`or` structure is preserved via `MatchSpecCondition::And`/`Or`
    /// directly, never a formatted-then-reparsed string.
    mod combinators {
        use super::*;

        #[test]
        fn combined_marker_preserves_and_or_structure() {
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "win32" and python_version >= "3.9""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Never
            );
            // A build targeting an unknown platform would keep both
            // sides here.
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "win32" and python_version >= "3.9""#,
                    Platform::Win64
                )
                .unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        /// `to_dnf()`'s leaf order follows the BDD's canonical variable
        /// ordering, not source-text order -- `implementation_version`
        /// orders before `python_full_version` here regardless of which
        /// appears first in the marker string. The 2-segment
        /// `implementation_version` literal gets the same `.0a0` anchor
        /// a `python_version` boundary would, per [`minor_precision`]'s
        /// docs.
        #[test]
        fn conjunction_of_two_free_variable_clauses_preserves_and() {
            assert_eq!(
                convert(
                    r#"requests; python_version >= "3.9" and implementation_version < "3.12""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(and2(python("<3.12.0a0"), python(">=3.9.0a0")))
            );
        }
    }

    mod unconvertible {
        use super::*;

        #[test]
        fn platform_release_has_no_matchspec_equivalent() {
            let err = convert(
                r#"requests; platform_release == "5.10.0""#,
                Platform::Linux64,
            )
            .unwrap_err();
            assert!(
                matches!(err, Unconvertible::NoMatchspecEquivalent { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn platform_version_has_no_matchspec_equivalent() {
            let err = convert(
                r#"requests; platform_version == "5.10.0-generic""#,
                Platform::Linux64,
            )
            .unwrap_err();
            assert!(
                matches!(err, Unconvertible::NoMatchspecEquivalent { .. }),
                "{err:?}"
            );
        }

        /// A `platform_release` clause alongside a free-variable clause
        /// still fails -- the free variable alone doesn't rescue a
        /// marker that also depends on a key with no matchspec
        /// equivalent.
        #[test]
        fn excluded_key_combined_with_the_free_variable_is_unconvertible() {
            let err = convert(
                r#"requests; platform_release == "5.10.0" and python_version >= "3.9""#,
                Platform::Linux64,
            )
            .unwrap_err();
            assert!(
                matches!(err, Unconvertible::NoMatchspecEquivalent { .. }),
                "{err:?}"
            );
        }
    }

    /// A confirmed upstream bug in `uv_pep508` 0.12.6 (still present on
    /// `uv`'s `main` branch as of this writing; reported as
    /// [astral-sh/uv#21309](https://github.com/astral-sh/uv/issues/21309),
    /// closed "not planned"): `"<literal>" in <version-key>` /
    /// `"<literal>" not in <version-key>` (the literal on the *left* of
    /// `in`/`not in`, against a *version* marker key) silently parses to
    /// `MarkerTree::TRUE` instead of a real, environment-dependent
    /// expression.
    ///
    /// Verified against `packaging` 26.3, which treats this as an
    /// ordinary substring test (`lhs_value in rhs_value`), not a
    /// tautology:
    ///
    /// ```text
    /// >>> Marker('"3.11" in python_version').evaluate({"python_version": "3.9"})
    /// False
    /// >>> Marker('"3.11" in python_version').evaluate({"python_version": "3.11"})
    /// True
    /// ```
    ///
    /// Root cause, in `uv_pep508`'s own `marker/parse.rs` (0.12.6):
    /// `parse_marker_value`'s `MarkerValue::QuotedString` arm dispatches
    /// the reversed form of a version-key comparison to
    /// `parse_inverted_version_expr`, which only knows how to invert an
    /// *ordinary* comparison operator (`==`, `<`, etc.) -- there is no
    /// PEP 440 equivalent for `in`/`not in`, so it returns `None`, and
    /// its callers (`parse_marker_and`/`parse_marker_or`) treat a `None`
    /// leaf as "nothing to add to this and/or chain," not a parse error
    /// -- so a marker consisting of *only* this shape parses to
    /// `MarkerTree::TRUE`, and inside a larger `and`/`or` expression the
    /// clause vanishes entirely. The analogous reversed form for a
    /// *string* key (`sys_platform`, etc.) doesn't have this problem --
    /// `in`/`not in` against a string never needs a PEP 440 operator to
    /// invert; the tests below that exercise `sys_platform` are passing
    /// contrasts, not part of the bug.
    ///
    /// Because the bug is at `uv_pep508`'s own *parse* time, it is
    /// unrecoverable at this crate's layer: [`to_matchspec_condition`]
    /// only ever receives an already-parsed `MarkerTree`, bit-for-bit
    /// identical to what a genuinely marker-free package produces.
    ///
    /// The `#[ignore]`d tests below assert the behavior a correct
    /// implementation should have (verified against `packaging` above)
    /// and are **intended to stay `#[ignore]`d indefinitely**, since
    /// astral-sh/uv#21309 was closed "not planned" -- kept (not deleted)
    /// so they're what should flip green if this crate ever adds its
    /// own local guard upstream of `restrict()`.
    mod reversed_membership_operand_order_asymmetry {
        use super::*;

        /// Should convert the same as the forward form (see
        /// `python_version_conversion::in_marker_converts_to_a_bounded_range`);
        /// the upstream bug (module docs) actually returns
        /// `Applicability::Always`.
        #[test]
        #[ignore = "upstream bug, not planned to be fixed: uv_pep508 silently drops the \
                    reversed \"<literal>\" in <version-key> form at parse time (confirmed \
                    against uv_pep508 0.12.6 and uv main @ 0697445c; reported upstream as \
                    astral-sh/uv#21309, closed \"not planned\"), so this crate receives \
                    MarkerTree::TRUE with no way to recover the real constraint -- intended \
                    to stay ignored forever absent a local workaround; see this module's docs"]
        fn reversed_in_python_version_should_convert_like_the_forward_form() {
            assert_eq!(
                convert(r#"requests; "3.11" in python_version"#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.11.0a0"), python("<3.12.0a0")))
            );
        }

        /// Same bug, `not in` direction: the forward form converts to a
        /// fuzzy minor exclusion (see
        /// `python_version_conversion::inequality_becomes_a_fuzzy_minor_exclusion`).
        #[test]
        #[ignore = "upstream bug, not planned to be fixed: uv_pep508 silently drops the \
                    reversed \"<literal>\" not in <version-key> form at parse time (confirmed \
                    against uv_pep508 0.12.6 and uv main @ 0697445c; reported upstream as \
                    astral-sh/uv#21309, closed \"not planned\"), so this crate receives \
                    MarkerTree::TRUE with no way to recover the real constraint -- intended \
                    to stay ignored forever absent a local workaround; see this module's docs"]
        fn reversed_not_in_python_version_should_convert_like_the_forward_form() {
            assert_eq!(
                convert(
                    r#"requests; "3.11" not in python_version"#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(python("!=3.11.*"))
            );
        }

        /// Contrast: the forward operand order converts correctly,
        /// proving the gap is specific to operand order, not `in`/`not
        /// in` in general.
        #[test]
        fn forward_in_python_version_still_converts_correctly() {
            assert_eq!(
                convert(r#"requests; python_version in "3.11""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.11.0a0"), python("<3.12.0a0")))
            );
        }

        /// Contrast: for a string key the reversed operand order still
        /// parses to a real expression (here,
        /// [`Unconvertible::NoMatchspecEquivalent`]), not a
        /// silently-dropped clause -- the bug is specific to version
        /// keys.
        #[test]
        fn reversed_in_sys_platform_still_produces_a_real_expression() {
            let err = convert(r#"requests; "nux" in sys_platform"#, Platform::Linux64).unwrap_err();
            assert!(
                matches!(
                    &err,
                    Unconvertible::NoMatchspecEquivalent { key } if key == "sys_platform"
                ),
                "{err:?}"
            );
        }

        /// Same contrast for `not in`.
        #[test]
        fn reversed_not_in_sys_platform_still_produces_a_real_expression() {
            let err =
                convert(r#"requests; "nux" not in sys_platform"#, Platform::Linux64).unwrap_err();
            assert!(
                matches!(
                    &err,
                    Unconvertible::NoMatchspecEquivalent { key } if key == "sys_platform"
                ),
                "{err:?}"
            );
        }

        /// The dropped clause vanishes so completely inside a
        /// conjunction that the other clause's own truth value alone
        /// determines the whole marker's [`Applicability`] -- correct
        /// behavior mirrors
        /// `applicability::a_known_and_free_conjunction_collapses_to_just_the_free_part`.
        #[test]
        #[ignore = "upstream bug, not planned to be fixed: uv_pep508 silently drops the \
                    reversed \"<literal>\" in <version-key> form at parse time (confirmed \
                    against uv_pep508 0.12.6 and uv main @ 0697445c; reported upstream as \
                    astral-sh/uv#21309, closed \"not planned\"), so the whole conjunction \
                    loses its python_version half before this crate ever sees it -- intended \
                    to stay ignored forever absent a local workaround; see this module's docs"]
        fn reversed_in_python_version_should_survive_a_surrounding_conjunction() {
            // A correct implementation keeps the python_version half;
            // the actual (buggy) result is Applicability::Always.
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "linux" and "3.11" in python_version"#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(and2(python(">=3.11.0a0"), python("<3.12.0a0")))
            );
            // Unaffected by the bug: False AND anything is False.
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "win32" and "3.11" in python_version"#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Never
            );
        }
    }
}
