//! Per-[`Requirement`] orchestration: name + version + extras + marker ->
//! `MatchSpec`. See the crate's module docs for the three-outcome return
//! shape and why `extra` clauses are this crate's own concern rather than
//! `ana-marker-matchspec`'s.
//!
//! Rust port of reroll's `pep508_to_matchspec()`. Name mapping
//! ([`conda_name`]) consults the `pypi_name -> conda_name` lookup table
//! every caller supplies (an `ana-pypi-conda-map::MappingHandle`); a name
//! absent from the table keeps the identity mapping, which is correct for
//! the vast majority of packages -- the table only ever holds the
//! differing entries.

use ana_marker_matchspec::{Applicability, Unconvertible};
use ana_pypi_conda_map::MappingHandle;
use rattler_conda_types::{MatchSpec, PackageName, PackageNameMatcher, ParseVersionError};
use uv_normalize::ExtraName;
use uv_pep508::{MarkerTree, Requirement, VersionOrUrl};

#[cfg(test)]
use rattler_conda_types::MatchSpecCondition;

use crate::version::version_spec;

/// CEP-26's package-name length limit, and CEP-29's `extras=[...]` bracket
/// key length limit -- both cap at 64 characters. `uv_pep508`'s
/// `PackageName`/`ExtraName` already guarantee every other part of each
/// grammar's shape (lowercase, alnum-bounded, single-separator-run, no
/// leading `_`) via PEP 503 name normalization: it only ever produces
/// `[a-z0-9]+(-[a-z0-9]+)*`, a strict subset of CEP-26's regex modulo
/// length, and the same holds for extras under PEP 508's grammar. Length
/// is the one thing normalization can't bound -- a PyPI name can be
/// arbitrarily long, conda's can't -- so it's the one check this module
/// still does itself, on top of whatever shape check already produced the
/// name (`uv_pep508`'s own normalization for an unmapped name,
/// [`MappingHandle::get`]'s for a mapped one), and the only reason
/// `rattler_conda_types::PackageName::new_unchecked` (not `TryFrom`) is
/// safe to use at the [`conda_name`] call site: `TryFrom`'s charset check
/// would be redundant with what normalization already guarantees, and it
/// doesn't check length at all, so it wouldn't catch the one case that
/// matters anyway.
const MAX_CEP26_NAME_LENGTH: usize = 64;

/// Converts one already-parsed PEP 508 [`Requirement`] into a conda
/// [`MatchSpec`], or `None` if `requirement`'s marker can never hold on
/// the machine `assumption` describes -- see the crate's module docs for
/// this three-outcome shape and why it isn't `Result<MatchSpec, _>`.
///
/// `allow_pre` governs whether a pre-release *package* version is accepted
/// (default policy, matching reroll: rejected). It has no bearing on
/// markers, which have no `allow_pre` concept at all.
///
/// `assumption` is [`ana_marker_matchspec::known_values_assumption`]'s
/// output for the subdir being installed onto -- built once by the
/// caller and reused across every [`convert`]/[`convert_all`] call, never
/// computed here.
///
/// `pypi_to_conda_map` is the `pypi_name -> conda_name` lookup table
/// [`conda_name`] consults for `requirement.name`; an entry absent from
/// it keeps the identity mapping (see the crate's module docs). Always a
/// real handle, never optional: correctly converting a PyPI requirement
/// depends on this table, so a caller with no mapping available (e.g. a
/// mapping load failed and the caller has already decided that's fatal)
/// has nothing sensible to pass here at all -- there's no permissive
/// "skip the lookup" mode to fall back to. Tests that don't care about
/// mapping behavior use `MappingHandle::from_map(HashMap::new())`, which
/// behaves identically to every name being absent from a real table.
///
/// Checks for an `extra == "..."` clause *before* ever calling into
/// `ana-marker-matchspec`: this crate has no notion of which extras are
/// "active" for the current install, so any requirement whose marker
/// mentions `extra` at all -- combined with an environment condition or
/// not -- is rejected outright with [`ConvertError::Marker`]. This is
/// unconditional, not delegated to
/// `ana_marker_matchspec::Unconvertible::ExtraMarker`: that variant is for
/// a marker whose *only* problem is an `extra` clause reaching conversion
/// by mistake (a caller bug), whereas this check is the intended,
/// permanent boundary between the two crates' scopes.
///
/// No string is formatted and reparsed to build the returned `MatchSpec`;
/// every field is constructed directly. See [`crate::version`] for the one
/// unavoidable string round-trip (an individual version literal).
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
/// [`convert`]'s docs for why any such clause is rejected outright. Built
/// on [`MarkerTree::visit_extras`], which -- unlike `top_level_extra_name`
/// -- walks the whole tree rather than only recognizing a single extra
/// clause sitting at the top.
fn marker_has_extra_clause(marker: MarkerTree) -> bool {
    let mut found = false;
    marker.visit_extras(|_operator, _extra| found = true);
    found
}

/// Below this many requirements, convert them sequentially instead of
/// handing them to `rayon`. Mirrors `ana-pyproject`'s own
/// `PARALLEL_PARSE_THRESHOLD` (`crates/ana-pyproject/src/project.rs`): a
/// single [`convert`] call is a handful of cheap checks plus at most one
/// small string round-trip, on the order of `Requirement::from_str` or
/// cheaper, while waking a parked `rayon` worker thread can cost an
/// OS-scheduler round trip an order of magnitude more. Below a few dozen
/// requirements there's no plausible parallelism gain that pays for
/// entering `rayon` at all -- a starting estimate, not a measured one.
const PARALLEL_CONVERT_THRESHOLD: usize = 64;

/// [`convert`], run over every element of `requirements` -- on `rayon`'s
/// work-stealing pool once there are enough of them to be worth it (see
/// [`PARALLEL_CONVERT_THRESHOLD`]), sequentially otherwise. Index-aligned
/// with `requirements`, so a caller can report every failing requirement
/// in one pass rather than fail-fast on the first `Err`.
///
/// Never constructs its own `rayon::ThreadPoolBuilder`: the parallel path
/// calls into the process-global pool via `into_par_iter`, same as
/// `ana-pyproject`'s own parallel requirement parsing, so the two stages
/// share cores instead of competing for them.
///
/// Generic over borrowed or owned requirements (`&[Requirement]` and
/// `&[&Requirement]` both work), so callers holding requirements inside a
/// larger struct don't have to deep-clone them into a slice first.
///
/// `pypi_to_conda_map` is forwarded to every [`convert`] call unchanged --
/// see that function's docs.
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
/// `pypi_to_conda_map` if it has an entry for `name` and that entry
/// itself has a valid conda-package-name shape, or kept unchanged
/// otherwise (the identity mapping, correct for every name the table
/// doesn't mention -- it only ever holds the entries that genuinely
/// differ; see `ana-pypi-conda-map`). [`MappingHandle::get`] is what
/// actually validates the mapped value's shape; a mapped value that
/// fails that check surfaces as [`ConvertError::InvalidMappedName`]
/// rather than reaching `PackageName::new_unchecked` unchecked.
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
/// `foo-bar`; `uv_pep508` normalizes each independently but does not
/// deduplicate across a requirement's own extras list) -- and validated
/// against CEP-29's 64-character limit, per [`MAX_CEP26_NAME_LENGTH`].
/// `None` for an empty (post-dedup) list, so a bracket-less matchspec is
/// produced rather than an explicit, useless `extras=[]` clause.
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
    /// `requirement.marker` contains an `extra == "..."` clause -- this
    /// crate's own, permanent scope boundary, unrelated to whether the
    /// rest of the marker would otherwise convert; see [`convert`]'s docs.
    /// `marker` is the marker's own rendered text (e.g. `extra == "foo"`),
    /// empty only if `MarkerTree::contents()` itself returned `None`,
    /// which isn't known to happen in practice but isn't ruled out by
    /// `is_true()`'s own documented false-negative behavior.
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
    /// its mapped value doesn't have a valid conda-package-name shape --
    /// see [`ana_pypi_conda_map::InvalidMappedName`]'s own docs for how
    /// that can happen despite the same check already having run once at
    /// fetch time.
    #[error("invalid pypi-to-conda mapping entry: {0}")]
    InvalidMappedName(#[from] ana_pypi_conda_map::InvalidMappedName),

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

    /// `ana-marker-matchspec` couldn't represent `requirement.marker` (once
    /// known values are restricted away) as a matchspec condition -- a key
    /// with no matchspec equivalent (`platform_release`/`platform_version`,
    /// or a genuinely unsupported comparator), propagated rather than
    /// re-wrapped in this crate's own words, since
    /// `ana_marker_matchspec::Unconvertible`'s own variants are already
    /// specific about which key/comparator was the problem.
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

    /// A `MappingHandle` with no entries -- the required-but-irrelevant
    /// mapping table for tests that don't care about name mapping at
    /// all. Behaves identically to every name being absent from a real
    /// table.
    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    /// The fixed, deterministic test target -- `linux-64`, regardless of
    /// whatever platform actually runs these tests. Tests that care about
    /// a *different* subdir's known values say so explicitly (see the
    /// `markers` module); everything else uses this so outcomes don't
    /// depend on the CI/dev machine's own platform.
    fn assumption() -> MarkerTree {
        ana_marker_matchspec::known_values_assumption(rattler_conda_types::Platform::Linux64)
            .unwrap()
    }

    /// [`convert`] against [`assumption`], asserting the requirement
    /// applies on `linux-64` (not `Applicability::Never`) and unwrapping
    /// the rest -- the shape most tests below want, since most aren't
    /// testing marker applicability itself (see the `markers` module for
    /// those).
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
    /// [`convert`]'s output without hand-building a `MatchSpec` per test --
    /// same "compare against the parser's own understanding" approach as
    /// `version.rs`'s tests. `with_extras(true)` because the
    /// `extras=[...]` bracket key is CEP-29/repodata-V3-gated behind that
    /// option, even though every `MatchSpec` this crate itself produces
    /// sets `extras` as a plain typed field, never through this string
    /// parser.
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

        /// The whole point of threading a mapping table through at all:
        /// a name present in it is replaced, not passed through
        /// identity-mapped.
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

        /// A name absent from the table keeps the identity mapping, even
        /// though a (non-empty, unrelated) table is in hand -- the table
        /// only ever holds the entries that genuinely differ, so "not
        /// mentioned" must mean "unchanged," not "reject" or "empty
        /// result."
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

        /// An empty table (no mapping data available at all -- e.g. a
        /// project with a genuinely empty `ana-pypi-conda-map` cache) is
        /// exactly the identity mapping -- the same outcome every other
        /// test in this module gets via [`convert_ok`]'s own
        /// [`no_mapping`], spelled out here once, explicitly.
        #[test]
        fn empty_table_is_identity_mapped() {
            let result = convert(&req("opencv-python"), false, assumption(), &no_mapping())
                .unwrap()
                .unwrap();
            assert_eq!(result, expect("opencv-python"));
        }

        /// The 64-character length check applies to the *mapped* name,
        /// not the original PyPI one -- a short PyPI name that maps to an
        /// over-long conda name must still be rejected, not silently
        /// accepted just because the pre-mapping name was fine.
        #[test]
        fn mapped_name_over_64_characters_is_rejected() {
            let long_name = "a".repeat(65);
            let handle =
                MappingHandle::from_map(HashMap::from([("short-pkg".to_string(), long_name)]));
            let err = convert(&req("short-pkg"), false, assumption(), &handle).unwrap_err();
            assert!(matches!(err, ConvertError::NameTooLong { .. }), "{err:?}");
        }

        /// The scenario [`ConvertError::InvalidMappedName`] exists for:
        /// a mapping entry whose value doesn't have a valid conda
        /// package-name shape (a space is never valid) is rejected with
        /// a specific error rather than reaching
        /// `PackageName::new_unchecked` unchecked.
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

        /// Ported from reroll's `test_pep508_to_matchspec.py`'s
        /// `test_local_version_label_with_strict_less_or_greater_than_is_rejected_by_packaging`:
        /// unlike `==`/`!=`/`===`, a local label combined with `<`/`>`
        /// never reaches this crate's own local-version-label check --
        /// `uv_pep508::Requirement::from_str` itself already rejects the
        /// combination (see
        /// [`crate::version::reject_unsupported_version`]'s docs), so
        /// there's no `Requirement` for [`convert`] to reject.
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

        /// A marker referencing a *known* key (one `assumption` covers)
        /// no longer produces [`ConvertError::Marker`] -- it resolves via
        /// `restrict()`, either to `Applicability::Always` (this test) or
        /// `Applicability::Never` (see
        /// [`known_false_key_marker_makes_the_dependency_never_apply`]).
        /// `sys_platform == "win32"` is false on `linux-64`, the fixed
        /// test target, so the *dependency* doesn't apply -- that's not
        /// an error, it's the ordinary "this platform-specific dependency
        /// doesn't apply here" outcome.
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

        /// Ported from reroll's `test_pep508_to_matchspec.py`'s
        /// `test_extra_marker_rejection_takes_precedence_over_a_direct_url`:
        /// [`convert`] checks for an `extra` clause before it ever
        /// inspects `requirement.version_or_url`, so a requirement with
        /// *both* a direct URL and an `extra` marker is rejected as
        /// [`ConvertError::Marker`], not [`ConvertError::DirectUrl`].
        #[test]
        fn marker_rejection_takes_precedence_over_a_direct_url() {
            let entry = r#"requests @ https://example.com/pkg.whl ; extra == "foo""#;
            let err = convert_err(&req(entry), false);
            assert!(matches!(err, ConvertError::Marker { .. }), "{err:?}");
        }

        /// The remaining `extra`-clause shapes reroll's
        /// `test_pep508_to_matchspec.py`'s `TestMarkers` dedicates separate
        /// tests to (combined `extra` clauses, `extra` mixed with an
        /// environment condition, a reversed comparison operand order)
        /// all collapse to the same outcome here: *any* marker containing
        /// an `extra` clause is rejected uniformly -- see [`convert`]'s
        /// docs for why this is this crate's own permanent scope
        /// boundary, not delegated to `ana-marker-matchspec`.
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

        /// Every one of these markers references a *known* key with a
        /// value that happens to hold on `linux-64` -- none are
        /// `ConvertError::Marker`, they resolve via `restrict()` to
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

        /// `sys_platform == "cygwin"` is a known key with a value that
        /// can never hold on `linux-64` -- `Applicability::Never`, so the
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
        /// pre-expands `~=` into a plain range before this crate (or
        /// `ana-marker-matchspec`) ever sees an operator -- see
        /// `ana-marker-matchspec`'s own
        /// `compatible_release_is_pre_expanded_and_converts` test for the
        /// exact expansion. This is a real, documented divergence from
        /// reroll's stricter behavior (reroll's own
        /// `_python_version_condition`/`_full_version_condition`
        /// explicitly reject `~=`).
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

        /// Reroll's `test_reversed_in_marker_raises` and
        /// `test_reversed_not_in_marker_raises` assert that a
        /// reversed-operand `in`/`not in` marker (literal on the left:
        /// `"foo" in extra`, `"3.9" not in python_version` -- as opposed
        /// to the supported `python_version in "3.9"` shape) raises
        /// `UnconvertableMarkerError` in reroll's own parser.
        ///
        /// **This crate's dependency, `uv_pep508`, does not raise for
        /// either shape and does not preserve them as a real constraint
        /// either**: `Requirement::from_str` parses both successfully,
        /// and the resulting `marker.is_true()` is `true`. So [`convert`]
        /// does *not* reject either shape: the dependency is silently
        /// treated as marker-free and converted as if the reversed
        /// clause were never written -- a real divergence from reroll's
        /// own (stricter) behavior. Pinned here as two tests asserting
        /// the *current* (surprising) behavior rather than the desired
        /// one, so a future `uv_pep508` upgrade that starts parsing these
        /// shapes into a real, non-trivial `MarkerTree` turns into a
        /// loud test failure here instead of a silent behavior change.
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

        /// A reversed-operand compatible-release comparison against a pure
        /// string marker field (literal on the left: `"posix" ~= os_name`,
        /// as opposed to the never-meaningful-either-way normal-order
        /// `os_name ~= "posix"`) used to reach an `unreachable!()` panic in
        /// `uv_pep508`'s marker algebra: any `pyproject.toml` containing
        /// this exact marker shape crashed the whole process, a real
        /// denial-of-service bug for `ana-pyproject`'s
        /// never-panic-on-untrusted-input contract (see that crate's
        /// `project.rs` module docs), not merely a `ConvertError::Marker`
        /// case this crate declines to convert.
        ///
        /// **Fixed by a later `uv-pep508` bump** (uv#19782, "Ignore
        /// reversed string compatible-release markers"), which applies
        /// the same "`~=` is not meaningful for strings, ignore it" guard
        /// already in place for the normal-order form to the reversed one
        /// too. Post-fix, `Requirement::from_str` parses this string
        /// successfully and `marker.is_true()` is `true`, so this falls
        /// into the same "silently accepted, not rejected" category as
        /// [`reversed_in_marker_is_silently_accepted_not_rejected`] above.
        /// Pinned here the same way, so a future `uv_pep508` upgrade that
        /// starts treating this shape as a real constraint (or
        /// reintroduces the panic) is a loud test failure, not a silent
        /// regression.
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
        /// values are restricted away (`platform_release`/
        /// `platform_version`, deliberately left out of `assumption` --
        /// see `ana-marker-matchspec`'s own docs) surfaces as
        /// [`ConvertError::UnconvertibleMarker`], not
        /// [`ConvertError::Marker`] -- propagated from
        /// `ana_marker_matchspec::Unconvertible` rather than re-described.
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

        /// A marker combining a known-and-holding key with the free
        /// `python_version` variable produces a real matchspec
        /// `condition`: `sys_platform == "linux"` resolves away (true on
        /// `linux-64`), leaving just the `python_version` residual as the
        /// condition.
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

    /// Reroll's marker *conversion* tests, ported now that
    /// `ana-marker-matchspec` is wired in. A few of reroll's original
    /// expectations don't carry over unchanged, called out individually:
    ///
    /// - Every expectation reroll wrote in terms of a `when=` clause
    ///   containing `__win`/`__unix`/`__osx` (a *virtual package*) has no
    ///   counterpart here: `sys_platform`/`os_name`/`platform_system`/
    ///   `platform_machine` are always fully resolved by `restrict()`
    ///   before a matchspec `condition` is ever built. A marker that's
    ///   *only* one of these keys resolves to `Applicability::Always`/
    ///   `Never` (see the `markers` module above), never a
    ///   virtual-package `condition`. Three of reroll's original tests
    ///   (`test_three_term_and_chain_is_fully_parenthesized`,
    ///   `test_mixed_or_and_precedence_parenthesizes_the_tighter_and_group`,
    ///   `test_explicit_parens_around_an_or_group_are_preserved`) exist
    ///   specifically to pin how virtual-package leaves and
    ///   `python_version` leaves combine and parenthesize in one `when=`
    ///   string; with no virtual-package leaf ever reaching a `condition`
    ///   here, that combining question doesn't arise, so those three
    ///   aren't ported.
    /// - Reroll's `python_version == "3.9"` expectation
    ///   (`python>=3.9.0a0,<3.10.0a0`, an explicit two-clause range) isn't
    ///   what this crate produces: `uv_pep508` itself rewrites
    ///   `python_version =="..."` into a single `EqualStar` operator,
    ///   which converts to conda's fuzzy match (`python=3.9`) instead --
    ///   semantically equivalent, structurally different. Ported with the
    ///   corrected expectation.
    /// - Reroll's `python_full_version >= "3.9.0rc1"` expectation
    ///   (`python>=3.9.0.rc1`, preserving the pre-release) isn't what this
    ///   crate produces either: `uv_pep508` silently drops a marker
    ///   version literal's pre/post/dev segments during parsing (see
    ///   `ana-marker-matchspec`'s
    ///   `prerelease_literal_converts_without_any_allow_pre_concept` for
    ///   the confirming probe). Ported with the corrected expectation.
    /// - `marker_equivalence_oracle_suite` (reroll's 270-line
    ///   `test_marker_matchspec_equivalence.py`) is ported for real: the
    ///   thorough boundary sweep lives in `ana-marker-matchspec`'s own
    ///   `condition::tests::equivalence_oracle` module, next to the
    ///   conversion logic it's exercising, checked against an independent
    ///   PEP 440 comparison, not `uv_pep508::MarkerTree::evaluate()`
    ///   (which has its own unrelated gap for `python_version`).
    ///   `marker_equivalence_oracle_suite` here is a thinner smoke test
    ///   confirming this crate's own public `convert()` surfaces that
    ///   same, already oracle-verified behavior end to end.
    mod markers_deferred {
        use rattler_conda_types::{ParseStrictness, Version, VersionSpec};

        use super::*;

        /// A leaf `MatchSpecCondition` for `python<version_spec>` -- same
        /// construction `ana-marker-matchspec`'s own tests use, ported
        /// here rather than exported from that crate since it's test-only
        /// scaffolding, not part of either crate's real API.
        fn python(version_spec: &str) -> MatchSpecCondition {
            MatchSpecCondition::MatchSpec(Box::new(MatchSpec {
                name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
                version: Some(
                    VersionSpec::from_str(version_spec, ParseStrictness::Lenient).unwrap(),
                ),
                ..MatchSpec::default()
            }))
        }

        /// Ported from reroll's `test_python_version_marker_converts_per_the_table`.
        /// The boundary is anchored at `.0a0` (`ana-marker-matchspec`'s
        /// own `minor_precision`/`convert_specifier` fix) so a
        /// pre-release build of Python 3.9 (`python==3.9.0a0`) is
        /// correctly included, matching reroll's own
        /// `_python_version_condition` exactly.
        #[test]
        fn python_version_marker_converts_per_the_table() {
            let matchspec = convert_ok(&req(r#"requests; python_version >= "3.9""#), false);
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// Ported from reroll's
        /// `test_python_version_equality_marker_produces_a_rattler_valid_when_clause`.
        /// This matches reroll's own expectation exactly
        /// (`python>=3.9.0a0,<3.10.0a0`): matchspec's fuzzy-equals syntax
        /// is deprecated, so this crate never emits it for a
        /// `python_version` equality boundary.
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

        /// Ported from reroll's
        /// `test_python_version_inequality_marker_produces_a_rattler_valid_when_clause`.
        #[test]
        fn python_version_inequality_marker_produces_a_rattler_valid_when_clause() {
            let matchspec = convert_ok(&req(r#"requests; python_version != "3.9""#), false);
            assert_eq!(matchspec.condition, Some(python("!=3.9.*")));
        }

        /// Ported from reroll's `test_combined_marker_preserves_and_or_structure`,
        /// adapted: on `linux-64` (this module's fixed target), `sys_platform ==
        /// "win32"` resolves away to `Applicability::Never`, so this uses
        /// `win-64` instead to keep both sides of the `and` present in the
        /// residual, same as `ana-marker-matchspec`'s own
        /// `combined_marker_preserves_and_or_structure` test.
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

        /// Ported from reroll's `test_reversed_comparison_operand_order_still_converts`.
        #[test]
        fn reversed_comparison_operand_order_still_converts() {
            let matchspec = convert_ok(&req(r#"requests; "3.9" <= python_version"#), false);
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// Ported from reroll's
        /// `test_reversed_virtual_package_operand_order_still_converts`,
        /// adapted: with no virtual-package `condition` in this design,
        /// the reversed operand order is exercised through `restrict()`'s
        /// own resolution instead -- `"win32" == sys_platform` still
        /// resolves to `Applicability::Never` on `linux-64` regardless of
        /// which side of `==` the literal is on.
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

        /// Ported from reroll's
        /// `test_prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre`.
        /// Two compounding divergences from reroll land on this literal:
        /// `uv_pep508` drops the `rc1` pre-release segment during marker
        /// parsing (see `ana-marker-matchspec`'s own
        /// `prerelease_literal_converts_without_any_allow_pre_concept`
        /// for the confirming probe), and the resulting 2-segment literal
        /// (`release=[3, 9]`) is then indistinguishable from a
        /// `python_version` boundary, so it gets that boundary's `.0a0`
        /// anchor too. Accepted as a narrow, documented cost of that
        /// heuristic, not a new bug -- see `ana-marker-matchspec`'s test
        /// for the full reasoning.
        #[test]
        fn prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre() {
            let matchspec = convert_ok(
                &req(r#"requests; python_full_version >= "3.9.0rc1""#),
                false,
            );
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// Ported from reroll's
        /// `test_full_version_glob_marker_produces_a_rattler_valid_when_clause`:
        /// the package-version part (`numpy<1.25.0,>=1.24.0`) and the
        /// marker part (`python_full_version == "3.8.*"`) convert
        /// independently and combine in the same `MatchSpec`. The glob
        /// literal collapses to a 2-segment `python_full_version`
        /// boundary (`"3.8"`), indistinguishable here from a
        /// `python_version` boundary at the same precision, so it gets
        /// the anchored two-clause range rather than reroll's own plain
        /// fuzzy `python=3.8` -- the same accepted, narrow tradeoff as
        /// [`prerelease_literal_in_a_full_version_marker_is_allowed_without_allow_pre`],
        /// not a new bug.
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

        /// Ported from reroll's `test_extras_and_marker_combine_in_one_bracket`,
        /// adapted: `sys_platform == "win32"` resolves away on `linux-64`
        /// (`Applicability::Never`), which would demonstrate dropping the
        /// dependency rather than extras-plus-condition combining, so
        /// this uses a free-variable marker instead to keep both extras
        /// and a real `condition` present together.
        #[test]
        fn extras_and_marker_combine_in_one_bracket() {
            let matchspec =
                convert_ok(&req(r#"fastapi[all]>=1.0; python_version >= "3.9""#), false);
            assert_eq!(matchspec.extras, expect("fastapi[extras=[all]]").extras);
            assert_eq!(matchspec.condition, Some(python(">=3.9.0a0")));
        }

        /// Ported from reroll's
        /// `test_python_version_literal_with_only_a_major_segment_converts`.
        /// Confirmed against reroll's own
        /// `_python_version_condition("==", "3")`
        /// (`python>=3.0.0a0,<3.1.0a0`): a bare major literal normalizes
        /// to minor `0`, so the upper bound is the next *minor* (`3.1`),
        /// not the next major.
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

        /// Ported from reroll's `test_in_marker_converts_via_the_membership_rewrite`,
        /// adapted: no `abi3_upper_bound` needed -- `uv_pep508` expands
        /// the literal's bounds directly, with no network fetch (see
        /// `ana-marker-matchspec`'s own module docs). Both boundaries get
        /// the `.0a0` anchor.
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

        /// Ported from reroll's `test_not_in_marker_converts_via_the_membership_rewrite`.
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

        /// Real, non-`#[ignore]`d smoke test for reroll's
        /// `test_marker_matchspec_equivalence.py`: the thorough
        /// boundary-crossing sweep (against an independent PEP 440
        /// comparison, not `uv_pep508::MarkerTree::evaluate()`) lives in
        /// `ana-marker-matchspec`'s own `condition::tests::equivalence_oracle`
        /// module, next to the conversion logic it actually exercises.
        /// This confirms this crate's own public `convert()` surfaces
        /// that same, already oracle-verified behavior end to end,
        /// including the `.0a0` pre-release anchor: a pre-release build
        /// of the boundary minor (`python==3.9.0a0`) must satisfy
        /// `python_version >= "3.9"`, and a version below the boundary
        /// minor entirely must not.
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

        /// `pypi_to_conda_map` reaches every element, on both the
        /// sequential and `rayon` paths -- exercised here (rather than
        /// only at the single-[`convert`] level) so a future change that
        /// forwards it to one dispatch branch but not the other would
        /// show up as a failure on whichever side has enough requirements
        /// to cross [`PARALLEL_CONVERT_THRESHOLD`].
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
