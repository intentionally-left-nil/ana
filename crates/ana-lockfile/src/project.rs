//! The project half of the algorithm's inputs: loading `pyproject.toml`
//! and selecting which requirements a given invocation solves for.
//!
//! Bucket *discovery* (which `lock_path`/`env_path` a `--group` selection
//! maps to) is `env_storage.md`'s concern and happens before this crate is
//! involved; this module only re-derives the requirement set that
//! selection represents, which must match what discovery hashed: the
//! bucket's requirements are `[project.dependencies]` unioned with every
//! requested group, in that order.

use std::fs;
use std::path::Path;

use ana_pyproject::Pyproject;
use uv_normalize::GroupName;
use uv_pep508::Requirement;

use crate::error::Error;
use crate::hash::sha256_hex;

/// `source` value written into the lock for a runtime
/// (`[project.dependencies]`) requirement.
pub(crate) const RUNTIME_SOURCE: &str = "runtime";

/// A loaded `pyproject.toml`: the parsed metadata plus the raw source
/// text (needed whole for the stage-1 `pyproject_hash`).
pub struct Project {
    source: String,
    parsed: Pyproject,
}

impl Project {
    /// Read and parse `<root>/pyproject.toml`.
    pub fn load(root: &Path) -> Result<Self, Error> {
        let path = root.join("pyproject.toml");
        let source = fs::read_to_string(&path).map_err(|err| Error::Read {
            path: path.clone(),
            source: err,
        })?;
        let parsed = Pyproject::parse(&source)?;
        Ok(Self { source, parsed })
    }

    /// The parsed `pyproject.toml`.
    pub fn pyproject(&self) -> &Pyproject {
        &self.parsed
    }

    /// SHA-256 of the whole `pyproject.toml` file -- the `pyproject_hash`
    /// half of the stage-1 cache. Deliberately whole-file: any edit causes
    /// a stage-1 miss, and a miss only costs a stage-2 recheck.
    pub fn source_hash(&self) -> String {
        sha256_hex(self.source.as_bytes())
    }

    /// The requirement set for a bucket: `runtime` unioned with every
    /// requested group, each requirement tagged with the `source` string
    /// the lock records for it (`"runtime"` / `"group:<name>"`).
    ///
    /// Group names must already be normalized (the caller's CLI layer does
    /// that, the same normalization `env_storage.md`'s bucket hash uses).
    /// A requested group that doesn't exist is an error, not an empty
    /// selection -- silently solving without a typo'd group would produce
    /// a valid-looking lock for the wrong requirement set.
    pub fn select_requirements(
        &self,
        groups: &[GroupName],
    ) -> Result<Vec<SelectedRequirement>, Error> {
        let mut selected = Vec::new();
        for requirement in &self.parsed.requirements.runtime {
            selected.push(SelectedRequirement {
                requirement: requirement.clone(),
                source: RUNTIME_SOURCE.to_string(),
            });
        }
        for group in groups {
            let requirements = self
                .parsed
                .requirements
                .groups
                .get(group)
                .ok_or_else(|| Error::UnknownGroup(group.as_str().to_string()))?;
            selected.extend(requirements.iter().map(|requirement| SelectedRequirement {
                requirement: requirement.clone(),
                source: format!("group:{}", group.as_str()),
            }));
        }
        Ok(selected)
    }
}

/// One requirement selected for a solve, with its provenance.
#[derive(Debug, Clone)]
pub struct SelectedRequirement {
    pub requirement: Requirement,
    /// `"runtime"` or `"group:<name>"` -- recorded in the lock for
    /// readability, never part of the stage-2 comparison.
    pub source: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use super::*;

    fn project(toml: &str) -> Project {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), toml).unwrap();
        let project = Project::load(dir.path()).unwrap();
        // Keep the tempdir alive by leaking -- tests are short-lived
        // processes; simpler than threading a guard through the fixture.
        std::mem::forget(dir);
        project
    }

    #[test]
    fn selection_is_runtime_then_groups_in_order() {
        let project = project(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]

[dependency-groups]
dev = ["ruff", "pytest"]
doc = ["sphinx"]
"#,
        );
        let groups = vec![
            GroupName::from_str("doc").unwrap(),
            GroupName::from_str("dev").unwrap(),
        ];
        let selected = project.select_requirements(&groups).unwrap();
        let summary: Vec<(&str, &str)> = selected
            .iter()
            .map(|s| (s.requirement.name.as_str(), s.source.as_str()))
            .collect();
        assert_eq!(
            summary,
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
        let project = project(
            r#"
[project]
name = "myproj"
"#,
        );
        let groups = vec![GroupName::from_str("nope").unwrap()];
        assert!(matches!(
            project.select_requirements(&groups),
            Err(Error::UnknownGroup(name)) if name == "nope"
        ));
    }

    #[test]
    fn source_hash_changes_with_any_edit() {
        let a = project("[project]\nname = \"myproj\"\n");
        let b = project("[project]\nname = \"myproj\"\n# comment\n");
        assert_ne!(a.source_hash(), b.source_hash());
    }
}
