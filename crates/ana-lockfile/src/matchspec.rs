//! PEP 508 requirements (plus `requires-python`) -> canonical matchspecs
//! for an arbitrary target platform.
//!
//! The conversion pipeline (`ana_marker_matchspec::known_values_assumption`
//! and `ana_pep508_to_matchspec::convert_all`) is a pure function of the
//! target [`Platform`], so this module can compute "what would `ana`
//! convert this project's requirements to on platform P" for any P, from
//! any machine, offline. Every mode funnels through
//! [`convert_for_platform`]; only *solving* needs the network.
//!
//! `requires-python` is converted to a `python` matchspec here too, folded
//! into the same requirement list as every other requirement (with its
//! own `source`, [`REQUIRES_PYTHON_SOURCE`]) rather than handled specially
//! downstream: the solver just sees `python` as an ordinary package
//! constraint, with no separate field to keep in sync.

use ana_dependency::Dependency;
use ana_pep508_to_matchspec::convert_all;
use ana_pypi_conda_map::MappingHandle;
use rattler_conda_types::{MatchSpec, PackageName, PackageNameMatcher, Platform};
use uv_pep440::VersionSpecifiers;
use uv_pep508::Requirement;

use crate::error::Error;
use crate::lock_file::LockedRequirement;
use crate::project::SelectedRequirement;

/// The conversion result, in the two forms the algorithm needs: typed
/// specs for the solver, and the locked entries for the file (also used
/// for the plain set-diff staleness check).
pub(crate) struct ConvertedRequirements {
    /// Typed matchspecs, in the same order as [`locked`] -- the solver
    /// only ever sees a flat spec list, with no distinction between an
    /// ordinary requirement and the `python` matchspec `requires-python`
    /// derives. One entry per `selected` entry (plus `requires-python`,
    /// if present) -- see the module docs for why nothing here is
    /// deduplicated.
    pub specs: Vec<MatchSpec>,
    /// Canonical matchspec strings with their sources, sorted by package
    /// name, then canonical string, then source. One entry per
    /// `selected` entry (plus `requires-python`, if present): two entries
    /// that happen to share a canonical matchspec string (e.g. the same
    /// package pinned in both a group and `runtime`) both appear here
    /// rather than one replacing the other -- see the module docs.
    pub locked: Vec<LockedRequirement>,
}

/// The `source` value recorded for the `python` matchspec `requires-python`
/// derives -- distinct from `crate::project::RUNTIME_SOURCE` and any
/// `"group:<name>"` string, so it can never collide with a real
/// `pyproject.toml` requirement's own source.
const REQUIRES_PYTHON_SOURCE: &str = "requires-python";

/// Convert `selected` (plus `requires_python`, if the project declares
/// one) to matchspecs as seen on `platform`.
///
/// `selected` entries carry either a PEP 508 requirement or a conda
/// `MatchSpec` (see [`Dependency`]). Only the PEP 508 ones go through
/// [`convert_all`] -- that's the only conversion that needs `platform`'s
/// marker assumption and the only one with a marker-driven drop case or a
/// genuine failure mode. A `Dependency::Matchspec` entry is already a
/// valid, platform-independent conda spec (it carries no PEP 508 marker
/// to evaluate), so it's copied straight into the output, with no
/// conversion step and no way for it to fail or be dropped.
///
/// A PEP 508 requirement whose marker can never hold on `platform` (e.g.
/// a win32-only dependency while targeting linux-64) is dropped, not an
/// error -- that's `convert`'s `Ok(None)` case. Genuine conversion
/// failures are aggregated into one error listing every failing
/// requirement (and `requires_python`, if that's what failed), rather
/// than failing fast on the first.
///
/// No deduplication -- see the module docs.
///
/// Computes [`matchspec_entries`] fresh every call -- fine for the
/// single-platform callers (`ensure_current_platform_locked`,
/// `lock_platform`), but a caller converting the same `selected` for
/// several platforms (`check`'s loop) should call [`matchspec_entries`]
/// once and drive [`convert_for_platform_with_matchspec_entries`]
/// directly instead, rather than re-deriving the platform-independent
/// half of the output once per platform for no reason.
///
/// `pypi_to_conda_map` is forwarded to `ana_pep508_to_matchspec::convert_all`
/// unchanged -- see that crate's docs. Always a real handle, never
/// optional -- see `ana_pep508_to_matchspec::convert`'s own docs for why.
/// A handle wrapping an empty table means every PyPI name is kept as-is
/// (no lookup finds anything), the test-only case (`MappingHandle::from_map(HashMap::new())`);
/// every real caller passes the `ana-pypi-conda-map::MappingHandle` it
/// actually loaded.
pub(crate) fn convert_for_platform(
    selected: &[SelectedRequirement<'_>],
    requires_python: Option<&VersionSpecifiers>,
    platform: Platform,
    pypi_to_conda_map: &MappingHandle,
) -> Result<ConvertedRequirements, Error> {
    convert_for_platform_with_matchspec_entries(
        &matchspec_entries(selected),
        selected,
        requires_python,
        platform,
        pypi_to_conda_map,
    )
}

/// The platform-independent half of [`convert_for_platform`]: every
/// `Dependency::Matchspec` entry in `selected`, converted to its `(name,
/// canonical matchspec string, spec, source)` form. `Dependency::Matchspec`
/// carries no PEP 508 marker to evaluate against a target platform, so
/// this has no dependence on `platform` at all -- see the module docs.
///
/// Callers converting the same `selected` for multiple platforms should
/// compute this once and reuse it via
/// [`convert_for_platform_with_matchspec_entries`], instead of paying for
/// the same `Display`-formatting, name-derivation, and `MatchSpec` clone
/// once per platform for output that's byte-identical every time.
pub(crate) fn matchspec_entries(
    selected: &[SelectedRequirement<'_>],
) -> Vec<(String, String, MatchSpec, String)> {
    selected
        .iter()
        .filter_map(|s| match s.dependency {
            Dependency::Pep508(_) => None,
            Dependency::Matchspec(spec) => {
                let canonical = spec.to_string();
                let name = spec
                    .name
                    .as_exact()
                    .map(|name| name.as_normalized().to_string())
                    .unwrap_or_else(|| canonical.clone());
                Some((name, canonical, spec.as_ref().clone(), s.source.clone()))
            }
        })
        .collect()
}

/// Like [`convert_for_platform`], but takes the platform-independent
/// `Dependency::Matchspec` conversion already computed (see
/// [`matchspec_entries`]) rather than re-deriving it. Every `entry` is
/// cloned into this call's own `entries` -- one clone per platform is
/// unavoidable given `ConvertedRequirements`' owned fields (see the
/// module docs on why nothing here is deduplicated/shared across
/// platforms), but this at least skips redoing the `Display`/name
/// lookup/`MatchSpec` derivation itself once per platform.
pub(crate) fn convert_for_platform_with_matchspec_entries(
    matchspec_entries: &[(String, String, MatchSpec, String)],
    selected: &[SelectedRequirement<'_>],
    requires_python: Option<&VersionSpecifiers>,
    platform: Platform,
    pypi_to_conda_map: &MappingHandle,
) -> Result<ConvertedRequirements, Error> {
    let assumption = ana_marker_matchspec::known_values_assumption(platform)?;

    let mut failures = Vec::new();
    let mut entries: Vec<(String, String, MatchSpec, String)> =
        Vec::with_capacity(matchspec_entries.len() + selected.len() + 1);
    entries.extend(matchspec_entries.iter().cloned());

    // `Dependency::Matchspec` entries are already in `entries` above;
    // only the PEP 508 ones need converting here.
    let pep508_entries: Vec<(&SelectedRequirement<'_>, &Requirement)> = selected
        .iter()
        .filter_map(|s| match s.dependency {
            Dependency::Pep508(requirement) => Some((s, requirement)),
            Dependency::Matchspec(_) => None,
        })
        .collect();

    // `allow_pre = false`: reroll's default policy, unchanged -- a
    // pre-release *package* version is never accepted just because the
    // specifier didn't forbid it. `convert_all` borrows, so this is a Vec
    // of references, not a deep clone of every requirement.
    let requirements: Vec<&Requirement> = pep508_entries.iter().map(|(_, req)| *req).collect();
    let converted = convert_all(&requirements, false, assumption, pypi_to_conda_map);

    for ((selected, requirement), outcome) in pep508_entries.iter().zip(converted) {
        match outcome {
            Ok(Some(spec)) => {
                let canonical = spec.to_string();
                let name = spec
                    .name
                    .as_exact()
                    .map(|name| name.as_normalized().to_string())
                    .unwrap_or_else(|| canonical.clone());
                entries.push((name, canonical, spec, selected.source.clone()));
            }
            Ok(None) => {}
            Err(err) => {
                failures.push(format!("  {requirement} (from {}): {err}", selected.source));
            }
        }
    }

    // `requires-python` isn't a PEP 508 requirement (no name, no marker --
    // just a bare PEP 440 specifier set), so it doesn't go through
    // `convert_all` above; it gets the exact same PEP 440 -> conda
    // `VersionSpec` conversion (`ana_pep508_to_matchspec::version_spec`)
    // every `python_version` marker in this workspace already goes
    // through, applied directly to a `python` matchspec. `allow_pre =
    // false`: the same policy as every other conversion in this function.
    if let Some(requires_python) = requires_python {
        match ana_pep508_to_matchspec::version_spec(requires_python, false) {
            Ok(Some(version)) => {
                let spec = MatchSpec {
                    name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
                    version: Some(version),
                    ..MatchSpec::default()
                };
                let canonical = spec.to_string();
                entries.push((
                    "python".to_string(),
                    canonical,
                    spec,
                    REQUIRES_PYTHON_SOURCE.to_string(),
                ));
            }
            Ok(None) => {}
            Err(err) => {
                failures.push(format!("  {REQUIRES_PYTHON_SOURCE}: {err}"));
            }
        }
    }

    if !failures.is_empty() {
        return Err(Error::Conversion(failures.join("\n")));
    }

    entries.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.3.cmp(&b.3))
    });

    let specs: Vec<MatchSpec> = entries.iter().map(|(_, _, spec, _)| spec.clone()).collect();
    let locked = entries
        .into_iter()
        .map(|(_, canonical, _, source)| LockedRequirement {
            matchspec: canonical,
            source,
        })
        .collect();
    Ok(ConvertedRequirements { specs, locked })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::{BTreeSet, HashMap};
    use std::str::FromStr;

    use super::*;

    /// A `MappingHandle` with no entries -- the required-but-irrelevant
    /// mapping table for tests that don't care about name mapping at
    /// all.
    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    /// Build `Dependency::Pep508` entries from PEP 508 requirement strings.
    fn pep508_deps(reqs: &[&str]) -> Vec<Dependency> {
        reqs.iter()
            .map(|r| Dependency::Pep508(Requirement::from_str(r).unwrap()))
            .collect()
    }

    /// Build `Dependency::Matchspec` entries from conda `MatchSpec`
    /// strings (bypassing PEP 508 entirely), for exercising the
    /// `Dependency::Matchspec` path through [`convert_for_platform`].
    fn matchspec_deps(specs: &[&str]) -> Vec<Dependency> {
        specs
            .iter()
            .map(|s| {
                Dependency::Matchspec(Box::new(
                    MatchSpec::from_str(s, rattler_conda_types::ParseMatchSpecOptions::lenient())
                        .unwrap(),
                ))
            })
            .collect()
    }

    /// Wrap already-built `Dependency`s as `SelectedRequirement`s with a
    /// `"runtime"` source -- `deps` must outlive the returned borrow.
    fn selected(deps: &[Dependency]) -> Vec<SelectedRequirement<'_>> {
        deps.iter()
            .map(|dependency| SelectedRequirement {
                dependency,
                source: "runtime".to_string(),
            })
            .collect()
    }

    /// Same as [`selected`], with an explicit `source` rather than the
    /// `"runtime"` default.
    fn selected_with_source<'p>(
        deps: &'p [Dependency],
        source: &str,
    ) -> Vec<SelectedRequirement<'p>> {
        deps.iter()
            .map(|dependency| SelectedRequirement {
                dependency,
                source: source.to_string(),
            })
            .collect()
    }

    #[test]
    fn converts_and_canonicalizes() {
        let deps = pep508_deps(&["numpy>=1.20", "ruff"]);
        let converted =
            convert_for_platform(&selected(&deps), None, Platform::Linux64, &no_mapping()).unwrap();
        let strings: Vec<&str> = converted
            .locked
            .iter()
            .map(|r| r.matchspec.as_str())
            .collect();
        assert_eq!(strings, vec!["numpy >=1.20", "ruff"]);
        assert_eq!(converted.specs.len(), 2);
    }

    #[test]
    fn foreign_platform_markers_resolve_without_host_detection() {
        // A win32-only requirement drops out of a linux-64 conversion...
        let deps = pep508_deps(&["numpy", "pywin32; sys_platform == 'win32'"]);
        let selected = selected(&deps);
        let linux =
            convert_for_platform(&selected, None, Platform::Linux64, &no_mapping()).unwrap();
        assert_eq!(linux.locked.len(), 1);
        assert_eq!(linux.locked[0].matchspec, "numpy");

        // ...and is present when targeting win-64, computed from this
        // (non-Windows) host -- the whole point of the pure conversion.
        let windows =
            convert_for_platform(&selected, None, Platform::Win64, &no_mapping()).unwrap();
        assert_eq!(windows.locked.len(), 2);
    }

    /// The same package pinned in both `runtime` and a group (e.g.
    /// `[project.dependencies]` and a `[dependency-groups]` entry that
    /// re-lists it) is *not* collapsed into one entry, and neither source
    /// takes precedence over the other. [PEP 735](https://peps.python.org/pep-0735/)
    /// requires this for its own dependency-group includes -- "Tools
    /// SHOULD NOT deduplicate or otherwise alter the list contents...
    /// Tools should handle such a list exactly as they would handle any
    /// other case in which they are asked to process the same
    /// requirement multiple times with different version constraints" --
    /// and this module applies the same rule across every source, not
    /// just within one group's own includes: every specifier collected
    /// from every source is an independent constraint on one solve (the
    /// same way `pip`/`uv` treat a package that's both a direct and a
    /// transitive dependency), not a precedence question this function
    /// should settle by picking a winner.
    #[test]
    fn duplicate_requirements_from_different_sources_are_both_kept() {
        let deps = pep508_deps(&["numpy>=1.20"]);
        let mut selected = selected(&deps);
        selected.push(SelectedRequirement {
            dependency: &deps[0],
            source: "group:dev".to_string(),
        });
        let converted =
            convert_for_platform(&selected, None, Platform::Linux64, &no_mapping()).unwrap();

        assert_eq!(
            converted.locked.len(),
            2,
            "both sources' requirements are kept, not collapsed into one"
        );
        assert!(
            converted
                .locked
                .iter()
                .all(|r| r.matchspec == "numpy >=1.20"),
            "both entries carry the same (jointly-satisfiable) constraint"
        );
        let sources: BTreeSet<&str> = converted.locked.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(
            sources,
            BTreeSet::from(["runtime", "group:dev"]),
            "neither source's copy is dropped in favor of the other"
        );
        assert_eq!(
            converted.specs.len(),
            2,
            "the solver sees both constraints, exactly as it would for a package that's both a \
             direct and a transitive dependency"
        );
    }

    #[test]
    fn requires_python_becomes_a_locked_requirement_with_its_own_source() {
        // `requires-python` is solved like any other package (no
        // separate solver-side handling) and is an ordinary entry in
        // `locked`/`ana.lock`'s own `requirements`, distinguished only by
        // its `source`: there is no separate
        // `PlatformSection::requires_python` field to skip it for.
        let requires_python = VersionSpecifiers::from_str(">=3.9").unwrap();
        let deps = pep508_deps(&["numpy>=1.20"]);
        let converted = convert_for_platform(
            &selected(&deps),
            Some(&requires_python),
            Platform::Linux64,
            &no_mapping(),
        )
        .unwrap();

        assert_eq!(
            converted.locked.len(),
            2,
            "python is an ordinary locked entry"
        );
        assert_eq!(converted.specs.len(), 2);
        let python = converted
            .locked
            .iter()
            .find(|req| req.source == REQUIRES_PYTHON_SOURCE)
            .expect("a requires-python-sourced requirement was recorded");
        assert_eq!(python.matchspec, "python >=3.9");

        let python_spec = converted
            .specs
            .iter()
            .find(|spec| spec.name.as_exact().map(|n| n.as_normalized()) == Some("python"))
            .expect("a python matchspec was produced");
        let version_spec = python_spec
            .version
            .as_ref()
            .expect("python carries a version");
        assert!(version_spec.matches(&rattler_conda_types::Version::from_str("3.9.0").unwrap()));
        assert!(!version_spec.matches(&rattler_conda_types::Version::from_str("3.8.0").unwrap()));
    }

    #[test]
    fn no_requires_python_means_no_python_spec() {
        let deps = pep508_deps(&["numpy"]);
        let converted =
            convert_for_platform(&selected(&deps), None, Platform::Linux64, &no_mapping()).unwrap();
        assert_eq!(converted.specs.len(), 1);
    }

    #[test]
    fn conversion_failures_are_aggregated() {
        let deps = pep508_deps(&["numpy @ https://example.com/numpy.whl", "also @ file:///x"]);
        let converted =
            convert_for_platform(&selected(&deps), None, Platform::Linux64, &no_mapping());
        match converted {
            Err(Error::Conversion(message)) => {
                assert!(message.contains("numpy @"), "{message}");
                assert!(message.contains("also @"), "{message}");
            }
            other => panic!("expected conversion error, got {}", {
                if other.is_ok() {
                    "ok"
                } else {
                    "different error"
                }
            }),
        }
    }

    #[test]
    fn matchspec_entries_pass_through_without_conversion() {
        // A `Dependency::Matchspec` entry needs no marker evaluation and
        // has no failure mode -- it's already a valid, platform-
        // independent spec, so it appears in the output unchanged
        // regardless of `platform`.
        let deps = matchspec_deps(&["compilers", "cmake >=3.20"]);
        let converted = convert_for_platform(
            &selected_with_source(&deps, "group:build"),
            None,
            Platform::Linux64,
            &no_mapping(),
        )
        .unwrap();
        let strings: Vec<&str> = converted
            .locked
            .iter()
            .map(|r| r.matchspec.as_str())
            .collect();
        assert_eq!(strings, vec!["cmake >=3.20", "compilers"]);
        assert!(converted.locked.iter().all(|r| r.source == "group:build"));
        assert_eq!(converted.specs.len(), 2);
    }

    #[test]
    fn pep508_and_matchspec_entries_merge_and_sort() {
        // A PEP 508 runtime requirement and a conda-only group dependency
        // both end up in the same output, sorted together by name.
        let pep508 = pep508_deps(&["ruff"]);
        let matchspec = matchspec_deps(&["compilers"]);
        let mut selected = selected(&pep508);
        selected.extend(selected_with_source(&matchspec, "group:build"));
        let converted =
            convert_for_platform(&selected, None, Platform::Linux64, &no_mapping()).unwrap();
        let summary: Vec<(&str, &str)> = converted
            .locked
            .iter()
            .map(|r| (r.matchspec.as_str(), r.source.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![("compilers", "group:build"), ("ruff", "runtime")]
        );
    }

    /// Same guarantee as
    /// `duplicate_requirements_from_different_sources_are_both_kept`, for
    /// two `Dependency::Matchspec` entries rather than two PEP 508 ones --
    /// the conda-only side of the unified `Dependency` graph follows the
    /// same PEP 735 "process the same requirement multiple times with
    /// different version constraints" rule, not a precedence rule,
    /// regardless of which `Dependency` variant is involved.
    #[test]
    fn duplicate_matchspec_entries_from_different_sources_are_both_kept() {
        let deps = matchspec_deps(&["numpy >=1.26"]);
        let mut selected = selected_with_source(&deps, "runtime");
        selected.extend(selected_with_source(&deps, "group:dev"));
        let converted =
            convert_for_platform(&selected, None, Platform::Linux64, &no_mapping()).unwrap();

        assert_eq!(converted.locked.len(), 2);
        assert!(converted
            .locked
            .iter()
            .all(|r| r.matchspec == "numpy >=1.26"));
        let sources: BTreeSet<&str> = converted.locked.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(sources, BTreeSet::from(["runtime", "group:dev"]));
        assert_eq!(converted.specs.len(), 2);
    }

    /// The pypi-to-conda mapping table reaches PEP 508 requirements all
    /// the way through this crate's own conversion entry point, not just
    /// at `ana-pep508-to-matchspec`'s own level: a name present in the
    /// table is replaced in both `locked`'s canonical string and
    /// `specs`' typed `MatchSpec`.
    #[test]
    fn pypi_to_conda_map_is_applied_through_convert_for_platform() {
        let deps = pep508_deps(&["opencv-python>=4.0"]);
        let handle = MappingHandle::from_map(HashMap::from([(
            "opencv-python".to_string(),
            "py-opencv".to_string(),
        )]));
        let converted =
            convert_for_platform(&selected(&deps), None, Platform::Linux64, &handle).unwrap();

        assert_eq!(converted.locked.len(), 1);
        assert_eq!(converted.locked[0].matchspec, "py-opencv >=4.0");
        assert_eq!(
            converted.specs[0]
                .name
                .as_exact()
                .map(|n| n.as_normalized()),
            Some("py-opencv")
        );
    }

    /// A name absent from the table is unaffected, even with a (non-empty,
    /// unrelated) table in hand -- same guarantee as
    /// `ana-pep508-to-matchspec`'s own `unmapped_name_...` test, checked
    /// again here since this crate's `convert_for_platform` is a distinct
    /// public entry point that could in principle drop the parameter on
    /// the way through.
    #[test]
    fn unmapped_name_is_unaffected_by_an_unrelated_table() {
        let deps = pep508_deps(&["numpy>=1.20"]);
        let handle = MappingHandle::from_map(HashMap::from([(
            "opencv-python".to_string(),
            "py-opencv".to_string(),
        )]));
        let converted =
            convert_for_platform(&selected(&deps), None, Platform::Linux64, &handle).unwrap();

        assert_eq!(converted.locked[0].matchspec, "numpy >=1.20");
    }
}
