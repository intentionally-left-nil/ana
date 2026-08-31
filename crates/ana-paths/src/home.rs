//! The current user's home directory -- `directories::BaseDirs`
//! directly, not `ProjectDirs`: callers here need the raw home directory
//! itself (e.g. `~/.anaconda/keyring`), not an OS-appropriate,
//! app-specific subdirectory.

use std::path::PathBuf;

use directories::BaseDirs;

/// The current user's home directory -- `None` if it can't be
/// determined (no resolvable `$HOME`/user profile).
pub fn home_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn home_dir_is_an_absolute_path_when_resolvable() {
        let Some(home) = home_dir() else {
            // No resolvable home directory in this environment -- not a
            // failure this function should report.
            return;
        };
        assert!(home.is_absolute());
    }
}
