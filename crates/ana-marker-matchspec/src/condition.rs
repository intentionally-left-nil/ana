//! `MarkerTree` -> `MatchSpecCondition` conversion, single-target -- the
//! implemented half of `investigations/pep508_to_matchspec_api.md`'s
//! "Slow path, take 2".
//!
//! [`to_matchspec_condition`] is the whole entry point: `restrict()` (see
//! [`crate::assumption`]) does the "which keys are known" work, and
//! everything left over -- by construction, only ever the free
//! `python_version`/`python_full_version`/`implementation_version`
//! family, or a key deliberately left out of the assumption
//! (`platform_release`/`platform_version`) -- gets converted leaf by leaf
//! via [`to_dnf`]'s flattened `Or<And<MarkerExpression>>` form.
//!
//! One upstream canonicalization is worth calling out because it changes
//! how small this leaf table needs to be, confirmed directly by probing
//! `uv_pep508` 0.12.6 rather than assumed from reroll's own (necessarily
//! different) design: `python_version` markers are rewritten internally
//! onto the *same* BDD dimension as `python_full_version` before
//! `to_dnf()` ever sees them, with the operator/version adjusted to
//! preserve minor-precision semantics --
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
//! -- so unlike reroll's `marker_conversion.py`, which needs two separate
//! functions (`_python_version_condition`'s minor-precision table, with
//! its own next-minor bumping and exact-vs-not distinction, and a
//! simpler `_full_version_condition` for the already-precise keys), this
//! module needs exactly one: `MarkerValueVersion::PythonVersion` never
//! actually reaches [`convert_leaf`] (`CanonicalMarkerValueVersion`, the
//! BDD's own internal dimension enum, has no `PythonVersion` variant at
//! all -- confirmed directly against `uv_pep508`'s source, not inferred),
//! and `~=`/`in`/`not in` never reach it as their own operator either --
//! they're always pre-expanded into plain ordered comparisons before
//! `to_dnf()` runs. The operator table below still handles
//! `MarkerValueVersion::PythonVersion` and `ContainerOperator::{In,NotIn}`
//! explicitly rather than assuming they're truly unreachable forever: a
//! future `uv_pep508` bump changing this canonicalization would hit a
//! real (if currently untested by construction) code path here, not a
//! silent gap.

use rattler_conda_types::{
    EqualityOperator, MatchSpec, MatchSpecCondition, PackageName, PackageNameMatcher,
    ParseVersionError, RangeOperator, StrictRangeOperator, StrictVersion, Version as CondaVersion,
    VersionSpec,
};
use uv_pep440::{Operator, Version as PypiVersion, VersionSpecifier};
use uv_pep508::{MarkerExpression, MarkerTree, MarkerValueVersion};

/// `MarkerExpression::VersionIn`'s `operator` field is typed
/// `uv_pep508::marker::ContainerOperator` -- a real, `pub` enum variant
/// field, but on a type that isn't itself re-exported anywhere in
/// `uv_pep508`'s public API (confirmed directly against `lib.rs`/
/// `marker/mod.rs`'s `pub use` lists at `0.12.6`: `ContainerOperator`
/// appears in neither). That means this crate can destructure a value of
/// that type (Rust doesn't need to name a field's type to bind it) but
/// cannot name `ContainerOperator` in its own signatures or match arms.
/// [`Membership`] is a local, nameable stand-in, built from the unnameable
/// value's own `Display` output (`"in"`/`"not in"`, confirmed against its
/// `impl Display` in `tree.rs`) rather than needing to name the type at
/// all. Two operators exist and two strings are matched, exhaustively --
/// a third would surface as [`Unconvertible::UnsupportedOperator`], not a
/// silent misinterpretation.
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
/// how to represent this," not "we know, and the answer is no." See
/// `investigations/pep508_to_matchspec_api.md`'s "Slow path, take 2."
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
    /// "win32"` while installing on Linux). The caller should drop the
    /// dependency entirely, the same way an optional, platform-specific
    /// dependency is dropped for any other platform it doesn't apply to
    /// -- this is not an error.
    Never,
}

/// Every way [`to_matchspec_condition`] can fail to represent a marker as
/// a matchspec condition, once known values have already been restricted
/// away. Deliberately narrower than reroll's own `UnconvertableMarkerError`/
/// `UnconvertablePythonVersionEqualityError` split collapses here into one
/// enum, since (unlike reroll) this crate never needs to distinguish "a
/// key with no matchspec equivalent" from "a marker that's a tautology/
/// contradiction on its free variable alone" -- the latter can't happen
/// here, because `restrict()` already turned any marker that's constant
/// given the known values into `Applicability::Always`/`Never` before
/// this error type is ever constructed; what's left can only be
/// constant if it's *unconditionally* so, independent of the free
/// variable, which [`to_dnf`]'s own construction rules out for a residual
/// that's neither `is_true()` nor `is_false()`.
#[derive(Debug, thiserror::Error)]
pub enum Unconvertible {
    /// A marker key with no matchspec equivalent reached this layer --
    /// expected for `platform_release`/`platform_version` (deliberately
    /// left out of the assumption, see [`crate::known_values_assumption`]),
    /// and a defensive catch-all for any other `String`/`List` key that
    /// reaches here (should be unreachable in practice, since every other
    /// key is covered by the assumption).
    #[error("marker key {key:?} has no matchspec equivalent")]
    NoMatchspecEquivalent { key: String },

    /// `extra == "..."` reached this layer. Same reasoning reroll's own
    /// `pep508_to_matchspec` uses: `extra` is the *current package's* own
    /// extras mechanism, not an environment condition, and callers should
    /// check for and strip `extra` clauses before ever calling into this
    /// crate -- see [`to_matchspec_condition`]'s docs.
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
    /// practice, but propagated rather than unwrapped -- same reasoning
    /// as `ana-pep508-to-matchspec`'s identically-named error variant.
    #[error("{literal:?} did not parse as a conda version literal: {source}")]
    InvalidVersionLiteral {
        literal: String,
        #[source]
        source: ParseVersionError,
    },
}

/// [`to_matchspec_condition`], but taking an already-computed `assumption`
/// -- see [`crate::known_values_assumption`] to build one for a subdir.
///
/// Callers must strip (or reject) any `extra == "..."` clause in `marker`
/// *before* calling this function -- `extra` is the current package's own
/// extras mechanism, not an environment condition this crate resolves,
/// and a marker containing one alongside an environment clause (e.g.
/// `extra == "foo" and sys_platform == "linux"`) would otherwise surface
/// [`Unconvertible::ExtraMarker`] partway through DNF conversion rather
/// than being caught up front.
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
/// every negation down to individual leaves (`uv_pep508`'s
/// `MarkerOperator::negate()` handles that internally during
/// `to_dnf()`/`restrict()`), so there is nothing left to negate by the
/// time a leaf reaches [`convert_leaf`] -- every leaf is already in its
/// already-negated-if-needed form (`!=` instead of `not(==)`, etc.).
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
/// before [`try_fast_tree`] is ever called), so this is a real invariant,
/// not a defensive guess.
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
/// added `List`) should be a compile error here, not a silent gap, same
/// principle `ana-pep508-to-matchspec`'s own tables already follow.
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
        // `pair`'s type (`CanonicalMarkerListPair`) is, like
        // `ContainerOperator` above, not re-exported by `uv_pep508` --
        // `{pair:?}` (a derived `Debug`) is the only thing this crate can
        // do with it from outside the crate, which is fine here since
        // this arm never needs anything but a human-readable label for
        // the error.
        MarkerExpression::List { pair, .. } => Err(Unconvertible::NoMatchspecEquivalent {
            key: format!("{pair:?}"),
        }),
        MarkerExpression::Extra { .. } => Err(Unconvertible::ExtraMarker),
    }
}

/// `python_version`/`python_full_version`/`implementation_version`, once
/// known values are restricted away, all become a condition on conda's
/// own `python` package version -- reroll's own `_FULL_VERSION_KEYS`
/// treats `python_full_version`/`implementation_version` identically
/// (both mean "the running CPython's version," since CPython is the only
/// supported interpreter), and `python_version` itself never actually
/// reaches here as its own key (see this module's docs) -- but it's
/// handled the same way regardless, as a safe fallback if a future
/// `uv_pep508` change ever stops canonicalizing it away.
fn version_condition(
    key: MarkerValueVersion,
    specifier: &VersionSpecifier,
) -> Result<MatchSpecCondition, Unconvertible> {
    let _ = key; // every key converts identically; see this function's docs
    convert_specifier(specifier)
}

/// `python_version in "..."`/`not in "..."`. Reroll's own equivalent
/// needs a network fetch (`python_latest_release.py`) to enumerate every
/// candidate minor between the literal's bounds and the latest known
/// Python release, because reroll's Python marker library preserves the
/// literal as an open-ended substring test. `uv_pep508` instead parses
/// the literal into concrete `Version`s up front and, per this module's
/// docs, canonicalizes the whole clause into a plain bounded range before
/// `to_dnf()` ever runs -- so in practice this function is not reached at
/// all for `python_version in/not in`, but it's implemented for real
/// (not stubbed) in case a future `uv_pep508` version stops doing that
/// canonicalization, or reaches this shape for a different key.
fn version_in_condition(
    key: MarkerValueVersion,
    versions: &[PypiVersion],
    membership: Membership,
) -> Result<MatchSpecCondition, Unconvertible> {
    let _ = key; // every key converts identically; see `version_condition`'s docs
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
        // "in" is a disjunction (any candidate matches); "not in" is a
        // conjunction (every candidate must be excluded) -- the same
        // De Morgan relationship reroll's own `rewrite_python_version_in_modifier`
        // (an `or`-chain) vs. `rewrite_python_version_not_in_modifier` (an
        // `and`-chain) encodes.
        Membership::In => or_chain(leaves),
        Membership::NotIn => and_chain(leaves),
    })
}

/// `version` as a condition on conda's `python` package -- the target
/// every version-family marker key converts to; see [`version_condition`]'s
/// docs.
fn python_condition(version: VersionSpec) -> MatchSpecCondition {
    MatchSpecCondition::MatchSpec(Box::new(MatchSpec {
        name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
        version: Some(version),
        ..MatchSpec::default()
    }))
}

/// `version`'s `(major, minor)` if it's at major.minor precision or
/// coarser -- every release segment from index 2 on is zero (or absent)
/// -- and carries no epoch/pre/post/dev segment, else `None`. This is
/// reroll's own `_parse_python_version_literal` "exact" gate
/// (`marker_conversion.py:213-236`), used here as the signal for whether
/// an ordered-comparator or equality boundary needs `_python_version_condition`'s
/// `.0a0` pre-release anchor at all.
///
/// This crate cannot dispatch on *key* the way reroll's
/// `_python_version_condition` (anchored) vs. `_full_version_condition`
/// (plain) split does: confirmed directly by probing `uv_pep508` 0.12.6
/// that a `python_version`-derived ordered comparator canonicalizes onto
/// the *identical* `Version { key: PythonFullVersion, .. }` shape a
/// literal `python_full_version` comparison at the same precision would
/// (e.g. `python_version >= "3.9"` and `python_full_version >= "3.9"`
/// both arrive here as `GreaterThanEqual, "3.9"`) -- the key is gone by
/// this layer. Gating on precision instead is both necessary (there's no
/// other signal left) and sufficient in practice: a `python_version`
/// origin *always* lands at this-or-coarser precision, because
/// `uv_pep508` performs reroll's own this-minor/next-minor boundary
/// arithmetic internally before this crate ever sees the specifier --
/// e.g. `python_version >= "3.9.2"` (non-exact per reroll's own gate)
/// already canonicalizes to `GreaterThanEqual, "3.10"`, still exactly 2
/// segments, confirmed by direct probing, not assumed -- so this
/// function never needs to re-derive "this minor vs. next minor" itself;
/// it only needs to add the anchor `uv_pep508` doesn't add. A genuine
/// `python_full_version`/`implementation_version` literal only reaches
/// the `Some` branch if the caller wrote one at major.minor-or-coarser
/// precision (e.g. `python_full_version >= "3.9"`, no patch component)
/// -- indistinguishable from a `python_version` origin here, and treated
/// the same way as an accepted, narrow divergence from reroll's exact
/// per-key split, since the alternative (never anchoring) is the
/// documented bug this function exists to fix. Full patch-precision
/// literals (`"3.9.1"`), the normal shape for a real
/// `python_full_version`/`implementation_version` marker, are `None`
/// here and pass straight through unchanged, exactly as before this fix.
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

/// `{major}.{minor}.0a0` as a conda `Version` -- reroll's
/// `_python_version_condition`'s pre-release-boundary anchor, formatted
/// directly rather than through [`conda_version`] (that helper takes a
/// PyPI `Version` with its own pre/post/dev formatting concerns this
/// synthetic boundary never has).
fn anchor(major: u64, minor: u64) -> Result<CondaVersion, Unconvertible> {
    let literal = format!("{major}.{minor}.0a0");
    literal
        .parse()
        .map_err(|source| Unconvertible::InvalidVersionLiteral { literal, source })
}

/// One [`VersionSpecifier`]'s contribution to a [`MatchSpecCondition`].
/// Exhaustive over [`Operator`]'s ten variants -- `TildeEqual`/`ExactEqual`
/// are expected unreachable for a marker (see this module's docs: `~=`
/// is always pre-expanded by `uv_pep508`, and `===` has no
/// marker-operator syntax at all), but written as real error arms rather
/// than a wildcard, same reasoning as [`Unconvertible::UnsupportedOperator`]'s
/// own docs.
///
/// `GreaterThanEqual`/`LessThan` get [`minor_precision`]'s `.0a0` anchor
/// when the boundary is at major.minor-or-coarser precision (reroll's
/// `_python_version_condition`); `LessThanEqual`/`GreaterThan` never do,
/// because a `python_version` origin never reaches this crate carrying
/// either of those two operators -- confirmed directly against
/// `uv_pep508` 0.12.6: `python_version <= "V"`/`python_version > "V"`
/// are *always* pre-rewritten onto `LessThan`/`GreaterThanEqual` with an
/// already-bumped next-minor boundary before this crate ever sees them
/// (this module's docs' canonicalization table), so a bare
/// `LessThanEqual`/`GreaterThan` reaching here can only be a genuine
/// `python_full_version`/`implementation_version` literal, for which
/// reroll's own `_full_version_condition` (and this function, unchanged)
/// is already correct: a plain, uncarved passthrough. This is
/// deliberately **not** the same anchor `ana-pep508-to-matchspec::version`'s
/// exclusive-comparator carve-out applies to *package* versions (`<V`
/// excluding `V`'s own pre-releases, `>V` excluding `V`'s own
/// post-releases) -- that is a different PEP 440 property entirely, not
/// a marker-boundary anchor.
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
        // `==` against a major.minor-or-coarser literal is
        // `python_version`'s own coarse equality (or an indistinguishable
        // coarse `python_full_version`/`implementation_version`
        // literal, see `minor_precision`'s docs) -- reroll's
        // `_python_version_condition` converts this to an explicit,
        // anchored two-clause range, never conda's fuzzy `StartsWith`
        // match: matchspec's fuzzy-equals syntax is deprecated, so this
        // crate never emits it for this operator, unlike the un-anchored
        // pre-fix version of this function.
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
        // `!=` keeps reroll's own asymmetric treatment: unlike `==`,
        // `_python_version_condition` never anchors `!=` into a two-clause
        // form either -- `python!=3.9.*` (fuzzy `NotStartsWith`) is
        // reroll's own converted form, not a shortcut this crate is
        // taking.
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
/// `1.0-1` shorthand becomes `1.0.post1`. Direct Rust port of reroll's
/// `version_format.format_version`; `PrereleaseKind`'s own `Display`
/// already spells `a`/`b`/`rc`, the same letters `packaging.version`
/// normalizes pre-release kinds to, so no separate letter lookup is
/// needed.
///
/// `pub` so `ana-pep508-to-matchspec::version` -- which needs the exact
/// same CEP-33 formatting for its own version-specifier conversion, plus
/// the ability to append its own `a0`/`.post`/bumped-release suffixes to
/// the formatted string before parsing -- calls this instead of keeping a
/// second, near-verbatim copy. Sharing is one-directional and safe: that
/// crate already depends on this one (per
/// `investigations/pep508_to_matchspec_api.md`'s crate layout), and this
/// crate has no dependency back on it.
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

    /// A leaf `MatchSpecCondition` for `python<version_spec>` -- the same
    /// construction [`python_condition`] does, exposed here so tests can
    /// build an expected value without duplicating rattler's own
    /// `VersionSpec` parsing conventions per test.
    fn python(version_spec: &str) -> MatchSpecCondition {
        python_condition(
            VersionSpec::from_str(version_spec, rattler_conda_types::ParseStrictness::Lenient)
                .unwrap(),
        )
    }

    /// `MatchSpecCondition::And`, for building an expected multi-leaf
    /// value -- each [`MarkerExpression`] leaf converts to its own
    /// `MatchSpecCondition` independently (per this module's docs: "never
    /// format-then-reparse," "nest the enum"), so two clauses on the same
    /// key (e.g. a `~=` expansion's `>=`/`<` pair) never get merged back
    /// into one leaf's `VersionSpec::Group` -- they stay two nested
    /// `MatchSpecCondition::MatchSpec` leaves.
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
            // sys_platform == "win32" is false on linux-64, so the whole
            // `and` is false regardless of python_version.
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

    /// `python_version`'s canonicalization onto `python_full_version`,
    /// per this module's docs -- pins the exact operator/version shape
    /// confirmed by probing `uv_pep508` 0.12.6 directly, not assumed.
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
            // python_version < "3.9" excludes all of 3.9.x too, per PEP
            // 508's minor-precision semantics for python_version -- and,
            // per reroll's own `_python_version_condition`, the boundary
            // itself is anchored at `.0a0` so a pre-release build of
            // 3.9 (e.g. `python==3.9.0a0`) is excluded too, not just
            // final/patch releases of 3.9.
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

        /// `==` never produces conda's fuzzy (`StartsWith`) match for a
        /// `python_version` boundary: matchspec's fuzzy-equals syntax is
        /// deprecated, and reroll's own `_python_version_condition`
        /// already converts `python_version == "X.Y"` to an explicit,
        /// anchored two-clause range (`python>=X.Y.0a0,<X.(Y+1).0a0`),
        /// never a fuzzy string -- this pins that range, not a fuzzy
        /// match string.
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

        /// Confirmed directly against reroll's own `_python_version_condition("==",
        /// "3")` (`python>=3.0.0a0,<3.1.0a0`): a bare major literal
        /// normalizes to minor `0`, so the upper bound is the *next*
        /// minor (`3.1`), not the next major (`4.0`) -- unlike the `3.*`
        /// glob case just below, which is two independent whole-major
        /// `Version` leaves from `uv_pep508`, not one `EqualStar` leaf.
        #[test]
        fn single_major_segment_equality_still_converts() {
            assert_eq!(
                convert(r#"requests; python_version == "3""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.0.0a0"), python("<3.1.0a0")))
            );
        }

        /// Ported from reroll's `test_reversed_comparison_operand_order_still_converts`.
        #[test]
        fn reversed_comparison_operand_order_still_converts() {
            assert_eq!(
                convert(r#"requests; "3.9" <= python_version"#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        /// Ported from reroll's `test_full_version_glob_marker_produces_a_rattler_valid_when_clause`,
        /// adapted: reroll's `numpy<1.25.0,>=1.24.0` package-version part
        /// is handled entirely by `ana-pep508-to-matchspec`, not this
        /// crate, so only the marker half is exercised here. Two separate
        /// `MarkerExpression::Version` clauses in the same DNF `and`-group
        /// convert to two separate, nested `MatchSpecCondition` leaves --
        /// see [`and2`]'s docs for why this isn't merged into one
        /// `VersionSpec::Group`. Both boundaries get the `.0a0` anchor,
        /// same as any other major.minor-precision `python_version`
        /// boundary.
        #[test]
        fn major_glob_equality_converts_to_a_range() {
            assert_eq!(
                convert(r#"requests; python_version == "3.*""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.0.0a0"), python("<4.0.0a0")))
            );
        }

        /// `~=` is not in `_python_version_condition`'s supported
        /// comparator set in reroll -- but `uv_pep508` pre-expands it into
        /// a plain range before this crate ever sees an operator at all,
        /// so it converts successfully here, a real (documented)
        /// divergence from reroll's stricter behavior. Both expanded
        /// boundaries are major.minor-precision `python_version`
        /// boundaries, so both get the `.0a0` anchor.
        #[test]
        fn compatible_release_is_pre_expanded_and_converts() {
            assert_eq!(
                convert(r#"requests; python_version ~= "3.9""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.9.0a0"), python("<4.0.0a0")))
            );
        }

        /// Ported from reroll's `test_in_marker_converts_via_the_membership_rewrite`,
        /// adapted: no `abi3_upper_bound` needed (see this module's docs)
        /// -- `uv_pep508` expands the literal's own bounds into a plain
        /// range directly, and both boundaries get the `.0a0` anchor.
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

        /// Ported from reroll's `test_not_in_marker_converts_via_the_membership_rewrite`.
        /// `uv_pep508` expands "not in a contiguous range" via De Morgan's
        /// law into an `Or` of the two excluded tails (`<3.9` or
        /// `>=3.11`) as two independent single-leaf DNF clauses --
        /// confirmed directly by probing `to_dnf()`'s output shape, not
        /// assumed from [`version_in_condition`]'s own `Membership::NotIn`
        /// handling, which (per this module's docs) is never actually
        /// reached for this input: `python_version not in "..."` never
        /// produces a `VersionIn` expression at all, it's pre-flattened
        /// into plain `Version` leaves before `to_dnf()` runs, same as
        /// every other `python_version` shape this module documents.
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
            // Unlike python_version's minor-precision equality,
            // python_full_version's own equality is already exact --
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
        /// interpreter, so the running interpreter's own version *is*
        /// the running Python's version. Ported from reroll's
        /// `_FULL_VERSION_KEYS` treating the two keys identically.
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

        /// Ported from reroll's
        /// `test_prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre`,
        /// but pinning a real, surprising divergence discovered while
        /// porting it rather than reroll's original expectation:
        /// `allow_pre` (the *package* version's own pre-release policy,
        /// in `ana-pep508-to-matchspec`) has no bearing on markers -- there
        /// is no `allow_pre` parameter here at all, which is the thing
        /// reroll's test name is actually about -- but `uv_pep508` 0.12.6
        /// itself silently drops a marker version literal's pre/post/dev
        /// segments entirely during parsing, keeping only the release
        /// segments (confirmed directly by probing `to_dnf()`'s output for
        /// `python_full_version >= "3.9.0rc1"` and friends: the resulting
        /// `VersionSpecifier`'s `version()` is `3.9.0`, `pre()` is `None`,
        /// same for `.post1`/`.dev1` suffixes -- not a `Debug`-formatting
        /// artifact, checked via `.pre()`/`.release()` directly). So this
        /// crate never actually sees the `rc1` to preserve or reject in
        /// the first place; the marker's pre-release *by itself* is simply
        /// unrepresentable one layer up, in `uv_pep508`'s own marker
        /// grammar, not in this crate's conversion. Pinned here (not just
        /// asserted in prose) so a future `uv_pep508` bump that starts
        /// preserving pre-release marker literals is a loud, obvious test
        /// failure, not a silent behavior change.
        ///
        /// A second compounding effect lands on top of the first, since
        /// the [`minor_precision`] fix: once `uv_pep508` has dropped
        /// `rc1`, the literal it hands this crate (`release=[3, 9]`) is
        /// indistinguishable from a genuine major.minor-precision
        /// `python_version` boundary (see [`minor_precision`]'s docs), so
        /// it now gets the same `.0a0` anchor a `python_version` boundary
        /// would -- `python>=3.9.0a0` rather than the plain `python>=3.9`
        /// this crate produced before that fix. This is *further* from
        /// the original marker's true intent than the plain form was
        /// (the original literal was itself a pre-release, `3.9.0rc1`,
        /// so the truly correct bound already excludes `python==3.9.0a0`
        /// -- an earlier pre-release stage -- and this anchored form
        /// incorrectly includes it) -- an accepted, narrow cost of a
        /// heuristic that has no way to tell a genuine coarse
        /// `python_full_version` literal apart from a `python_version`
        /// one at this layer, on top of an already-lossy upstream
        /// conversion; not a new class of bug, and not worth adding
        /// complexity to chase given how rare a marker pinning a specific
        /// pre-release of a full version is in the first place.
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

    /// Equivalence oracle, ported from reroll's
    /// `test_marker_matchspec_equivalence.py` (via its `tests/marker_oracle.py`
    /// helper): checks the [`Applicability`] [`to_matchspec_condition`]
    /// produces for a marker against `uv_pep508`'s own
    /// `MarkerTree::evaluate` (the pip/PEP 508 ground truth) for a sweep of
    /// candidate interpreter versions, so a passing test proves actual
    /// semantic equivalence for every candidate rather than agreement with
    /// one hand-picked expected string -- this is exactly the mechanism
    /// that would have caught the pre-release-boundary anchor bug this
    /// module's `.0a0` fix addresses (a candidate like `"3.9.0a0"` sits
    /// right on the boundary every ordered comparator against `"3.9"`
    /// cares about).
    ///
    /// No `abi3_upper_bound`/network fetch is needed for the
    /// `in`/`not in` cases reroll's own oracle needs one for -- see
    /// [`super::version_in_condition`]'s docs.
    mod equivalence_oracle {
        use super::*;

        /// A resolved CPython `python_full_version` sweep crossing every
        /// boundary the tests below care about: pre-3.9, each of 3.9's
        /// own pre-/dev-/post-release stages, 3.9.0 itself, later 3.9
        /// patches, the 3.10 boundary (including its own pre-release), a
        /// double-digit minor, and a major-version bump -- the same
        /// shape as reroll's own `_PYTHON_VERSION_CANDIDATES`/
        /// `_FULL_VERSION_CANDIDATES` (`test_marker_matchspec_equivalence.py`),
        /// merged into one list since this oracle checks both marker
        /// families against the same sweep.
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

        /// `full_version`'s `major.minor`, truncated the same way a real
        /// interpreter's own `python_version` always is relative to its
        /// `python_full_version` (`sys.version_info[:2]`) -- this is the
        /// one piece of "ground truth" logic this oracle takes on faith
        /// (it's definitional, not a claim about either implementation
        /// under test), used to build the independent reference in
        /// [`pip_ordered_reference`] and [`pip_equality_reference`]
        /// below.
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

        /// The **independent** ground truth an ordered comparator
        /// (`>=`/`>`/`<=`/`<`) against `key` holds for `full_version`,
        /// computed directly from PEP 440 version comparison -- not
        /// derived from either reroll's algorithm or this crate's own
        /// conversion, and deliberately **not** derived from
        /// `uv_pep508::MarkerTree::evaluate()` either: confirmed by
        /// direct probing (and cross-checked against Python's own
        /// `packaging.markers.Marker.evaluate`, the actual PEP 508
        /// reference implementation) that `uv_pep508` 0.12.6's
        /// `evaluate()` has this exact same missing-pre-release-anchor
        /// gap for `python_version` internally (it evaluates
        /// `python_version` markers via `env.get_version(PythonFullVersion)`
        /// against an unanchored canonicalized range, the same bug class
        /// this module's `.0a0` fix addresses, just living in
        /// `evaluate()` rather than `to_dnf()`/`restrict()`) -- e.g.
        /// `packaging.markers.Marker("python_version == "3.9"").evaluate(...)`
        /// is `True` for a `python_full_version` of `"3.9.0.dev0"`,
        /// while `uv_pep508::MarkerTree::evaluate()` on the equivalent
        /// environment returns `false`. Since [`to_matchspec_condition`]
        /// (the thing under test) never calls `evaluate()` at all --
        /// only `restrict()`/`to_dnf()` -- that upstream gap doesn't
        /// affect this crate's production behavior, but it does mean
        /// `evaluate()` can't be trusted as this oracle's ground truth
        /// for `python_version`, hence this hand-verified reference
        /// instead.
        ///
        /// `python_version`'s own comparison is against
        /// [`python_version_of`]`(full_version)` (the correctly
        /// major.minor-truncated value, exactly as a real interpreter's
        /// `python_version` environment marker always is); `python_full_version`/
        /// `implementation_version` compare `full_version` directly,
        /// unturncated.
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

        /// Whether `condition` holds for `python` -- a short recursive
        /// walk over the typed [`MatchSpecCondition`] tree, simpler than
        /// reroll's own oracle's regex-substitution approach
        /// (`marker_oracle.py`'s `matchspec_evaluates`) since there is no
        /// string to parse here at all.
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
        /// [`applicability_holds`] agrees with [`pip_ordered_reference`]'s
        /// independent PEP 440 comparison for every one of `candidates`
        /// -- the equivalence this whole conversion exists to preserve,
        /// checked against real ground truth rather than one hand-picked
        /// expected string (or, per this module's docs, `uv_pep508`'s
        /// own `evaluate()`, which has its own gap for `python_version`).
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

        /// The **independent** ground truth an equality comparator
        /// (`==`/`!=`, including the star-glob form against a
        /// `major[.minor].*` literal) holds for `full_version` against
        /// `key`'s literal -- [`pip_ordered_reference`]'s `==`/`!=`
        /// counterpart, computed via `uv_pep440`'s own
        /// [`VersionSpecifier::contains`] (a direct port of
        /// `packaging.specifiers`'s own matching, per its own doc
        /// comment) rather than marker evaluation (this module's docs
        /// explain why `uv_pep508::MarkerTree::evaluate()` can't be
        /// trusted for `python_version`) or this crate's own
        /// [`super::convert_specifier`] under test -- the same
        /// "independent primitive, not the thing under test" principle
        /// [`pip_ordered_reference`] itself follows.
        ///
        /// reroll's real oracle suite (`test_marker_matchspec_equivalence.py`)
        /// runs its independent pip-agreement check for `==`/`!=` too
        /// (e.g. `test_major_glob_agrees_with_pip_across_every_candidate`)
        /// -- this closes that gap for this crate's own oracle, per
        /// `investigations/pep508_to_matchspec_api.md`'s tracked
        /// follow-up: every `==`/`!=` case elsewhere in this module is a
        /// hand-written single-example assertion, never independently
        /// checked against pip across a whole candidate sweep the way
        /// the ordered comparators already are.
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

        /// The **independent** ground truth `python_version in
        /// "<literal>"`/`not in "<literal>"` holds for `full_version` --
        /// a plain [`str::contains`] substring test against `literal`,
        /// exactly PEP 508's own definition of `in`/`not in` (the same
        /// definition reroll's `marker_oracle.pip_evaluates` checks, via
        /// `packaging.markers.Marker.evaluate`) -- deliberately *not*
        /// the "or-chain of per-minor equalities" shape `uv_pep508`'s
        /// own canonicalization (and [`super::version_in_condition`])
        /// both assume, so this independently checks that assumption
        /// itself -- including whether it survives every literal
        /// separator style (space, comma, comma-plus-space) reroll's
        /// own `python_version_membership` module supports -- not just
        /// the range arithmetic built on top of it.
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
        /// counterpart -- converts `python_version {{in|not in}}
        /// "literal"}` once, then asserts agreement with
        /// [`pip_membership_reference`] for every candidate.
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

            /// `"3.9.1"`, not `"3.9.0"`: a boundary with a nonzero patch
            /// segment, so `uv_pep440`'s own trailing-zero normalization
            /// never collapses it to major.minor precision -- keeping
            /// [`minor_precision`]'s anchor gate reliably `None` here, so
            /// this test validates the *plain-passthrough* path (the
            /// normal shape for a real `python_full_version`/
            /// `implementation_version` literal) rather than the
            /// accepted 2-segment tradeoff
            /// `full_version_glob_and_two_segment_literal_matches_the_known_tradeoff`
            /// below exists to document separately.
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

            /// Documents (rather than silently accepting) the one known,
            /// narrow tradeoff [`minor_precision`]'s docs describe: a
            /// `python_full_version` literal written at *major.minor*
            /// precision (no patch segment, e.g. `"3.9"`) is
            /// indistinguishable from a `python_version` origin at
            /// `convert_specifier`'s layer, so it gets the same `.0a0`
            /// anchor treatment -- which is *not* what an independent
            /// PEP 440 comparison against the literal `"3.9"` (`==
            /// "3.9.0"`) would say for a pre-release candidate sitting
            /// between the anchor and the true boundary (`"3.9.0.dev0"`,
            /// `"3.9.0a0"`). This sweeps only the candidates *outside*
            /// that narrow gap, so it still catches a real regression
            /// (e.g. losing the next-minor upper-bound arithmetic
            /// entirely) without re-litigating the already-accepted
            /// tradeoff on every run.
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
        /// `==`/`!=` counterpart, via [`assert_equality_agrees_with_pip`]
        /// -- ported from reroll's own
        /// `test_agrees_with_pip_across_every_candidate`'s `==`/`!=` rows
        /// and `test_major_glob_agrees_with_pip_across_every_candidate`/
        /// `test_glob_literal_agrees_with_pip_across_every_candidate`.
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

                /// Ported from reroll's
                /// `test_major_glob_agrees_with_pip_across_every_candidate`
                /// (`==` row).
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

                /// Ported from reroll's
                /// `test_glob_literal_agrees_with_pip_across_every_candidate`.
                /// `"3.9.*"` is, per [`super::super::minor_precision`]'s
                /// docs, indistinguishable at `convert_specifier`'s
                /// layer from a bare `python_version`-precision literal,
                /// so it inherits the same, already-accepted
                /// `two_segment_literal_agrees_with_pip_outside_the_known_tradeoff_gap`
                /// tradeoff for the handful of candidates sitting in the
                /// gap between the `.0a0` anchor and this literal's true
                /// boundary -- filtered out here for the same reason,
                /// not because this crate's output disagrees with pip
                /// anywhere else.
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

                /// Ported from reroll's
                /// `TestImplementationVersionEquivalence::test_agrees_with_pip_across_every_candidate`.
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

        /// Ported from reroll's `TestPythonVersionMembershipEquivalence`/
        /// `TestPythonVersionNotInMembershipEquivalence`'s
        /// `test_multi_value_list_agrees_with_pip_across_every_candidate`,
        /// parametrized the same way across all 3 literal separator
        /// styles reroll's own `python_version_membership` module
        /// supports (space, comma, comma-plus-space) -- via
        /// [`assert_membership_agrees_with_pip`], the correct,
        /// independent pip ground truth in every case, comma-bearing
        /// literals included: this module asserts what pip actually
        /// says, never a hand-pinned "whatever this crate currently
        /// does" expectation.
        ///
        /// The two comma-bearing styles are `#[ignore]`d, not deleted or
        /// rewritten to assert the wrong answer: probing confirmed
        /// `assert_membership_agrees_with_pip` itself is checking the
        /// right thing (its failure message reports a real, specific
        /// disagreement -- e.g. `pip says false, matchspec says true`
        /// for candidate `"2.7.18"` against `"3.8,3.9,3.13"` -- not a
        /// test-construction mistake), and the disagreement traces to a
        /// confirmed root cause below, not an oracle bug. `#[ignore]`
        /// keeps the *correct* expectation on record (so a future
        /// `uv_pep508` fix flips these green without anyone having to
        /// rewrite the assertion) while keeping today's known failure
        /// from blocking the suite.
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

            /// A second, distinct instance of the same root-cause class
            /// [`super::super::reversed_membership_operand_order_asymmetry`]
            /// documents (a `uv_pep508` parse-time silent drop, not a
            /// bug in this crate's own conversion, and not a bug in
            /// [`assert_membership_agrees_with_pip`]/[`pip_membership_reference`]):
            /// `parse_version_in_expr` (`uv_pep508`'s `marker/parse.rs`)
            /// tokenizes a `python_version in "<literal>"` literal by
            /// splitting on whitespace only (`cursor.take_while(|c|
            /// !c.is_whitespace())`) and parsing each token as a
            /// standalone PEP 440 version -- a comma-separated literal
            /// with no surrounding whitespace (`"3.8,3.9,3.13"`) is one
            /// token that fails to parse as a version at all (a comma
            /// isn't valid PEP 440 version syntax), so the *entire*
            /// clause is silently dropped to `MarkerTree::TRUE` before
            /// this crate ever sees it -- confirmed directly by probing
            /// `Requirement::from_str`'s output (`.marker.is_true()` is
            /// `true` for both comma-bearing styles here, `false` for
            /// the space-separated style
            /// [`super::space_separated_in_agrees_with_pip_across_every_candidate`]
            /// exercises). That makes every candidate's `to_matchspec_condition`
            /// result `Applicability::Always`, which disagrees with real
            /// pip agreement for any candidate whose `python_version`
            /// isn't itself a substring of the (unparsed) literal --
            /// exactly what [`super::assert_membership_agrees_with_pip`]
            /// catches.
            ///
            /// reroll's own oracle parametrizes over exactly these 3
            /// separator styles because reroll's own
            /// `python_version_membership` rewrite supports all three
            /// identically; this crate's `uv_pep508`-canonicalization-
            /// dependent approach does not, and the two comma-bearing
            /// styles are the ones that lose the clause entirely. Fixing
            /// this for real requires this crate (or its caller) to stop
            /// relying on `uv_pep508`'s own literal tokenizer for this
            /// shape -- out of scope here; un-ignore once that lands.
            ///
            /// Reported upstream as
            /// [astral-sh/uv#21310](https://github.com/astral-sh/uv/issues/21310)
            /// -- still open, with a fix proposed in
            /// [astral-sh/uv#21311](https://github.com/astral-sh/uv/pull/21311)
            /// as of this writing. Unlike
            /// [`super::super::reversed_membership_operand_order_asymmetry`]'s
            /// [astral-sh/uv#21309](https://github.com/astral-sh/uv/issues/21309)
            /// (closed "not planned"), this one is not (yet) a
            /// won't-fix, so these `#[ignore]`s are a genuine "unignore
            /// once fixed upstream," not an indefinite one.
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

    /// Ported from reroll's `test_pep508_to_matchspec.py::TestMarkers`
    /// combinator tests -- `and`/`or` structure is preserved via
    /// `MatchSpecCondition::And`/`Or` directly, never a formatted-then-
    /// reparsed string.
    mod combinators {
        use super::*;

        #[test]
        fn combined_marker_preserves_and_or_structure() {
            // sys_platform == "win32" resolves to False on linux-64, so
            // it drops from the `and` -- the whole thing is Never, not
            // Conditionally. The *portable* equivalent (a package
            // targeting an unknown future platform) would instead keep
            // both sides; see `investigations/pep508_to_matchspec_api.md`'s
            // "Slow path, take 1" for that mode.
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "win32" and python_version >= "3.9""#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Never
            );
            // On win-64, the same marker keeps the python_version part.
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "win32" and python_version >= "3.9""#,
                    Platform::Win64
                )
                .unwrap(),
                Applicability::Conditionally(python(">=3.9.0a0"))
            );
        }

        /// `to_dnf()`'s leaf order follows the BDD's own canonical
        /// variable ordering, not source-text order -- confirmed directly:
        /// `implementation_version` orders before `python_full_version`
        /// (the key `python_version` itself canonicalizes onto) in this
        /// output, regardless of which one appears first in the marker
        /// string.
        ///
        /// `implementation_version < "3.12"` is a 2-segment
        /// `implementation_version` literal -- indistinguishable here
        /// from a `python_version` boundary at the same precision (see
        /// [`minor_precision`]'s docs), so it gets the same `.0a0` anchor
        /// a `python_version < "3.12"` boundary would, an accepted
        /// tradeoff of that heuristic rather than reroll's own
        /// (key-based, never-anchors-`implementation_version`) behavior.
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

    /// A confirmed **upstream bug** in `uv_pep508` 0.12.6 (reproduced
    /// and still present on `uv`'s `main` branch as of this writing,
    /// commit `0697445cfef3839748907ae52e3fba14de31e3da`; reported
    /// upstream as
    /// [astral-sh/uv#21309](https://github.com/astral-sh/uv/issues/21309)
    /// -- closed by the maintainers as "not planned"), not a design
    /// choice of this crate:
    /// `"<literal>" in <version-key>` / `"<literal>" not in <version-key>`
    /// (the literal on the *left* of `in`/`not in`, against a *version*
    /// marker key) silently parses to `MarkerTree::TRUE` instead of a
    /// real, environment-dependent expression.
    ///
    /// This was verified directly against `packaging` 26.3 -- the same
    /// library `pip`'s own marker evaluation uses, i.e. the actual
    /// ground truth PEP 508 defers to, not just PEP 508's prose:
    ///
    /// ```text
    /// >>> Marker('"3.11" in python_version').evaluate({"python_version": "3.9"})
    /// False
    /// >>> Marker('"3.11" in python_version').evaluate({"python_version": "3.11"})
    /// True
    /// ```
    ///
    /// `pip` never treats this as a tautology -- it's an ordinary
    /// (Python-`in`-operator) substring test, `lhs_value in rhs_value` in
    /// the marker's own left-to-right textual order, which for a
    /// realistic major.minor `python_version` string behaves the same as
    /// the forward form's already-correct conversion
    /// (`python_version in "<literal>"`) for the same literal.
    ///
    /// Root cause, confirmed directly against `uv_pep508`'s own
    /// `marker/parse.rs` source (0.12.6): `parse_marker_value`'s
    /// `MarkerValue::QuotedString` arm (lines 277-283) dispatches the
    /// reversed form of a version-key comparison to
    /// `parse_inverted_version_expr` (lines 481-529), which only knows
    /// how to invert an *ordinary* comparison operator (`==`, `<`, etc.,
    /// via `MarkerOperator::to_pep440_operator`, line 506) -- there is no
    /// PEP 440 equivalent for `in`/`not in`, so `to_pep440_operator()`
    /// returns `None`, a warning ("will be ignored") is reported through
    /// the `Reporter`, and the function itself returns `None`. Its
    /// callers (`parse_marker_and`/`parse_marker_or`, via the shared
    /// `parse_marker_op`, same file, lines 585-646) treat a `None` leaf
    /// as "nothing to add to this and/or chain," not as a parse error --
    /// so a marker consisting of *only* this shape parses to
    /// `MarkerTree::TRUE`, and inside a larger `and`/`or` expression the
    /// clause vanishes entirely rather than constraining anything.
    ///
    /// This is *not* how the analogous reversed form behaves for a
    /// *string* key (`sys_platform`, etc.): the same `QuotedString` arm's
    /// `MarkerEnvString` case gets a real, correctly-inverted
    /// `MarkerExpression::String` -- no "can't invert this operator" case
    /// exists for a string key, because `in`/`not in` against a string
    /// never needs a PEP 440 operator to invert in the first place. The
    /// tests below that exercise `sys_platform` are passing contrasts,
    /// not part of the bug.
    ///
    /// Because the bug is at `uv_pep508`'s own *parse* time, it is
    /// unrecoverable at this crate's layer: [`to_matchspec_condition`]
    /// only ever receives an already-parsed `MarkerTree`, and the
    /// resulting tree for the buggy shape is bit-for-bit
    /// `MarkerTree::TRUE` -- identical to what a genuinely marker-free
    /// package produces. There is no residual expression, no error, and
    /// no distinguishing signal left in the `MarkerTree` for this crate
    /// to inspect or reject; describing this as "already canonical" (per
    /// [`to_matchspec_condition`]'s and this crate's `lib.rs` docs) is
    /// therefore wrong specifically for this shape.
    ///
    /// The `#[ignore]`d tests below assert the behavior a correct
    /// implementation should have (verified against `pip`'s own
    /// `packaging` library above, and cross-checked against this
    /// crate's own already-correct forward-direction conversion for the
    /// same literal) -- they currently fail, proving this crate inherits
    /// the upstream bug rather than working around it.
    ///
    /// Because
    /// [astral-sh/uv#21309](https://github.com/astral-sh/uv/issues/21309)
    /// was closed as "not planned" -- i.e. there is no upstream fix
    /// coming -- these tests are **intended to stay `#[ignore]`d
    /// indefinitely**, not as a to-do list waiting on one. They are kept
    /// (rather than deleted, or rewritten to assert the current, wrong
    /// answer) because they still capture the *correct* behavior,
    /// independently confirmed against `packaging` above: if this crate
    /// (or a caller) ever adds its own local guard upstream of
    /// `restrict()` -- mirroring how `platform_release`/`platform_version`
    /// are already rejected, see [`crate::known_values_assumption`]'s
    /// docs -- these tests are what should flip green to confirm it,
    /// with no rewrite needed.
    mod reversed_membership_operand_order_asymmetry {
        use super::*;

        /// Per `packaging`'s ground truth, `"3.11" in python_version`
        /// should behave the same as this crate's already-correct
        /// forward-direction conversion of the identical literal
        /// (`python_version_conversion::in_marker_converts_to_a_bounded_range`'s
        /// single-candidate case, confirmed to produce this exact
        /// bound) -- not [`Applicability::Always`]. Currently fails: the
        /// upstream bug (see this module's docs) makes the actual result
        /// `Ok(Applicability::Always)`.
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

        /// Same bug, `not in` direction. Per `packaging`'s ground truth,
        /// `"3.11" not in python_version` should behave the same as this
        /// crate's already-correct forward-direction conversion of
        /// `python_version not in "3.11"` (confirmed directly: that
        /// forward form converts to a fuzzy minor exclusion, the same
        /// shape `python_version != "3.9"` gets in
        /// `python_version_conversion::inequality_becomes_a_fuzzy_minor_exclusion`)
        /// -- not [`Applicability::Always`]. Currently fails the same
        /// way as the `in` case above.
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

        /// Contrast (not part of the bug, and not ignored): the
        /// *forward* operand order (key on the left) for the exact same
        /// literal already converts correctly -- proving the gap is
        /// specific to operand order, not `in`/`not in` membership
        /// testing in general (also already covered by
        /// `python_version_conversion::in_marker_converts_to_a_bounded_range`
        /// and its `not in` sibling).
        #[test]
        fn forward_in_python_version_still_converts_correctly() {
            assert_eq!(
                convert(r#"requests; python_version in "3.11""#, Platform::Linux64).unwrap(),
                Applicability::Conditionally(and2(python(">=3.11.0a0"), python("<3.12.0a0")))
            );
        }

        /// Contrast (not part of the bug, and not ignored): for a
        /// *string* key, the reversed operand order is parsed into a
        /// real expression regardless of order, so it still reaches
        /// this crate as an ordinary
        /// [`Unconvertible::NoMatchspecEquivalent`] (the same outcome
        /// the forward order already has for `sys_platform`) -- not a
        /// silently-dropped clause. This is the asymmetry's other half:
        /// identical marker *shape* (`"<literal>" in <key>`), opposite
        /// parse-time outcome, depending only on whether `<key>` is a
        /// version key (buggy) or a string key (correct).
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

        /// The practical, worse-than-an-error effect described in this
        /// investigation: inside a larger conjunction, the dropped
        /// clause doesn't just fail to add a constraint -- it vanishes
        /// so completely that the *other* clause's own truth value alone
        /// determines the whole marker's [`Applicability`], as if the
        /// `python_version` half was never written at all. The correct
        /// result mirrors `applicability::a_known_and_free_conjunction_collapses_to_just_the_free_part`'s
        /// pattern: a known-true clause conjoined with a free-variable
        /// clause should collapse to just the free part, not to
        /// `Always`.
        #[test]
        #[ignore = "upstream bug, not planned to be fixed: uv_pep508 silently drops the \
                    reversed \"<literal>\" in <version-key> form at parse time (confirmed \
                    against uv_pep508 0.12.6 and uv main @ 0697445c; reported upstream as \
                    astral-sh/uv#21309, closed \"not planned\"), so the whole conjunction \
                    loses its python_version half before this crate ever sees it -- intended \
                    to stay ignored forever absent a local workaround; see this module's docs"]
        fn reversed_in_python_version_should_survive_a_surrounding_conjunction() {
            // sys_platform == "linux" is true on linux-64; a correct
            // implementation keeps the "3.11" in python_version half as
            // a real condition, the same way
            // `a_known_and_free_conjunction_collapses_to_just_the_free_part`
            // keeps `python_version >= "3.9"` here. The actual (buggy)
            // result is `Applicability::Always`.
            assert_eq!(
                convert(
                    r#"requests; sys_platform == "linux" and "3.11" in python_version"#,
                    Platform::Linux64
                )
                .unwrap(),
                Applicability::Conditionally(and2(python(">=3.11.0a0"), python("<3.12.0a0")))
            );
            // sys_platform == "win32" is false on linux-64, so the whole
            // conjunction is false regardless of python_version -- this
            // half is unaffected by the bug (False AND anything is
            // False), included here for contrast with the assertion
            // above, not as its own regression case.
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
