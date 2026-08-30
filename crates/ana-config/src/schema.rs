//! `config.toml`'s schema: the four fields [`AnaConfig`] holds, the [`Key`]
//! enum `ana config get`/`set` address them by, and [`parse_uri`], the one
//! validation rule shared by a hand-written `config.toml` and `ana config
//! set pypi_to_conda_uri ...`.

use std::path::PathBuf;
use std::str::FromStr;

use rattler_conda_types::{Channel, ChannelConfig};
use url::Url;

/// `config.toml`'s four fields. Every field is `Option<_>` -- presence in
/// the file is the only way a field is "set"; there is no distinct
/// "explicitly set to the default" state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnaConfig {
    pub default_channels: Option<Vec<String>>,
    pub allowed_channels: Option<Vec<String>>,
    pub dry_solve_channels: Option<Vec<String>>,
    pub pypi_to_conda_uri: Option<Url>,
}

/// The channel list `default_channels` means when nothing configures it
/// otherwise. `ana-solver`'s own `"defaults"` -> `repo.anaconda.com/pkgs/*`
/// expansion (`crates/ana-solver/src/channels.rs`) resolves whatever name
/// it's handed, including this one.
pub const DEFAULT_CHANNELS: &[&str] = &["defaults"];

/// The pypi-to-conda name mapping URI used when `pypi_to_conda_uri` isn't
/// set.
pub const DEFAULT_PYPI_TO_CONDA_URI: &str = "https://shards.terminal.space/pypi_to_conda.json";

/// One of the four `config.toml` fields, addressable by `ana config
/// get`/`set`. `Display`/`FromStr` both use the literal TOML key
/// (`"default_channels"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    DefaultChannels,
    AllowedChannels,
    DrySolveChannels,
    PypiToCondaUri,
}

impl Key {
    pub const ALL: [Key; 4] = [
        Key::DefaultChannels,
        Key::AllowedChannels,
        Key::DrySolveChannels,
        Key::PypiToCondaUri,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Key::DefaultChannels => "default_channels",
            Key::AllowedChannels => "allowed_channels",
            Key::DrySolveChannels => "dry_solve_channels",
            Key::PypiToCondaUri => "pypi_to_conda_uri",
        }
    }

    /// Whether this key holds a single URI (`set` takes exactly one
    /// value) rather than an array of channel names (`set` takes any
    /// number, including zero).
    pub fn is_uri(self) -> bool {
        matches!(self, Key::PypiToCondaUri)
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Key {
    type Err = ParseKeyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Key::ALL
            .into_iter()
            .find(|key| key.as_str() == s)
            .ok_or_else(|| ParseKeyError(s.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown config key `{0}` (expected one of: default_channels, allowed_channels, dry_solve_channels, pypi_to_conda_uri)")]
pub struct ParseKeyError(String);

/// Shared by `document.rs`'s read path and the CLI's `set` path, so both
/// validate `pypi_to_conda_uri` the same way: scheme must be `file://` or
/// `https://`.
pub fn parse_uri(raw: &str) -> Result<Url, crate::ConfigError> {
    let url = Url::parse(raw).map_err(|source| crate::ConfigError::InvalidUri {
        key: Key::PypiToCondaUri,
        reason: source.to_string(),
    })?;
    match url.scheme() {
        "file" | "https" => Ok(url),
        other => Err(crate::ConfigError::InvalidUri {
            key: Key::PypiToCondaUri,
            reason: format!("scheme must be file:// or https://, got {other}://"),
        }),
    }
}

/// Rejects a channel entry (`default_channels`/`allowed_channels`/
/// `dry_solve_channels`) that names a local filesystem path. Resolves
/// `raw` through `Channel::from_str` and rejects it exactly when that
/// resolves to a `file://` base URL -- covering an explicit `file://`
/// URL and a bare absolute/`~/` path alike, since either resolves to a
/// real `file://` channel regardless of `root_dir`. A relative path that
/// fails to resolve here is left alone; it still surfaces as an error
/// downstream, as `ana_lockfile::Error::InvalidChannel`.
///
/// Shared by `document.rs`'s read path and `ana::config::config_set`'s
/// write path.
pub fn reject_file_channel(key: Key, raw: &str) -> Result<(), crate::ConfigError> {
    let channel_config = ChannelConfig::default_with_root_dir(PathBuf::new());
    let is_file_channel = Channel::from_str(raw, &channel_config)
        .is_ok_and(|channel| channel.base_url.as_ref().scheme() == "file");
    if is_file_channel {
        return Err(crate::ConfigError::InvalidField {
            key,
            message: format!(
                "channel {raw:?} names a local filesystem path, which is not supported"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn key_from_str_round_trips_through_as_str() {
        for key in Key::ALL {
            assert_eq!(key.as_str().parse::<Key>().unwrap(), key);
        }
    }

    #[test]
    fn key_from_str_rejects_unknown_keys() {
        assert!("not_a_key".parse::<Key>().is_err());
    }

    #[test]
    fn key_display_matches_as_str() {
        for key in Key::ALL {
            assert_eq!(key.to_string(), key.as_str());
        }
    }

    #[test]
    fn parse_uri_accepts_file_and_https() {
        assert!(parse_uri("file:///tmp/mapping.json").is_ok());
        assert!(parse_uri("https://example.com/mapping.json").is_ok());
    }

    #[test]
    fn parse_uri_rejects_other_schemes() {
        assert!(parse_uri("ftp://example.com/mapping.json").is_err());
    }

    #[test]
    fn parse_uri_rejects_garbage() {
        assert!(parse_uri("not a url").is_err());
    }

    #[test]
    fn reject_file_channel_rejects_file_scheme() {
        assert!(matches!(
            reject_file_channel(Key::DefaultChannels, "file:///tmp/local-channel"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn reject_file_channel_accepts_a_bare_alias() {
        assert!(reject_file_channel(Key::DefaultChannels, "conda-forge").is_ok());
    }

    #[test]
    fn reject_file_channel_accepts_an_https_url() {
        assert!(
            reject_file_channel(Key::AllowedChannels, "https://repo.mycompany.com/conda").is_ok()
        );
    }

    #[test]
    fn reject_file_channel_rejects_an_absolute_path() {
        assert!(matches!(
            reject_file_channel(Key::DefaultChannels, "/tmp/local-channel"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn reject_file_channel_rejects_a_home_relative_path() {
        assert!(matches!(
            reject_file_channel(Key::DefaultChannels, "~/local-channel"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn reject_file_channel_leaves_a_relative_path_to_the_downstream_resolve() {
        assert!(reject_file_channel(Key::DefaultChannels, "./not-a-url-at-all").is_ok());
    }

    #[test]
    fn reject_file_channel_accepts_a_non_path_non_url_string() {
        assert!(reject_file_channel(Key::DefaultChannels, "not a url").is_ok());
    }
}
