//! OS-appropriate cache directory resolution for this crate's data.

use std::path::PathBuf;

use directories::ProjectDirs;

const CACHE_FILE_NAME: &str = "pypi_mapping.msgpack";

/// A dedicated, never-renamed file used purely as a cross-process mutex
/// (see `refresh.rs`) -- separate from [`cache_file_path`] because an
/// advisory lock on a file's old inode does not block a `rename()` onto
/// its path, and [`cache_file_path`]'s file is replaced by rename on
/// every write.
const LOCK_FILE_NAME: &str = "pypi_mapping.lock";

/// The cache directory `ana` uses for this crate's data. Also the shared
/// root other runtime-fetched caches (e.g. `ana-solver`'s repodata cache)
/// should nest their own subdirectory under.
pub fn cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "ana").map(|dirs| dirs.cache_dir().to_path_buf())
}

pub(crate) fn cache_file_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(CACHE_FILE_NAME))
}

pub(crate) fn lock_file_path() -> Option<PathBuf> {
    cache_dir().map(|dir| dir.join(LOCK_FILE_NAME))
}
