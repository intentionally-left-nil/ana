//! `ana`'s own Kilo config directory: a directory `ana` fully owns and
//! points a wrapped `kilo` subprocess at (via `KILO_CONFIG_DIR`),
//! independent of whatever `~/.config/kilo` the user already has (if
//! anything) on this machine.

use std::path::PathBuf;

use directories::ProjectDirs;

/// `ana`'s dedicated Kilo config directory -- `None` if the
/// OS-appropriate config directory can't be determined (no resolvable
/// home directory).
pub fn kilo_config_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "ana").map(|dirs| dirs.config_dir().join("kilo"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn kilo_config_dir_is_a_dedicated_subdirectory_of_anas_own_config_dir() {
        let Some(dir) = kilo_config_dir() else {
            // No resolvable home directory in this environment -- not a
            // failure this function should report.
            return;
        };
        assert_eq!(dir.file_name(), Some(std::ffi::OsStr::new("kilo")));

        let own_config_dir = ProjectDirs::from("", "", "ana")
            .expect("resolvable above")
            .config_dir()
            .to_path_buf();
        assert_eq!(dir.parent(), Some(own_config_dir.as_path()));
    }
}
