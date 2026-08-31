//! `config.toml`'s schema: the four fields [`AnaConfig`] holds, the [`Key`]
//! enum `ana config get`/`set` address them by, and [`parse_uri`], the one
//! validation rule shared by a hand-written `config.toml` and `ana config
//! set pypi_to_conda_uri ...`.

use std::str::FromStr;

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

/// `dry_solve_channels`'s value when nothing configures it otherwise, in
/// a community (non-`commercial-config`) build only -- a
/// `commercial-config` build's compiled-in config never gets this
/// fallback, an absent `dry_solve_channels` there leaves `ana sync --dry`
/// widening off. See `ana::config::resolve`.
pub const DEFAULT_DRY_SOLVE_CHANNELS: &[&str] =
    &["https://repo.terminal.space/api/channels/pypi/mirror"];

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

    /// Which position this key's own channel-list entries occupy: a
    /// search list (every key but `allowed_channels`, since
    /// `pypi_to_conda_uri` never reaches [`validate_channel`]) or the
    /// allow list -- the only position a `/*` wildcard pattern is legal
    /// in. See `ana_channels::ChannelListPosition`.
    fn channel_list_position(self) -> ana_channels::ChannelListPosition {
        match self {
            Key::AllowedChannels => ana_channels::ChannelListPosition::AllowList,
            Key::DefaultChannels | Key::DrySolveChannels | Key::PypiToCondaUri => {
                ana_channels::ChannelListPosition::SearchList
            }
        }
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
/// `dry_solve_channels`) that `ana_channels::validate_channel_entry`
/// itself rejects for `key`'s position: a local filesystem path (an
/// explicit `file://` URL, or a bare absolute/`~/` path, either of which
/// resolves to one), a credentialed URL, or (for `default_channels`/
/// `dry_solve_channels`, which are search lists) a `/*` wildcard pattern,
/// legal only in `allowed_channels`.
///
/// Shared by `document.rs`'s read path and `ana::config::config_set`'s
/// write path.
pub fn validate_channel(key: Key, raw: &str) -> Result<(), crate::ConfigError> {
    ana_channels::validate_channel_entry(key.channel_list_position(), raw).map_err(|source| {
        crate::ConfigError::InvalidField {
            key,
            message: source.to_string(),
        }
    })
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
    fn validate_channel_rejects_file_scheme() {
        assert!(matches!(
            validate_channel(Key::DefaultChannels, "file:///tmp/local-channel"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn validate_channel_accepts_a_bare_alias() {
        assert!(validate_channel(Key::DefaultChannels, "conda-forge").is_ok());
    }

    #[test]
    fn validate_channel_accepts_an_https_url() {
        assert!(validate_channel(Key::AllowedChannels, "https://repo.mycompany.com/conda").is_ok());
    }

    #[test]
    fn validate_channel_rejects_an_absolute_path() {
        assert!(matches!(
            validate_channel(Key::DefaultChannels, "/tmp/local-channel"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn validate_channel_rejects_a_home_relative_path() {
        assert!(matches!(
            validate_channel(Key::DefaultChannels, "~/local-channel"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn validate_channel_rejects_a_malformed_entry() {
        // Delegating to `ana_channels::validate_channel_entry` means a
        // string that doesn't even resolve as a channel is now caught
        // here too, not just downstream at solve time.
        assert!(matches!(
            validate_channel(Key::DefaultChannels, "./not-a-url-at-all"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn validate_channel_accepts_an_allowed_channels_wildcard() {
        assert!(validate_channel(Key::AllowedChannels, "https://example.com/pkgs/main/*").is_ok());
    }

    #[test]
    fn validate_channel_rejects_a_default_channels_wildcard() {
        assert!(matches!(
            validate_channel(Key::DefaultChannels, "https://example.com/pkgs/main/*"),
            Err(crate::ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn validate_channel_rejects_a_credentialed_url() {
        assert!(matches!(
            validate_channel(
                Key::AllowedChannels,
                "https://user:pass@example.com/channel"
            ),
            Err(crate::ConfigError::InvalidField {
                key: Key::AllowedChannels,
                ..
            })
        ));
    }
}
