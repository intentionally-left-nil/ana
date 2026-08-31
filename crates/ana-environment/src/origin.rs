//! Where an [`crate::Environment`]'s declaration came from: diagnostic
//! and policy metadata (error messages, which origins have a group
//! concept at all), not a dispatch mechanism.

use std::path::PathBuf;

/// Which kind of source a [`crate::Environment`]'s declaration was built
/// from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementOrigin {
    /// `<dir>/pyproject.toml`.
    PyprojectToml { path: PathBuf },
    /// `<dir>/requirements.txt`, used only when no `pyproject.toml`
    /// exists.
    RequirementsTxt { path: PathBuf },
    /// An ad hoc declaration built entirely from CLI-declared specifiers
    /// (`-g`/`-i`), with no project file at all.
    CommandLine,
    /// A PEP 723 inline script declaration: the `# /// script ... # ///`
    /// metadata block embedded in `path` itself, with no project file
    /// at all.
    Script { path: PathBuf },
}
