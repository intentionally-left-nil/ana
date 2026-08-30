//! Resolution: an invocation (a project directory, or CLI-declared
//! requirements) plus the filesystem, turned into an [`Environment`] --
//! a declaration, its group selection, and the paths it lives at.
//! [`Environment::select`] is infallible because groups were already
//! validated here, the same moment its [`ana_paths::EnvironmentKey`] was
//! derived from them: a mismatched (declaration, key) pair is
//! unconstructable.

use std::path::Path;

use ana_dependency::{Dependency, SelectedRequirement};
use ana_matchspec_convert::canonical_matchspecs;
use ana_paths::{EnvironmentKey, EnvironmentLayout, EnvironmentPaths};
use ana_pypi_conda_map::MappingHandle;
use ana_requirements::RequirementSet;
use rattler_conda_types::Platform;
use uv_normalize::GroupName;
use uv_pep440::VersionSpecifiers;

use crate::error::Error;
use crate::origin::RequirementOrigin;
use crate::project_file::load_project_dir;

/// `source` recorded for an extra (`-i`) requirement, appended on top of
/// whatever a project or CLI-declared origin already contributes.
const EXTRA_SOURCE: &str = "include";

/// What an invocation declares its requirements from.
pub enum RequirementInput<'a> {
    /// A project directory: auto-detect and load its `pyproject.toml`/
    /// `requirements.txt`.
    ProjectDir { dir: &'a Path },
    /// Already-parsed CLI specifiers (`-g`), with no project file at
    /// all.
    CommandLine { dependencies: &'a [Dependency] },
}

/// Everything [`resolve`] needs: where the declaration comes from, which
/// groups and extra requirements to add on top of it, and the context a
/// content key or matchspec conversion needs.
pub struct EnvironmentRequest<'a> {
    pub input: RequirementInput<'a>,
    /// `--group` selections. Only meaningful for a project-file origin;
    /// any non-empty value against a `CommandLine` input fails
    /// [`resolve`] with [`Error::UnknownGroup`], since that origin has no
    /// group concept.
    pub groups: &'a [GroupName],
    /// `-i`/`--include` ad hoc requirements, layered on top of whatever
    /// `input` already declares.
    pub extra: &'a [Dependency],
    /// The platform a content key's canonical matchspecs are computed
    /// for. Unused when neither `extra` nor a `CommandLine` input is
    /// present, since no content key is needed then.
    pub platform: Platform,
    pub pypi_to_conda_map: &'a MappingHandle,
    /// Where a `CommandLine` (or, later, script) input's environment
    /// lives, with no project root of its own. Unused for a `ProjectDir`
    /// input.
    pub global_cache_root: &'a Path,
}

/// A resolved environment: a unified requirement declaration, the group
/// selection and extra requirements it was resolved with, and the
/// filesystem paths that selection maps to. Every consumer downstream
/// (lockfile generation, environment materialization) starts from here.
pub struct Environment {
    origin: RequirementOrigin,
    requirements: RequirementSet,
    groups: Vec<GroupName>,
    extra: Vec<Dependency>,
    paths: EnvironmentPaths,
}

impl Environment {
    pub fn origin(&self) -> &RequirementOrigin {
        &self.origin
    }

    pub fn paths(&self) -> &EnvironmentPaths {
        &self.paths
    }

    /// The interpreter constraint, for the `python` matchspec a
    /// conversion derives from it. `None` when the origin has no such
    /// concept (`requirements.txt`, a CLI-declared origin) or simply
    /// doesn't declare one.
    pub fn requires_python(&self) -> Option<&VersionSpecifiers> {
        self.requirements.requires_python()
    }

    /// The declaration's own channel override, checked by the caller
    /// against `default_channels ∪ allowed_channels` before use.
    pub fn channels(&self) -> Option<&[String]> {
        self.requirements.channels()
    }

    /// The full requirement set for this environment: the declaration's
    /// own selection (runtime dependencies unioned with every requested
    /// group) plus every extra (`-i`) requirement, tagged
    /// [`EXTRA_SOURCE`]. Infallible -- `resolve` already validated
    /// `groups` against the declaration before this `Environment` could
    /// exist.
    pub fn select(&self) -> Vec<SelectedRequirement<'_>> {
        let mut selected = self.requirements.select(&self.groups).unwrap_or_default();
        selected.extend(self.extra.iter().map(|dependency| SelectedRequirement {
            dependency,
            source: EXTRA_SOURCE.to_string(),
        }));
        selected
    }
}

/// Resolve an invocation to an [`Environment`]: load (or build) its
/// declaration, validate `request.groups` against it, derive its
/// [`EnvironmentKey`] per the origin/extra combination, and discover the
/// paths that key maps to.
pub fn resolve(request: &EnvironmentRequest<'_>) -> Result<Environment, Error> {
    match request.input {
        RequirementInput::ProjectDir { dir } => resolve_project_dir(dir, request),
        RequirementInput::CommandLine { dependencies } => {
            resolve_command_line(dependencies, request)
        }
    }
}

fn resolve_project_dir(dir: &Path, request: &EnvironmentRequest<'_>) -> Result<Environment, Error> {
    let (origin, requirements) = load_project_dir(dir)?;
    requirements.validate_groups(request.groups)?;

    let groups = request.groups.to_vec();
    let extra = request.extra.to_vec();
    let group_names: Vec<&str> = groups.iter().map(GroupName::as_str).collect();

    let layout = if extra.is_empty() {
        if groups.is_empty() {
            EnvironmentLayout::ProjectDefault { root: dir }
        } else {
            EnvironmentLayout::ProjectKeyed {
                root: dir,
                key: EnvironmentKey::from_symbolic_names(&group_names),
            }
        }
    } else {
        let canonical = content_key_matchspecs(&extra, requirements.requires_python(), request)?;
        let canonical_refs: Vec<&str> = canonical.iter().map(String::as_str).collect();
        EnvironmentLayout::ProjectKeyed {
            root: dir,
            key: EnvironmentKey::from_names_and_content(&group_names, &canonical_refs),
        }
    };

    Ok(Environment {
        origin,
        requirements,
        groups,
        extra,
        paths: ana_paths::discover(layout),
    })
}

fn resolve_command_line(
    dependencies: &[Dependency],
    request: &EnvironmentRequest<'_>,
) -> Result<Environment, Error> {
    let requirements = RequirementSet::from_dependencies(dependencies.to_vec());
    requirements.validate_groups(request.groups)?;

    let extra = request.extra.to_vec();
    let mut declared = dependencies.to_vec();
    declared.extend(extra.iter().cloned());
    let canonical = content_key_matchspecs(&declared, requirements.requires_python(), request)?;
    let canonical_refs: Vec<&str> = canonical.iter().map(String::as_str).collect();
    let layout = EnvironmentLayout::Global {
        cache_root: request.global_cache_root,
        key: EnvironmentKey::from_content(&canonical_refs),
    };

    Ok(Environment {
        origin: RequirementOrigin::CommandLine,
        requirements,
        groups: Vec::new(),
        extra,
        paths: ana_paths::discover(layout),
    })
}

/// The canonical matchspecs a content key is derived from, for
/// `dependencies` on `request.platform`.
fn content_key_matchspecs(
    dependencies: &[Dependency],
    requires_python: Option<&VersionSpecifiers>,
    request: &EnvironmentRequest<'_>,
) -> Result<Vec<String>, Error> {
    Ok(canonical_matchspecs(
        dependencies,
        requires_python,
        request.platform,
        request.pypi_to_conda_map,
    )?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;

    use uv_pep508::Requirement;

    use super::*;

    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    fn pep508(req: &str) -> Dependency {
        Dependency::Pep508(Requirement::from_str(req).unwrap())
    }

    fn matchspec(spec: &str) -> Dependency {
        Dependency::Matchspec(Box::new(ana_dependency::parse_matchspec(spec).unwrap()))
    }

    fn group(name: &str) -> GroupName {
        GroupName::from_str(name).unwrap()
    }

    fn project_dir(pyproject: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), pyproject).unwrap();
        dir
    }

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
dependencies = ["requests"]

[dependency-groups]
dev = ["ruff"]
"#;

    fn request<'a>(
        input: RequirementInput<'a>,
        groups: &'a [GroupName],
        extra: &'a [Dependency],
        map: &'a MappingHandle,
        cache_root: &'a Path,
    ) -> EnvironmentRequest<'a> {
        EnvironmentRequest {
            input,
            groups,
            extra,
            platform: Platform::Linux64,
            pypi_to_conda_map: map,
            global_cache_root: cache_root,
        }
    }

    #[test]
    fn project_dir_no_groups_no_extra_is_the_default_layout() {
        let dir = project_dir(PYPROJECT);
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let env = resolve(&request(
            RequirementInput::ProjectDir { dir: dir.path() },
            &[],
            &[],
            &map,
            cache.path(),
        ))
        .unwrap();

        assert_eq!(env.paths().lock_path, dir.path().join("ana.lock"));
        assert_eq!(env.paths().env_path, dir.path().join(".env"));
        assert_eq!(
            env.origin(),
            &RequirementOrigin::PyprojectToml {
                path: dir.path().join("pyproject.toml")
            }
        );
    }

    #[test]
    fn project_dir_with_groups_and_no_extra_uses_the_legacy_symbolic_key() {
        let dir = project_dir(PYPROJECT);
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let groups = vec![group("dev")];
        let env = resolve(&request(
            RequirementInput::ProjectDir { dir: dir.path() },
            &groups,
            &[],
            &map,
            cache.path(),
        ))
        .unwrap();

        assert_eq!(
            env.paths().lock_path,
            dir.path().join(".ana/ef260e9a/ana.lock"),
            "must reproduce the legacy --group hash byte-for-byte"
        );
    }

    #[test]
    fn project_dir_with_extra_uses_a_content_key_distinct_from_symbolic_alone() {
        let dir = project_dir(PYPROJECT);
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let groups = vec![group("dev")];
        let extra = vec![pep508("black")];

        let with_extra = resolve(&request(
            RequirementInput::ProjectDir { dir: dir.path() },
            &groups,
            &extra,
            &map,
            cache.path(),
        ))
        .unwrap();
        let without_extra = resolve(&request(
            RequirementInput::ProjectDir { dir: dir.path() },
            &groups,
            &[],
            &map,
            cache.path(),
        ))
        .unwrap();

        assert_ne!(
            with_extra.paths().lock_path,
            without_extra.paths().lock_path
        );
        let names: Vec<&str> = with_extra
            .select()
            .iter()
            .map(|s| match s.dependency {
                Dependency::Pep508(req) => req.name.as_str(),
                Dependency::Matchspec(_) => "",
            })
            .collect();
        assert!(names.contains(&"black"));
    }

    #[test]
    fn project_dir_unknown_group_is_an_error_before_any_conversion() {
        let dir = project_dir(PYPROJECT);
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let groups = vec![group("nope")];
        let result = resolve(&request(
            RequirementInput::ProjectDir { dir: dir.path() },
            &groups,
            &[],
            &map,
            cache.path(),
        ));
        assert!(matches!(result, Err(Error::UnknownGroup(name)) if name == "nope"));
    }

    #[test]
    fn command_line_input_uses_the_global_layout_with_a_content_key() {
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let deps = vec![matchspec("::python==3.14")];
        let env = resolve(&request(
            RequirementInput::CommandLine {
                dependencies: &deps,
            },
            &[],
            &[],
            &map,
            cache.path(),
        ))
        .unwrap();

        assert!(env.paths().lock_path.starts_with(cache.path()));
        assert_eq!(env.origin(), &RequirementOrigin::CommandLine);
        assert_eq!(env.select().len(), 1);
    }

    #[test]
    fn command_line_input_folds_extra_into_the_same_content_key() {
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let primary = vec![matchspec("::python==3.14")];
        let extra = vec![matchspec("fastapi")];

        let with_extra = resolve(&request(
            RequirementInput::CommandLine {
                dependencies: &primary,
            },
            &[],
            &extra,
            &map,
            cache.path(),
        ))
        .unwrap();
        let without_extra = resolve(&request(
            RequirementInput::CommandLine {
                dependencies: &primary,
            },
            &[],
            &[],
            &map,
            cache.path(),
        ))
        .unwrap();

        assert_ne!(
            with_extra.paths().lock_path,
            without_extra.paths().lock_path
        );
        assert_eq!(with_extra.select().len(), 2);
    }

    #[test]
    fn command_line_input_rejects_any_group() {
        let map = no_mapping();
        let cache = tempfile::tempdir().unwrap();
        let deps = vec![matchspec("::python==3.14")];
        let groups = vec![group("dev")];
        let result = resolve(&request(
            RequirementInput::CommandLine {
                dependencies: &deps,
            },
            &groups,
            &[],
            &map,
            cache.path(),
        ));
        assert!(matches!(result, Err(Error::UnknownGroup(name)) if name == "dev"));
    }
}
