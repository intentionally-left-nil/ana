//! OS-appropriate config directory resolution, matching the pattern
//! `ana-pypi-conda-map/src/cache_dir.rs` already uses for the cache dir.

use std::path::PathBuf;

pub const CONFIG_FILE_NAME: &str = "config.toml";

/// `ANA_CONFIG_PATH` overrides the default location -- useful for tests
/// and for pointing at a non-default location deliberately.
pub fn config_path() -> Option<PathBuf> {
    resolve(std::env::var_os("ANA_CONFIG_PATH").map(PathBuf::from))
}

/// `config.toml`'s default, platform-appropriate location -- no
/// `ANA_CONFIG_PATH` override applied.
pub fn default_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ana")
        .map(|dirs| dirs.config_dir().join(CONFIG_FILE_NAME))
}

/// `override_path` wins when set; otherwise the default location. Pure
/// (no env access): `std::env::set_var` is process-wide state, and
/// mutating it in-process races concurrent `getenv`.
fn resolve(override_path: Option<PathBuf>) -> Option<PathBuf> {
    override_path.or_else(default_config_path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn an_override_wins_over_the_default() {
        assert_eq!(
            resolve(Some(PathBuf::from("/tmp/custom/config.toml"))),
            Some(PathBuf::from("/tmp/custom/config.toml"))
        );
    }

    #[test]
    fn without_an_override_the_default_is_used() {
        assert_eq!(resolve(None), default_config_path());
    }
}
