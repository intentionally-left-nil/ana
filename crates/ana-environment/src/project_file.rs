//! Auto-detecting and loading a project directory's dependency
//! declaration file into a unified [`RequirementSet`] and
//! [`RequirementOrigin`].
//!
//! [`detect_project_file`] prefers `pyproject.toml` if it exists, else
//! falls back to `requirements.txt`, else there is no project file.
//! There is no walk-up search for either. `requirements.txt` has no
//! dependency-groups or `requires-python` concept, so a
//! `requirements.txt`-backed declaration reports an empty group map and
//! no `requires-python` -- both already valid, ordinary states for a
//! `pyproject.toml` declaration too, so no downstream code needs to
//! special-case which file was found.

use std::fs;
use std::path::Path;

use ana_pyproject::Pyproject;
use ana_requirements::RequirementSet;
use ana_requirements_txt::RequirementsTxt;
use indexmap::IndexMap;

use crate::error::Error;
use crate::origin::RequirementOrigin;

/// Which project file [`detect_project_file`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectFileKind {
    Pyproject,
    RequirementsTxt,
}

/// Auto-detects which project file exists at `dir`: prefers
/// `pyproject.toml`, falls back to `requirements.txt`, or `None` if
/// neither exists. No walk-up search -- `dir` must be the exact
/// directory to check.
fn detect_project_file(dir: &Path) -> Option<ProjectFileKind> {
    if dir.join("pyproject.toml").is_file() {
        Some(ProjectFileKind::Pyproject)
    } else if dir.join("requirements.txt").is_file() {
        Some(ProjectFileKind::RequirementsTxt)
    } else {
        None
    }
}

/// Whether `dir` has a project file at all, without parsing its content
/// -- for a caller (`ana clean`) that only needs the precondition, not
/// the requirements themselves.
pub fn project_file_exists(dir: &Path) -> bool {
    detect_project_file(dir).is_some()
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

/// Auto-detects and loads `dir`'s project file (see
/// [`detect_project_file`]), parsed by its own front-end crate and
/// unified into a [`RequirementSet`], alongside which file it came from.
pub fn load_project_dir(dir: &Path) -> Result<(RequirementOrigin, RequirementSet), Error> {
    match detect_project_file(dir) {
        Some(ProjectFileKind::Pyproject) => load_pyproject(dir),
        Some(ProjectFileKind::RequirementsTxt) => load_requirements_txt(dir),
        None => Err(Error::NoProjectFile {
            path: dir.to_path_buf(),
        }),
    }
}

/// Read and parse `<dir>/pyproject.toml`.
fn load_pyproject(dir: &Path) -> Result<(RequirementOrigin, RequirementSet), Error> {
    let path = dir.join("pyproject.toml");
    let source = read_project_file(&path)?;
    let parsed = Pyproject::parse(&source)?;
    let requirements = RequirementSet::new(
        parsed.requirements.runtime,
        parsed.requirements.groups,
        parsed.requires_python,
        parsed.channels,
    );
    Ok((RequirementOrigin::PyprojectToml { path }, requirements))
}

/// Read and parse `<dir>/requirements.txt`.
fn load_requirements_txt(dir: &Path) -> Result<(RequirementOrigin, RequirementSet), Error> {
    let path = dir.join("requirements.txt");
    let source = read_project_file(&path)?;
    let parsed = RequirementsTxt::parse(&source)?;
    let channels = parsed.channels;
    let dependencies = parsed
        .requirements
        .into_iter()
        .map(|entry| entry.dependency)
        .collect();
    let requirements = RequirementSet::new(dependencies, IndexMap::new(), None, channels);
    Ok((RequirementOrigin::RequirementsTxt { path }, requirements))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use super::*;

    fn write_project(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn no_project_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_project_dir(dir.path()),
            Err(Error::NoProjectFile { .. })
        ));
        assert!(!project_file_exists(dir.path()));
    }

    #[test]
    fn pyproject_toml_is_preferred_over_requirements_txt() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            "pyproject.toml",
            "[project]\nname = \"myproj\"\ndependencies = [\"requests\"]\n",
        );
        write_project(dir.path(), "requirements.txt", "numpy\n");

        assert!(project_file_exists(dir.path()));
        let (origin, requirements) = load_project_dir(dir.path()).unwrap();
        assert_eq!(
            origin,
            RequirementOrigin::PyprojectToml {
                path: dir.path().join("pyproject.toml")
            }
        );
        assert_eq!(requirements.select(&[]).unwrap().len(), 1);
    }

    #[test]
    fn requirements_txt_is_used_when_no_pyproject_toml_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), "requirements.txt", "numpy>=1.20\nruff\n");

        let (origin, requirements) = load_project_dir(dir.path()).unwrap();
        assert_eq!(
            origin,
            RequirementOrigin::RequirementsTxt {
                path: dir.path().join("requirements.txt")
            }
        );
        assert_eq!(requirements.requires_python(), None);
        assert_eq!(requirements.select(&[]).unwrap().len(), 2);
    }

    #[test]
    fn oversized_pyproject_toml_is_rejected_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = "a".repeat(MAX_PROJECT_FILE_SIZE as usize + 1);
        write_project(dir.path(), "pyproject.toml", &oversized);

        assert!(matches!(
            load_project_dir(dir.path()),
            Err(Error::ProjectFileTooLarge {
                size,
                limit,
                ..
            }) if size == MAX_PROJECT_FILE_SIZE + 1 && limit == MAX_PROJECT_FILE_SIZE
        ));
    }

    #[test]
    fn requirements_txt_has_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        write_project(dir.path(), "requirements.txt", "numpy\n");
        let (_, requirements) = load_project_dir(dir.path()).unwrap();
        assert!(requirements
            .validate_groups(&[uv_normalize::GroupName::from_str("dev").unwrap()])
            .is_err());
    }
}
