//! PEP 508 requirements (plus `requires-python`) -> canonical matchspecs
//! for an arbitrary target platform.
//!
//! Conversion is a pure function of the target [`Platform`] (only
//! *solving* needs the network), so this crate can compute "what would
//! `ana` convert this project's requirements to on platform P" for any P,
//! offline.
//!
//! `requires-python` is converted to a `python` matchspec here too,
//! folded into the same requirement list under [`REQUIRES_PYTHON_SOURCE`]
//! rather than handled specially downstream.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod error;

use ana_dependency::{Dependency, SelectedRequirement};
use ana_pep508_to_matchspec::convert_all;
use ana_pypi_conda_map::MappingHandle;
use rattler_conda_types::{MatchSpec, PackageName, PackageNameMatcher, Platform};
use uv_pep440::VersionSpecifiers;
use uv_pep508::Requirement;

pub use error::Error;

/// One requirement's conversion to a matchspec, named so it can cross a
/// crate boundary instead of staying an anonymous tuple: the package
/// name, the canonical matchspec string, the typed [`MatchSpec`] itself,
/// and where the requirement came from (`"runtime"` / `"group:<name>"` /
/// [`REQUIRES_PYTHON_SOURCE`]).
#[derive(Debug, Clone)]
pub struct MatchspecEntry {
    pub name: String,
    pub canonical: String,
    pub spec: MatchSpec,
    pub source: String,
}

/// One requirement a platform section was solved from: the canonical
/// matchspec string ([`MatchSpec`]'s `Display`), plus where it came from
/// (`source` -- `"runtime"`, `"group:<name>"`, or
/// [`REQUIRES_PYTHON_SOURCE`] for the `python` matchspec `requires-python`
/// derives; informational only, never part of a staleness comparison,
/// which is a pure set diff on matchspec strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedRequirement {
    pub matchspec: String,
    pub source: String,
}

/// The conversion result, in the two forms downstream needs: typed
/// specs for the solver, and the locked entries for the lock file.
pub struct ConvertedRequirements {
    /// Typed matchspecs, in the same order as [`locked`](Self::locked).
    /// One entry per `selected` entry (plus `requires-python`, if
    /// present); never deduplicated, even if two entries share a
    /// canonical string.
    pub specs: Vec<MatchSpec>,
    /// Canonical matchspec strings with their sources, sorted by package
    /// name, then canonical string, then source.
    pub locked: Vec<LockedRequirement>,
}

/// The `source` value recorded for the `python` matchspec `requires-python`
/// derives. Distinct from any real declaration source string, so it
/// never collides.
pub const REQUIRES_PYTHON_SOURCE: &str = "requires-python";

/// The platform-independent half of matchspec conversion: every
/// `Dependency::Matchspec` entry in `selected`, converted to its
/// [`MatchspecEntry`] form.
///
/// Callers converting the same `selected` for multiple platforms should
/// compute this once and reuse it via
/// [`convert_for_platform_with_matchspec_entries`] rather than
/// recomputing it per platform.
pub fn matchspec_entries(selected: &[SelectedRequirement<'_>]) -> Vec<MatchspecEntry> {
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
                Some(MatchspecEntry {
                    name,
                    canonical,
                    spec: spec.as_ref().clone(),
                    source: s.source.clone(),
                })
            }
        })
        .collect()
}

/// Converts `selected` (plus `requires_python`, if the caller declares
/// one) to matchspecs as seen on `platform`, taking the
/// platform-independent `Dependency::Matchspec` conversion already
/// computed (see [`matchspec_entries`]) rather than re-deriving it.
///
/// A PEP 508 requirement whose marker can never hold on `platform` (e.g.
/// a win32-only dependency while targeting linux-64) is dropped, not an
/// error. Genuine conversion failures are aggregated into one error
/// listing every failing requirement, rather than failing fast on the
/// first.
///
/// No deduplication: two entries with the same canonical matchspec string
/// but different sources both appear in the output.
pub fn convert_for_platform_with_matchspec_entries(
    matchspec_entries: &[MatchspecEntry],
    selected: &[SelectedRequirement<'_>],
    requires_python: Option<&VersionSpecifiers>,
    platform: Platform,
    pypi_to_conda_map: &MappingHandle,
) -> Result<ConvertedRequirements, Error> {
    let assumption = ana_marker_matchspec::known_values_assumption(platform)?;

    let mut failures = Vec::new();
    let mut entries: Vec<MatchspecEntry> =
        Vec::with_capacity(matchspec_entries.len() + selected.len() + 1);
    entries.extend(matchspec_entries.iter().cloned());

    let pep508_entries: Vec<(&SelectedRequirement<'_>, &Requirement)> = selected
        .iter()
        .filter_map(|s| match s.dependency {
            Dependency::Pep508(requirement) => Some((s, requirement)),
            Dependency::Matchspec(_) => None,
        })
        .collect();

    // `allow_pre = false`: a pre-release package version is never
    // accepted just because the specifier didn't forbid it.
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
                entries.push(MatchspecEntry {
                    name,
                    canonical,
                    spec,
                    source: selected.source.clone(),
                });
            }
            Ok(None) => {}
            Err(err) => {
                failures.push(format!("  {requirement} (from {}): {err}", selected.source));
            }
        }
    }

    // `requires-python` has no name or marker, so it goes through
    // `version_spec` directly rather than `convert_all`.
    if let Some(requires_python) = requires_python {
        match ana_pep508_to_matchspec::version_spec(requires_python, false) {
            Ok(Some(version)) => {
                let spec = MatchSpec {
                    name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
                    version: Some(version),
                    ..MatchSpec::default()
                };
                let canonical = spec.to_string();
                entries.push(MatchspecEntry {
                    name: "python".to_string(),
                    canonical,
                    spec,
                    source: REQUIRES_PYTHON_SOURCE.to_string(),
                });
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
        a.name
            .cmp(&b.name)
            .then_with(|| a.canonical.cmp(&b.canonical))
            .then_with(|| a.source.cmp(&b.source))
    });

    let specs: Vec<MatchSpec> = entries.iter().map(|e| e.spec.clone()).collect();
    let locked = entries
        .into_iter()
        .map(|e| LockedRequirement {
            matchspec: e.canonical,
            source: e.source,
        })
        .collect();
    Ok(ConvertedRequirements { specs, locked })
}

/// The one-shot form of conversion a content key needs: `dependencies`
/// (plus `requires_python`, if any) converted to canonical matchspec
/// strings for `platform`, with no distinct sources to track (every
/// entry is tagged with the same placeholder source internally, which
/// never appears in the output). Every requirement is treated as a
/// single, unnamed group -- there is no persistent declaration to diff
/// this against later, unlike [`convert_for_platform_with_matchspec_entries`].
pub fn canonical_matchspecs(
    dependencies: &[Dependency],
    requires_python: Option<&VersionSpecifiers>,
    platform: Platform,
    pypi_to_conda_map: &MappingHandle,
) -> Result<Vec<String>, Error> {
    const CONTENT_KEY_SOURCE: &str = "content-key";
    let selected: Vec<SelectedRequirement<'_>> = dependencies
        .iter()
        .map(|dependency| SelectedRequirement {
            dependency,
            source: CONTENT_KEY_SOURCE.to_string(),
        })
        .collect();
    let entries = matchspec_entries(&selected);
    let converted = convert_for_platform_with_matchspec_entries(
        &entries,
        &selected,
        requires_python,
        platform,
        pypi_to_conda_map,
    )?;
    Ok(converted
        .locked
        .into_iter()
        .map(|entry| entry.matchspec)
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::{BTreeSet, HashMap};
    use std::str::FromStr;

    use super::*;

    /// A `MappingHandle` with no entries, for tests that don't care about
    /// name mapping.
    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    /// Test-only convenience combining [`matchspec_entries`] and
    /// [`convert_for_platform_with_matchspec_entries`] into one call.
    fn convert_for_platform(
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

    fn pep508_deps(reqs: &[&str]) -> Vec<Dependency> {
        reqs.iter()
            .map(|r| Dependency::Pep508(Requirement::from_str(r).unwrap()))
            .collect()
    }

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

    /// Wraps `deps` as `SelectedRequirement`s with source `"runtime"`.
    /// `deps` must outlive the returned borrow.
    fn selected(deps: &[Dependency]) -> Vec<SelectedRequirement<'_>> {
        deps.iter()
            .map(|dependency| SelectedRequirement {
                dependency,
                source: "runtime".to_string(),
            })
            .collect()
    }

    /// Same as [`selected`], with an explicit `source`.
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
        let deps = pep508_deps(&["numpy", "pywin32; sys_platform == 'win32'"]);
        let selected = selected(&deps);
        let linux =
            convert_for_platform(&selected, None, Platform::Linux64, &no_mapping()).unwrap();
        assert_eq!(linux.locked.len(), 1);
        assert_eq!(linux.locked[0].matchspec, "numpy");

        let windows =
            convert_for_platform(&selected, None, Platform::Win64, &no_mapping()).unwrap();
        assert_eq!(windows.locked.len(), 2);
    }

    /// The same package pinned in both `runtime` and a group is not
    /// collapsed into one entry; per [PEP 735](https://peps.python.org/pep-0735/),
    /// duplicate requirements across sources are kept as independent
    /// constraints, not deduplicated by precedence.
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
    /// `Dependency::Matchspec` entries.
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

    /// A name present in the pypi-to-conda mapping table is replaced in
    /// both `locked` and `specs`.
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

    /// A name absent from the mapping table is unaffected, even with an
    /// unrelated non-empty table.
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

    #[test]
    fn canonical_matchspecs_returns_sorted_canonical_strings() {
        let deps = pep508_deps(&["numpy>=1.20", "ruff"]);
        let strings = canonical_matchspecs(&deps, None, Platform::Linux64, &no_mapping()).unwrap();
        assert_eq!(strings, vec!["numpy >=1.20", "ruff"]);
    }

    #[test]
    fn canonical_matchspecs_includes_requires_python() {
        let requires_python = VersionSpecifiers::from_str(">=3.9").unwrap();
        let deps = pep508_deps(&["numpy"]);
        let strings = canonical_matchspecs(
            &deps,
            Some(&requires_python),
            Platform::Linux64,
            &no_mapping(),
        )
        .unwrap();
        assert!(strings.contains(&"python >=3.9".to_string()), "{strings:?}");
    }

    #[test]
    fn canonical_matchspecs_surfaces_conversion_failures() {
        let deps = pep508_deps(&["numpy @ https://example.com/numpy.whl"]);
        let err = canonical_matchspecs(&deps, None, Platform::Linux64, &no_mapping()).unwrap_err();
        assert!(matches!(err, Error::Conversion(_)));
    }
}
