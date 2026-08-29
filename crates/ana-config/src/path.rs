//! OS-appropriate config directory resolution, matching the pattern
//! `ana-pypi-conda-map/src/cache_dir.rs` already uses for the cache dir.

use std::path::PathBuf;

pub const CONFIG_FILE_NAME: &str = "config.toml";

/// `ANA_CONFIG_PATH` overrides the default location -- useful for tests
/// and for pointing at a non-default location deliberately.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("ANA_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    default_config_path()
}

/// `config.toml`'s default, platform-appropriate location -- no
/// `ANA_CONFIG_PATH` override applied.
pub fn default_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ana")
        .map(|dirs| dirs.config_dir().join(CONFIG_FILE_NAME))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;

    // `ANA_CONFIG_PATH` is process-wide env state; serialize this
    // module's tests so they can't observe each other's mutations.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ana_config_path_overrides_the_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("ANA_CONFIG_PATH", "/tmp/custom/config.toml");
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/tmp/custom/config.toml"))
        );
        std::env::remove_var("ANA_CONFIG_PATH");
    }

    #[test]
    fn without_the_override_config_path_matches_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("ANA_CONFIG_PATH");
        assert_eq!(config_path(), default_config_path());
    }
}
