//! The project half of the algorithm's inputs: auto-detecting and
//! loading the project's dependency-declaration file, and selecting
//! which requirements a given invocation solves for.
//!
//! Path *discovery* (which `lock_path`/`env_path` a `--group` selection
//! maps to) is `ana-paths`' concern and happens before this crate is
//! involved; this module only re-derives the requirement set that
//! selection represents, which must match what discovery hashed: the
//! environment's requirements are the runtime dependencies unioned with
//! every requested group, in that order.
//!
//! ## Auto-detection
//!
//! [`detect_project_file`] prefers `pyproject.toml` if it exists, else
//! falls back to `requirements.txt`, else it's an error. There is no
//! walk-up search for either file. Whichever file is found is parsed by
//! its own front-end crate and unified into the same [`Project`] shape
//! everything downstream works against.
//!
//! `requirements.txt` has no dependency-groups or `requires-python`
//! concept, so a `requirements.txt`-backed [`Project`] reports an empty
//! [`Project::groups`] map and [`Project::requires_python`] of `None` --
//! both are already valid, ordinary states for a `pyproject.toml`
//! project too, so no downstream code needs to special-case file kind.

use std::fs;
use std::path::Path;

use ana_dependency::Dependency;
use ana_pyproject::Pyproject;
use ana_requirements_txt::RequirementsTxt;
use indexmap::IndexMap;
use uv_normalize::GroupName;
use uv_pep440::VersionSpecifiers;

use crate::error::Error;

/// `source` value written into the lock for a runtime requirement: a
/// `pyproject.toml` `[project.dependencies]`/`[tool.ana.matchspec-dependencies]`
/// entry, or any `requirements.txt` entry (which has no group concept,
/// so every one of its entries is a runtime requirement).
pub(crate) const RUNTIME_SOURCE: &str = "runtime";

/// Which project file [`detect_project_file`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectFileKind {
    /// `<root>/pyproject.toml` -- checked, and preferred, first.
    Pyproject,
    /// `<root>/requirements.txt` -- the fallback used only when no
    /// `pyproject.toml` exists.
    RequirementsTxt,
}

/// Auto-detects which project file exists at `root`: prefers
/// `pyproject.toml`, falls back to `requirements.txt`, or `None` if
/// neither exists. No walk-up search -- `root` must be the exact
/// directory to check.
///
/// Exposed for callers that need to know "is this a project root at
/// all" without a full parse (`ana clean`, which never reads the file's
/// contents).
pub fn detect_project_file(root: &Path) -> Option<ProjectFileKind> {
    if root.join("pyproject.toml").is_file() {
        Some(ProjectFileKind::Pyproject)
    } else if root.join("requirements.txt").is_file() {
        Some(ProjectFileKind::RequirementsTxt)
    } else {
        None
    }
}

/// The largest a `pyproject.toml`/`requirements.txt` is allowed to be
/// before [`read_project_file`] refuses to read it. 1 MiB is generously
/// above any realistic hand-written file of either kind, while bounding
/// the worst case for an untrusted checkout to a fixed, small amount of
/// memory.
pub const MAX_PROJECT_FILE_SIZE: u64 = 1024 * 1024;

/// Reads `path` into a `String`, refusing to do so if it is larger than
/// [`MAX_PROJECT_FILE_SIZE`]. The size check is a separate
/// `fs::metadata` call before `fs::read_to_string`, so an oversized file
/// is never allocated into memory in the first place.
fn read_project_file(path: &Path) -> Result<String, Error> {
    let metadata = fs::metadata(path).map_err(|err| Error::Read {
        path: path.to_path_buf(),
        source: err,
    })?;
    let size = metadata.len();
    if size > MAX_PROJECT_FILE_SIZE {
        return Err(Error::ProjectFileTooLarge {
            path: path.to_path_buf(),
            size,
            limit: MAX_PROJECT_FILE_SIZE,
        });
    }
    fs::read_to_string(path).map_err(|err| Error::Read {
        path: path.to_path_buf(),
        source: err,
    })
}

/// A loaded project: whichever dependency-declaration file
/// [`detect_project_file`] found, read and parsed by its own front-end
/// crate, then unified into this format-agnostic shape.
pub struct Project {
    /// The project's runtime dependencies: `pyproject.toml`'s
    /// `[project.dependencies]`/`[tool.ana.matchspec-dependencies]`
    /// (merged, PEP 508 entries first), or every accepted
    /// `requirements.txt` entry -- both already in file order.
    dependencies: Vec<Dependency>,
    /// `pyproject.toml`'s resolved `[dependency-groups]`/
    /// `[tool.ana.matchspec-dependency-groups]`. Always empty for a
    /// `requirements.txt`-backed project.
    groups: IndexMap<GroupName, Vec<Dependency>>,
    /// `pyproject.toml`'s `[project.requires-python]`, if declared.
    /// Always `None` for a `requirements.txt`-backed project.
    requires_python: Option<VersionSpecifiers>,
    /// The project's own channel override: `pyproject.toml`'s
    /// `[tool.ana] conda-channels`, or `requirements.txt`'s
    /// `# ana-channels:` directive. `None` means no override -- the
    /// project solves against whatever `default_channels ∪
    /// allowed_channels` the caller supplies instead.
    channels: Option<Vec<String>>,
}

impl Project {
    /// Auto-detects and loads `root`'s project file (see
    /// [`detect_project_file`]), read and parsed by its own front-end
    /// crate and unified into this format-agnostic [`Project`].
    pub fn load(root: &Path) -> Result<Self, Error> {
        match detect_project_file(root) {
            Some(ProjectFileKind::Pyproject) => Self::load_pyproject(root),
            Some(ProjectFileKind::RequirementsTxt) => Self::load_requirements_txt(root),
            None => Err(Error::NoProjectFile {
                path: root.to_path_buf(),
            }),
        }
    }

    /// Read and parse `<root>/pyproject.toml`.
    fn load_pyproject(root: &Path) -> Result<Self, Error> {
        let path = root.join("pyproject.toml");
        let source = read_project_file(&path)?;
        let parsed = Pyproject::parse(&source)?;
        Ok(Self {
            dependencies: parsed.requirements.runtime,
            groups: parsed.requirements.groups,
            requires_python: parsed.requires_python,
            channels: parsed.channels,
        })
    }

    /// Read and parse `<root>/requirements.txt`.
    fn load_requirements_txt(root: &Path) -> Result<Self, Error> {
        let path = root.join("requirements.txt");
        let source = read_project_file(&path)?;
        let parsed = RequirementsTxt::parse(&source)?;
        let channels = parsed.channels;
        let dependencies = parsed
            .requirements
            .into_iter()
            .map(|entry| entry.dependency)
            .collect();
        Ok(Self {
            dependencies,
            groups: IndexMap::new(),
            requires_python: None,
            channels,
        })
    }

    /// `[project.requires-python]`, for the `python` matchspec
    /// `crate::matchspec::convert_for_platform` derives from it. `None`
    /// for a `requirements.txt`-backed project, or a `pyproject.toml`
    /// one that simply doesn't declare it -- both already mean "derive
    /// no `python` matchspec," with no distinction downstream.
    pub fn requires_python(&self) -> Option<&VersionSpecifiers> {
        self.requires_python.as_ref()
    }

    /// The project's own channel override -- `pyproject.toml`'s
    /// `[tool.ana] conda-channels`, or `requirements.txt`'s
    /// `# ana-channels:` directive. `None` means the project declares no
    /// override, so `crate::channels::effective_channels` falls back to
    /// `default_channels` unchecked; `Some(list)` must have every entry
    /// checked against `default_channels ∪ allowed_channels` before use.
    pub fn channels(&self) -> Option<&[String]> {
        self.channels.as_deref()
    }

    /// Validate that every requested group exists, without cloning any
    /// requirements. `ensure_current_platform` runs this cheap preflight
    /// up front so a typo'd `--group` errors even when the requirements
    /// otherwise turn out unchanged and no solve is needed. Always fails
    /// for any non-empty `groups` on a `requirements.txt`-backed project
    /// -- its own `groups` map is always empty, so every name is
    /// "unknown."
    pub fn validate_groups(&self, groups: &[GroupName]) -> Result<(), Error> {
        for group in groups {
            if !self.groups.contains_key(group) {
                return Err(Error::UnknownGroup(group.as_str().to_string()));
            }
        }
        Ok(())
    }

    /// The requirement set for an environment: the runtime dependencies
    /// unioned with every requested group, each tagged with the `source`
    /// string the lock records for it (`"runtime"` / `"group:<name>"`).
    ///
    /// Group names must already be normalized. A requested group that
    /// doesn't exist is an error, not an empty selection -- silently
    /// ignoring a typo'd group would produce a valid-looking lock for
    /// the wrong requirement set.
    pub fn select_requirements<'p>(
        &'p self,
        groups: &[GroupName],
    ) -> Result<Vec<SelectedRequirement<'p>>, Error> {
        let mut selected = Vec::new();
        for dependency in &self.dependencies {
            selected.push(SelectedRequirement {
                dependency,
                source: RUNTIME_SOURCE.to_string(),
            });
        }
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
}

/// One dependency selected for a solve, with its provenance. Borrows
/// the underlying [`Dependency`] out of the [`Project`] it was selected
/// from rather than cloning it, since `Project` already outlives every
/// consumer of this type.
#[derive(Debug, Clone)]
pub struct SelectedRequirement<'p> {
    pub dependency: &'p Dependency,
    /// `"runtime"` or `"group:<name>"` -- recorded in the lock for
    /// readability, never compared for staleness.
    pub source: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use super::*;

    fn write_project(dir: &std::path::Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    fn project(toml: &str) -> Project {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), "pyproject.toml", toml);
        let project = Project::load(dir.path()).unwrap();
        // Leak the tempdir so it outlives the returned `Project`.
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
            .map(|s| {
                let Dependency::Pep508(requirement) = &s.dependency else {
                    panic!("expected a Pep508 dependency");
                };
                (requirement.name.as_str(), s.source.as_str())
            })
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
        assert!(matches!(
            project.validate_groups(&groups),
            Err(Error::UnknownGroup(name)) if name == "nope"
        ));
        assert!(project.validate_groups(&[]).is_ok());
    }

    #[test]
    fn no_project_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Project::load(dir.path()),
            Err(Error::NoProjectFile { .. })
        ));
        assert_eq!(detect_project_file(dir.path()), None);
    }

    #[test]
    fn oversized_pyproject_toml_is_rejected_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        // Content doesn't matter -- the size check runs before parsing.
        let oversized = "a".repeat(MAX_PROJECT_FILE_SIZE as usize + 1);
        write_project(dir.path(), "pyproject.toml", &oversized);

        assert!(matches!(
            Project::load(dir.path()),
            Err(Error::ProjectFileTooLarge {
                size,
                limit,
                ..
            }) if size == MAX_PROJECT_FILE_SIZE + 1 && limit == MAX_PROJECT_FILE_SIZE
        ));
    }

    #[test]
    fn oversized_requirements_txt_is_rejected_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = "a".repeat(MAX_PROJECT_FILE_SIZE as usize + 1);
        write_project(dir.path(), "requirements.txt", &oversized);

        assert!(matches!(
            Project::load(dir.path()),
            Err(Error::ProjectFileTooLarge {
                size,
                limit,
                ..
            }) if size == MAX_PROJECT_FILE_SIZE + 1 && limit == MAX_PROJECT_FILE_SIZE
        ));
    }

    #[test]
    fn a_project_file_at_exactly_the_limit_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let body = "numpy\n";
        // Comment lines pad the file without adding requirements, so
        // the parse still succeeds once padded to the exact limit.
        let padding = "# pad\n".repeat((MAX_PROJECT_FILE_SIZE as usize - body.len()) / 6);
        let mut contents = body.to_string();
        contents.push_str(&padding);
        contents.truncate(MAX_PROJECT_FILE_SIZE as usize);
        // Truncation may have landed mid-line; pad with more comment
        // characters up to the exact byte count.
        while contents.len() < MAX_PROJECT_FILE_SIZE as usize {
            contents.push('#');
        }
        assert_eq!(contents.len(), MAX_PROJECT_FILE_SIZE as usize);
        write_project(dir.path(), "requirements.txt", &contents);

        assert!(Project::load(dir.path()).is_ok());
    }

    #[test]
    fn pyproject_toml_is_preferred_over_requirements_txt() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "pyproject.toml",
            r#"
[project]
name = "myproj"
dependencies = ["requests"]
"#,
        );
        write_project(dir.path(), "requirements.txt", "numpy\n");

        assert_eq!(
            detect_project_file(dir.path()),
            Some(ProjectFileKind::Pyproject)
        );
        let project = Project::load(dir.path()).unwrap();
        let selected = project.select_requirements(&[]).unwrap();
        let names: Vec<&str> = selected
            .iter()
            .map(|s| {
                let Dependency::Pep508(requirement) = &s.dependency else {
                    panic!("expected a Pep508 dependency");
                };
                requirement.name.as_str()
            })
            .collect();
        assert_eq!(names, vec!["requests"]);
    }

    #[test]
    fn requirements_txt_is_used_when_no_pyproject_toml_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), "requirements.txt", "numpy>=1.20\nruff\n");

        assert_eq!(
            detect_project_file(dir.path()),
            Some(ProjectFileKind::RequirementsTxt)
        );
        let project = Project::load(dir.path()).unwrap();
        assert_eq!(project.requires_python(), None);

        let selected = project.select_requirements(&[]).unwrap();
        let summary: Vec<(&str, &str)> = selected
            .iter()
            .map(|s| {
                let Dependency::Pep508(requirement) = &s.dependency else {
                    panic!("expected a Pep508 dependency");
                };
                (requirement.name.as_str(), s.source.as_str())
            })
            .collect();
        assert_eq!(summary, vec![("numpy", "runtime"), ("ruff", "runtime")]);
    }

    #[test]
    fn requirements_txt_has_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), "requirements.txt", "numpy\n");
        let project = Project::load(dir.path()).unwrap();

        let groups = vec![GroupName::from_str("dev").unwrap()];
        assert!(matches!(
            project.select_requirements(&groups),
            Err(Error::UnknownGroup(name)) if name == "dev"
        ));
        assert!(matches!(
            project.validate_groups(&groups),
            Err(Error::UnknownGroup(name)) if name == "dev"
        ));
        assert!(project.validate_groups(&[]).is_ok());
    }
}
