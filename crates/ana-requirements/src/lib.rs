//! The format-agnostic dependency declaration `ana` resolves an
//! environment against, and group selection over it.
//!
//! A [`RequirementSet`] is deliberately front-end-agnostic: whether it
//! came from `pyproject.toml`, `requirements.txt`, or (via
//! [`RequirementSet::from_dependencies`]) a CLI-declared ad hoc
//! specifier list, every consumer downstream of this crate works
//! against the same shape. Parsing a source file into one, and
//! auto-detecting which file to parse, are a front end's concern (see
//! `ana_pyproject`/`ana_requirements_txt`) plus whichever crate resolves
//! an invocation to an origin -- this crate owns only the declaration
//! itself and selecting from it.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use ana_dependency::{Dependency, SelectedRequirement};
use indexmap::IndexMap;
use uv_normalize::GroupName;
use uv_pep440::VersionSpecifiers;

/// `source` value recorded for a runtime requirement -- one declared
/// outside any dependency group. `"group:<name>"` is a group
/// requirement's own source, built inline where it's tagged.
pub const RUNTIME_SOURCE: &str = "runtime";

/// Every way selecting from a [`RequirementSet`] can fail.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// A `--group` name that doesn't exist. For a `pyproject.toml`
    /// declaration, that means it's not defined in
    /// `[dependency-groups]`/`[tool.ana.matchspec-dependency-groups]`; a
    /// `requirements.txt` declaration has no group concept at all, so
    /// *every* name is "unknown" there; a CLI-declared
    /// ([`RequirementSet::from_dependencies`]) declaration has no groups
    /// either, for the same reason.
    #[error("dependency group `{0}` is not defined")]
    UnknownGroup(String),
}

/// A unified dependency declaration: runtime dependencies, dependency
/// groups, `requires-python`, and a channel override, all
/// format-agnostic. What `ana_pyproject`/`ana_requirements_txt` (or, for
/// a CLI-declared origin, no file at all) unify into.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequirementSet {
    /// The runtime dependencies, in declaration order.
    dependencies: Vec<Dependency>,
    /// Dependency groups, keyed by normalized name. Empty for a
    /// declaration with no group concept (`requirements.txt`, a
    /// CLI-declared set).
    groups: IndexMap<GroupName, Vec<Dependency>>,
    /// The interpreter constraint, if the declaration has one. Not a
    /// requirement itself -- it constrains the interpreter the
    /// environment is solved around, so callers check it as its own
    /// field rather than it being folded into the requirement set.
    requires_python: Option<VersionSpecifiers>,
    /// The declaration's own channel override. `None` means no
    /// override -- a solve falls back to whatever `default_channels ∪
    /// allowed_channels` the caller supplies instead.
    channels: Option<Vec<String>>,
}

impl RequirementSet {
    /// Build a [`RequirementSet`] from every field a front end already
    /// parsed. `pub(crate)`-free: any crate unifying a new format (or
    /// building this shape some other way) can construct one directly.
    pub fn new(
        dependencies: Vec<Dependency>,
        groups: IndexMap<GroupName, Vec<Dependency>>,
        requires_python: Option<VersionSpecifiers>,
        channels: Option<Vec<String>>,
    ) -> Self {
        Self {
            dependencies,
            groups,
            requires_python,
            channels,
        }
    }

    /// A [`RequirementSet`] with only runtime dependencies: no groups,
    /// no `requires-python`, no channel override. For a CLI-declared
    /// (`-g`/`-i`) or other originless declaration, where there is no
    /// source file to carry any of those.
    pub fn from_dependencies(dependencies: Vec<Dependency>) -> Self {
        Self {
            dependencies,
            groups: IndexMap::new(),
            requires_python: None,
            channels: None,
        }
    }

    /// The interpreter constraint, for the `python` matchspec a
    /// conversion derives from it. `None` when the declaration doesn't
    /// have one -- already a valid, ordinary state, with no
    /// distinction downstream from "this format has no such concept."
    pub fn requires_python(&self) -> Option<&VersionSpecifiers> {
        self.requires_python.as_ref()
    }

    /// The declaration's own channel override. `None` means no
    /// override, so a solve falls back to `default_channels` unchecked;
    /// `Some(list)` must have every entry checked against
    /// `default_channels ∪ allowed_channels` before use.
    pub fn channels(&self) -> Option<&[String]> {
        self.channels.as_deref()
    }

    /// Validate that every requested group exists, without cloning any
    /// requirements -- a cheap preflight callers run before doing any
    /// more expensive work, so a typo'd `--group` errors immediately.
    pub fn validate_groups(&self, groups: &[GroupName]) -> Result<(), Error> {
        for group in groups {
            if !self.groups.contains_key(group) {
                return Err(Error::UnknownGroup(group.as_str().to_string()));
            }
        }
        Ok(())
    }

    /// The requirement set for an environment: the runtime dependencies
    /// unioned with every requested group, each tagged with the
    /// `source` string a lock records for it (`"runtime"` /
    /// `"group:<name>"`).
    ///
    /// Group names must already be normalized. A requested group that
    /// doesn't exist is an error, not an empty selection -- silently
    /// ignoring a typo'd group would produce a valid-looking selection
    /// for the wrong requirement set.
    pub fn select<'p>(
        &'p self,
        groups: &[GroupName],
    ) -> Result<Vec<SelectedRequirement<'p>>, Error> {
        let mut selected = self.runtime_selected();
        for group in groups {
            let dependencies = self
                .groups
                .get(group)
                .ok_or_else(|| Error::UnknownGroup(group.as_str().to_string()))?;
            selected.extend(dependencies.iter().map(|dependency| SelectedRequirement {
                dependency,
                source: format!("group:{}", group.as_str()),
            }));
        }
        Ok(selected)
    }

    /// This declaration's runtime dependencies, tagged with
    /// [`RUNTIME_SOURCE`]. Shared by [`select`](Self::select) so every
    /// caller's runtime entries are tagged identically, regardless of
    /// which groups (if any) are also requested.
    fn runtime_selected(&self) -> Vec<SelectedRequirement<'_>> {
        self.dependencies
            .iter()
            .map(|dependency| SelectedRequirement {
                dependency,
                source: RUNTIME_SOURCE.to_string(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use uv_pep508::Requirement;

    use super::*;

    fn dep(req: &str) -> Dependency {
        Dependency::Pep508(Requirement::from_str(req).unwrap())
    }

    fn group_name(name: &str) -> GroupName {
        GroupName::from_str(name).unwrap()
    }

    fn names<'a>(selected: &'a [SelectedRequirement<'a>]) -> Vec<(&'a str, &'a str)> {
        selected
            .iter()
            .map(|s| {
                let Dependency::Pep508(requirement) = &s.dependency else {
                    panic!("expected a Pep508 dependency");
                };
                (requirement.name.as_str(), s.source.as_str())
            })
            .collect()
    }

    #[test]
    fn selection_is_runtime_then_groups_in_order() {
        let mut groups = IndexMap::new();
        groups.insert(group_name("dev"), vec![dep("ruff"), dep("pytest")]);
        groups.insert(group_name("doc"), vec![dep("sphinx")]);
        let set = RequirementSet::new(vec![dep("requests")], groups, None, None);

        let selected = set.select(&[group_name("doc"), group_name("dev")]).unwrap();
        assert_eq!(
            names(&selected),
            vec![
                ("requests", "runtime"),
                ("sphinx", "group:doc"),
                ("ruff", "group:dev"),
                ("pytest", "group:dev"),
            ]
        );
    }

    #[test]
    fn unknown_group_is_an_error() {
        let set = RequirementSet::new(vec![dep("requests")], IndexMap::new(), None, None);
        let groups = vec![group_name("nope")];
        assert!(matches!(
            set.select(&groups),
            Err(Error::UnknownGroup(name)) if name == "nope"
        ));
        assert!(matches!(
            set.validate_groups(&groups),
            Err(Error::UnknownGroup(name)) if name == "nope"
        ));
        assert!(set.validate_groups(&[]).is_ok());
    }

    #[test]
    fn empty_groups_selects_only_runtime() {
        let set = RequirementSet::new(vec![dep("numpy"), dep("ruff")], IndexMap::new(), None, None);
        let selected = set.select(&[]).unwrap();
        assert_eq!(
            names(&selected),
            vec![("numpy", "runtime"), ("ruff", "runtime")]
        );
    }

    #[test]
    fn from_dependencies_has_no_groups_no_requires_python_no_channels() {
        let set = RequirementSet::from_dependencies(vec![dep("numpy")]);
        assert_eq!(set.requires_python(), None);
        assert_eq!(set.channels(), None);
        assert_eq!(names(&set.select(&[]).unwrap()), vec![("numpy", "runtime")]);
        assert!(matches!(
            set.validate_groups(&[group_name("dev")]),
            Err(Error::UnknownGroup(name)) if name == "dev"
        ));
    }

    #[test]
    fn requires_python_and_channels_are_exposed_verbatim() {
        let requires_python = VersionSpecifiers::from_str(">=3.9").unwrap();
        let set = RequirementSet::new(
            vec![],
            IndexMap::new(),
            Some(requires_python.clone()),
            Some(vec!["conda-forge".to_string()]),
        );
        assert_eq!(set.requires_python(), Some(&requires_python));
        assert_eq!(set.channels(), Some(&["conda-forge".to_string()][..]));
    }
}
