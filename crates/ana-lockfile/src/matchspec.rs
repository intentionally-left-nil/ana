//! PEP 508 requirements -> canonical matchspecs for an arbitrary target
//! platform.
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

use std::collections::BTreeMap;

use ana_pep508_to_matchspec::convert_all;
use rattler_conda_types::{MatchSpec, Platform};

use crate::error::Error;
use crate::lock_file::LockedRequirement;
use crate::project::SelectedRequirement;

/// The conversion result, in the three forms the algorithm needs:
/// typed specs for the solver, locked entries for the file, and the bare
/// canonical strings for the stage-2 set diff.
pub(crate) struct ConvertedRequirements {
    /// Typed matchspecs, in the same order as [`locked`].
    pub specs: Vec<MatchSpec>,
    /// Canonical matchspec strings with their sources, sorted by package
    /// name then string, deduplicated by canonical string (first source
    /// wins -- runtime is always selected before groups, so it wins ties).
    pub locked: Vec<LockedRequirement>,
}

/// Convert `selected` to matchspecs as seen on `platform`.
///
/// A requirement whose marker can never hold on `platform` (e.g. a
/// win32-only dependency while targeting linux-64) is dropped, not an
/// error -- that's `convert`'s `Ok(None)` case. Genuine conversion
/// failures are aggregated into one error listing every failing
/// requirement, rather than failing fast on the first.
pub(crate) fn convert_for_platform(
    selected: &[SelectedRequirement],
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

    let specs = entries.iter().map(|(_, _, spec, _)| spec.clone()).collect();
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
            convert_for_platform(&selected(&["numpy>=1.20", "ruff"]), Platform::Linux64).unwrap();
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
        let linux = convert_for_platform(&selected, Platform::Linux64).unwrap();
        assert_eq!(linux.locked.len(), 1);
        assert_eq!(linux.locked[0].matchspec, "numpy");

        // ...and is present when targeting win-64, computed from this
        // (non-Windows) host -- the whole point of the pure conversion.
        let windows = convert_for_platform(&selected, Platform::Win64).unwrap();
        assert_eq!(windows.locked.len(), 2);
    }

    #[test]
    fn duplicates_dedupe_by_canonical_string() {
        let mut selected = selected(&["numpy>=1.20"]);
        selected.push(SelectedRequirement {
            requirement: Requirement::from_str("numpy>=1.20").unwrap(),
            source: "group:dev".to_string(),
        });
        let converted = convert_for_platform(&selected, Platform::Linux64).unwrap();
        assert_eq!(converted.locked.len(), 1);
        // First source wins, and runtime is always selected first.
        assert_eq!(converted.locked[0].source, "runtime");
    }

    #[test]
    fn conversion_failures_are_aggregated() {
        let converted = convert_for_platform(
            &selected(&["numpy @ https://example.com/numpy.whl", "also @ file:///x"]),
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
