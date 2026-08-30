//! The global cache root: where a CLI-declared (`-g`/`-i`) or other
//! project-less environment lives, independent of any project root.

use std::path::PathBuf;

use directories::ProjectDirs;

/// `ana`'s global environment cache root -- `None` if the OS-appropriate
/// cache directory can't be determined (no resolvable home directory).
pub fn global_cache_root() -> Option<PathBuf> {
    ProjectDirs::from("", "", "ana").map(|dirs| dirs.cache_dir().join("global_envs"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn global_cache_root_is_a_dedicated_subdirectory_of_the_cache_dir() {
        let Some(root) = global_cache_root() else {
            // No resolvable home directory in this environment -- not a
            // failure this function should report.
            return;
        };
        assert_eq!(root.file_name().unwrap(), "global_envs");
    }
}
