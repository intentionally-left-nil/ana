//! OS-appropriate cache directory resolution for this crate's data.

use std::path::PathBuf;

use directories::ProjectDirs;

const CACHE_FILE_NAME: &str = "pypi_mapping.msgpack";

/// A dedicated, never-renamed file used purely as a cross-process mutex
/// (see `refresh.rs`) -- separate from [`cache_file_path`] because that
/// file is replaced by rename on every write, and an advisory lock on a
/// file's old inode does not block a `rename()` onto its path.
const LOCK_FILE_NAME: &str = "pypi_mapping.lock";

/// The cache directory `ana` uses for this crate's data. Also the shared
/// root other runtime-fetched caches (e.g. `ana-solver`'s repodata cache)
/// should nest their own subdirectory under, rather than each consumer
/// re-deriving its own `ProjectDirs` triple and risking drift. Public so
/// those other consumers can depend on this crate for exactly that.
pub fn cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "ana").map(|dirs| dirs.cache_dir().to_path_buf())
}

pub(crate) fn cache_file_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(CACHE_FILE_NAME))
}

pub(crate) fn lock_file_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(LOCK_FILE_NAME))
}
