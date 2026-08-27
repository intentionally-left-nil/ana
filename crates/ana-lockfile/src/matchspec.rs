//! PEP 508 requirements (plus `requires-python`) -> canonical matchspecs
//! for an arbitrary target platform.
//!
//! This is the "key enabling fact" of
//! `investigations/lock_generation_algorithm.md` made concrete: the
//! conversion pipeline (`ana_marker_matchspec::known_values_assumption` +
//! `ana_pep508_to_matchspec::convert_all`) is a pure function of the target
//! [`Platform`], so this module can compute "what would `ana` convert this
//! project's requirements to on platform P" for any P, from any machine,
//! offline. Every mode of the algorithm -- default, cross-platform, and CI
//! check -- funnels through [`convert_for_platform`]; only *solving* needs
//! the network, and solving is not this module's job.
//!
//! `requires-python` is converted to a `python` matchspec right here too,
//! alongside every other requirement, rather than downstream in the
//! solver: as far as conda (and the solver crate behind [`crate::Solver`])
//! is concerned, `python` is just an ordinary package, and the solver has
//! no business knowing that `requires-python` is the `pyproject.toml` key
//! that happened to produce this particular constraint on it. This module
//! already owns "pyproject-derived data -> canonical matchspec" for every
//! other requirement; `requires-python` is not a special case of that, so
//! it doesn't get a special code path.

use std::collections::BTreeMap;

use ana_pep508_to_matchspec::convert_all;
use rattler_conda_types::{MatchSpec, PackageName, PackageNameMatcher, Platform};
use uv_pep440::VersionSpecifiers;

use crate::error::Error;
use crate::lock_file::LockedRequirement;
use crate::project::SelectedRequirement;

/// The conversion result, in the three forms the algorithm needs:
/// typed specs for the solver, locked entries for the file, and the bare
/// canonical strings for the stage-2 set diff.
pub(crate) struct ConvertedRequirements {
    /// Typed matchspecs, in the same order as [`locked`], plus the
    /// `python` matchspec `requires_python` implies (if any) -- the
    /// solver only ever sees a flat spec list, never a `requires-python`
    /// value of its own. Not reflected in [`locked`]/`ana.lock`'s own
    /// `requirements`: `PlatformSection::requires_python` remains its own
    /// lock-file field (see that type's docs for why), this is purely
    /// what gets solved.
    pub specs: Vec<MatchSpec>,
    /// Canonical matchspec strings with their sources, sorted by package
    /// name then string, deduplicated by canonical string (first source
    /// wins -- runtime is always selected before groups, so it wins ties).
    pub locked: Vec<LockedRequirement>,
}

/// The literal PEP 621 key `requires-python` derives from, used only as a
/// diagnostic label if its conversion fails -- it is never a real
/// `pyproject.toml` requirement `source`, so it can't collide with
/// `crate::project::RUNTIME_SOURCE` or a `"group:<name>"` string.
const REQUIRES_PYTHON_LABEL: &str = "requires-python";

/// Convert `selected` (plus `requires_python`, if the project declares
/// one) to matchspecs as seen on `platform`.
///
/// A requirement whose marker can never hold on `platform` (e.g. a
/// win32-only dependency while targeting linux-64) is dropped, not an
/// error -- that's `convert`'s `Ok(None)` case. Genuine conversion
/// failures are aggregated into one error listing every failing
/// requirement (and `requires_python`, if that's what failed), rather
/// than failing fast on the first.
pub(crate) fn convert_for_platform(
    selected: &[SelectedRequirement],
    requires_python: Option<&VersionSpecifiers>,
    platform: Platform,
) -> Result<ConvertedRequirements, Error> {
    let assumption = ana_marker_matchspec::known_values_assumption(platform)?;

    // `allow_pre = false`: reroll's default policy, unchanged -- a
    // pre-release *package* version is never accepted just because the
    // specifier didn't forbid it. `convert_all` borrows, so this is a Vec
    // of references, not a deep clone of every requirement.
    let requirements: Vec<&uv_pep508::Requirement> =
        selected.iter().map(|s| &s.requirement).collect();
    let converted = convert_all(&requirements, false, assumption);

    let mut failures = Vec::new();
    // Keyed by canonical string so duplicates dedupe; value is
    // (sort key, spec, source).
    let mut deduped: BTreeMap<String, (String, MatchSpec, String)> = BTreeMap::new();
    for (selected, outcome) in selected.iter().zip(converted) {
        match outcome {
            Ok(Some(spec)) => {
                let canonical = spec.to_string();
                let name = spec
                    .name
                    .as_exact()
                    .map(|name| name.as_normalized().to_string())
                    .unwrap_or_else(|| canonical.clone());
                deduped
                    .entry(canonical)
                    .or_insert_with(|| (name, spec, selected.source.clone()));
            }
            Ok(None) => {}
            Err(err) => {
                failures.push(format!(
                    "  {} (from {}): {err}",
                    selected.requirement, selected.source
                ));
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
    let python_spec = match requires_python {
        Some(requires_python) => {
            match ana_pep508_to_matchspec::version_spec(requires_python, false) {
                Ok(Some(version)) => Some(MatchSpec {
                    name: PackageNameMatcher::Exact(PackageName::new_unchecked("python")),
                    version: Some(version),
                    ..MatchSpec::default()
                }),
                Ok(None) => None,
                Err(err) => {
                    failures.push(format!("  {REQUIRES_PYTHON_LABEL}: {err}"));
                    None
                }
            }
        }
        None => None,
    };

    if !failures.is_empty() {
        return Err(Error::Conversion(failures.join("\n")));
    }

    // The dedup key *is* the spec's canonical string; carry it through the
    // sort and into the locked entry rather than re-stringifying every
    // spec per comparison and again at the end.
    let mut entries: Vec<(String, String, MatchSpec, String)> = deduped
        .into_iter()
        .map(|(canonical, (name, spec, source))| (name, canonical, spec, source))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    // `+ python_spec.is_some() as usize`: exactly sized even when a
    // `python` constraint is appended below, so that push never
    // reallocates.
    let mut specs: Vec<MatchSpec> =
        Vec::with_capacity(entries.len() + python_spec.is_some() as usize);
    specs.extend(entries.iter().map(|(_, _, spec, _)| spec.clone()));
    if let Some(python_spec) = python_spec {
        specs.push(python_spec);
    }

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

    use std::str::FromStr;

    use uv_pep508::Requirement;

    use super::*;

    fn selected(reqs: &[&str]) -> Vec<SelectedRequirement> {
        reqs.iter()
            .map(|r| SelectedRequirement {
                requirement: Requirement::from_str(r).unwrap(),
                source: "runtime".to_string(),
            })
            .collect()
    }

    #[test]
    fn converts_and_canonicalizes() {
        let converted =
            convert_for_platform(&selected(&["numpy>=1.20", "ruff"]), None, Platform::Linux64)
                .unwrap();
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
        let selected = selected(&["numpy", "pywin32; sys_platform == 'win32'"]);
        let linux = convert_for_platform(&selected, None, Platform::Linux64).unwrap();
        assert_eq!(linux.locked.len(), 1);
        assert_eq!(linux.locked[0].matchspec, "numpy");

        // ...and is present when targeting win-64, computed from this
        // (non-Windows) host -- the whole point of the pure conversion.
        let windows = convert_for_platform(&selected, None, Platform::Win64).unwrap();
        assert_eq!(windows.locked.len(), 2);
    }

    #[test]
    fn duplicates_dedupe_by_canonical_string() {
        let mut selected = selected(&["numpy>=1.20"]);
        selected.push(SelectedRequirement {
            requirement: Requirement::from_str("numpy>=1.20").unwrap(),
            source: "group:dev".to_string(),
        });
        let converted = convert_for_platform(&selected, None, Platform::Linux64).unwrap();
        assert_eq!(converted.locked.len(), 1);
        // First source wins, and runtime is always selected first.
        assert_eq!(converted.locked[0].source, "runtime");
    }

    #[test]
    fn requires_python_becomes_a_python_spec_without_touching_locked() {
        // `requires-python` is solved like any other package (no separate
        // solver-side handling), but is deliberately not reflected in
        // `locked`/`ana.lock`'s own `requirements` --
        // `PlatformSection::requires_python` remains its own field.
        let requires_python = VersionSpecifiers::from_str(">=3.9").unwrap();
        let converted = convert_for_platform(
            &selected(&["numpy>=1.20"]),
            Some(&requires_python),
            Platform::Linux64,
        )
        .unwrap();

        assert_eq!(converted.locked.len(), 1, "python is not a locked entry");
        assert_eq!(converted.specs.len(), 2, "but it is solved for");
        let python = converted
            .specs
            .iter()
            .find(|spec| spec.name.as_exact().map(|n| n.as_normalized()) == Some("python"))
            .expect("a python matchspec was produced");
        let version_spec = python.version.as_ref().expect("python carries a version");
        assert!(version_spec.matches(&rattler_conda_types::Version::from_str("3.9.0").unwrap()));
        assert!(!version_spec.matches(&rattler_conda_types::Version::from_str("3.8.0").unwrap()));
    }

    #[test]
    fn no_requires_python_means_no_python_spec() {
        let converted =
            convert_for_platform(&selected(&["numpy"]), None, Platform::Linux64).unwrap();
        assert_eq!(converted.specs.len(), 1);
    }

    #[test]
    fn conversion_failures_are_aggregated() {
        let converted = convert_for_platform(
            &selected(&["numpy @ https://example.com/numpy.whl", "also @ file:///x"]),
            None,
            Platform::Linux64,
        );
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
}
