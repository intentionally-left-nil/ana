//! The crate's error type: everything that can fail resolving an
//! invocation to an [`crate::Environment`].

use std::io;
use std::path::PathBuf;

/// Every way resolution can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// None of `pyproject.toml`, `requirements.txt`, or `environment.yml`
    /// exists at `path`. There is no walk-up search for any of them:
    /// `ana` must be run from the project root.
    #[error(
        "could not find pyproject.toml, requirements.txt, or environment.yml in {path} \
         (ana must be run from the project root)"
    )]
    NoProjectFile { path: PathBuf },

    /// `path` (`pyproject.toml`, `requirements.txt`, or
    /// `environment.yml`) is larger than
    /// [`crate::project_file::MAX_PROJECT_FILE_SIZE`], rejected before it
    /// is read into memory.
    #[error(
        "{path} is {size} bytes, which is larger than the {limit}-byte limit \
         for a project file"
    )]
    ProjectFileTooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },

    /// Reading `pyproject.toml`/`requirements.txt`/`environment.yml`
    /// failed.
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },

    /// `pyproject.toml` failed `ana_pyproject`'s own validation.
    #[error("{0}")]
    Pyproject(#[from] ana_pyproject::PyprojectError),

    /// `requirements.txt` failed `ana_requirements_txt`'s own
    /// validation.
    #[error("{0}")]
    RequirementsTxt(#[from] ana_requirements_txt::RequirementsTxtError),

    /// `environment.yml` failed `ana_environment_yml`'s own validation.
    #[error("{0}")]
    EnvironmentYml(#[from] ana_environment_yml::EnvironmentYmlError),

    /// A `--group` name that doesn't exist. For a `pyproject.toml`
    /// project, that means it's not defined in `[dependency-groups]`/
    /// `[tool.ana.matchspec-dependency-groups]`; a `requirements.txt`/
    /// `environment.yml` project, or a CLI-declared (`-g`/`-i`)
    /// invocation, has no group concept at all, so *every* name is
    /// "unknown" there. See [`ana_requirements::Error::UnknownGroup`].
    #[error(transparent)]
    Groups(#[from] ana_requirements::Error),

    /// An extra (`-i`) or CLI-declared (`-g`) requirement could not be
    /// converted to a matchspec for the target platform, while computing
    /// its content key.
    #[error(transparent)]
    Convert(#[from] ana_matchspec_convert::Error),
}
