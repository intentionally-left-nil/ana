//! Config resolution. In a `commercial-config` build, the compiled-in
//! config *is* the config -- `config.toml` is never read, for any field
//! (see `build.rs` for how the compiled value is produced and validated).
//! Otherwise, everything comes from `config.toml` on disk.

#[cfg(feature = "commercial-config")]
include!(concat!(env!("OUT_DIR"), "/compiled_config.rs"));

use ana_config::AnaConfig;

use crate::Error;

/// The config this invocation actually runs with: the compiled-in config
/// in a `commercial-config` build (disk untouched), otherwise
/// `config.toml`. `default_channels` is always populated -- see the
/// module docs on why an unset `default_channels` still resolves to
/// `ana_config::DEFAULT_CHANNELS` rather than staying empty.
pub fn resolve_config() -> Result<ResolvedConfig, Error> {
    let raw = raw_config()?;
    Ok(ResolvedConfig {
        default_channels: raw.default_channels.unwrap_or_else(|| {
            ana_config::DEFAULT_CHANNELS
                .iter()
                .map(ToString::to_string)
                .collect()
        }),
        allowed_channels: raw.allowed_channels,
        dry_solve_channels: raw.dry_solve_channels,
        pypi_to_conda_uri: raw.pypi_to_conda_uri,
    })
}

/// The four fields as `ana config get`/`ana run`/`ana sync` actually see
/// them -- `default_channels` is the one field with a real fallback
/// applied; the rest are exactly what `AnaConfig` has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub default_channels: Vec<String>,
    pub allowed_channels: Option<Vec<String>>,
    pub dry_solve_channels: Option<Vec<String>>,
    pub pypi_to_conda_uri: Option<url::Url>,
}

#[cfg(feature = "commercial-config")]
fn raw_config() -> Result<AnaConfig, Error> {
    compiled_config()
}

#[cfg(not(feature = "commercial-config"))]
fn raw_config() -> Result<AnaConfig, Error> {
    Ok(ana_config::load_from_disk()?)
}

/// `COMPILED_CONFIG`'s `&'static str`s, turned into the owned `AnaConfig`
/// shape `load_from_disk` returns. `pypi_to_conda_uri` failing to parse
/// here would mean `build.rs` baked in something `ana_config::parse_str`
/// had *not* actually validated -- a `build.rs` bug, not user input, but
/// still surfaced as an ordinary `Result` (never `expect`/`unwrap`), per
/// this crate's own policy.
#[cfg(feature = "commercial-config")]
fn compiled_config() -> Result<AnaConfig, Error> {
    Ok(AnaConfig {
        default_channels: COMPILED_CONFIG.default_channels.map(owned),
        allowed_channels: COMPILED_CONFIG.allowed_channels.map(owned),
        dry_solve_channels: COMPILED_CONFIG.dry_solve_channels.map(owned),
        pypi_to_conda_uri: COMPILED_CONFIG
            .pypi_to_conda_uri
            .map(url::Url::parse)
            .transpose()
            .map_err(|source| Error::InvalidCompiledConfig {
                field: "pypi_to_conda_uri",
                source,
            })?,
    })
}

#[cfg(feature = "commercial-config")]
fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

/// `ana config get [KEY]`.
pub fn config_get(key: Option<ana_config::Key>) -> Result<String, Error> {
    let config = resolve_config()?;
    Ok(match key {
        Some(key) => format_value(key, &config),
        None => ana_config::Key::ALL
            .iter()
            .map(|key| format!("{key} = {}", format_value(*key, &config)))
            .collect::<Vec<_>>()
            .join("\n"),
    })
}

fn format_value(key: ana_config::Key, config: &ResolvedConfig) -> String {
    use ana_config::Key::*;
    match key {
        DefaultChannels => format_channels(&config.default_channels),
        AllowedChannels => format_optional_channels(&config.allowed_channels),
        DrySolveChannels => format_optional_channels(&config.dry_solve_channels),
        PypiToCondaUri => config
            .pypi_to_conda_uri
            .as_ref()
            .map_or_else(|| "(not set)".to_string(), |url| format!("{url:?}")),
    }
}

fn format_channels(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
    format!("[{}]", items.join(", "))
}

fn format_optional_channels(values: &Option<Vec<String>>) -> String {
    match values {
        None => "(not set)".to_string(),
        Some(values) => format_channels(values),
    }
}

/// `ana config set <key> <values...>`. Disabled outright in a
/// `commercial-config` build -- centrally managed configuration is the
/// entire point of that build. `cli.rs` also hides the `set` subcommand
/// from `--help` under this feature; this is the runtime backstop for
/// the (still-parseable) case where it's invoked anyway.
#[cfg(feature = "commercial-config")]
pub fn config_set(_key: ana_config::Key, _values: &[String]) -> Result<(), Error> {
    Err(Error::ConfigSetDisabled)
}

/// `cli.rs`'s `num_args = 1..` already keeps a real CLI invocation from
/// ever reaching this function with an empty `values`; this is the
/// backstop for any other caller (including within this crate) so a
/// channel key can never be written as an explicit `key = []` -- which
/// `resolve_config` would then treat as "set to empty," not "unset,"
/// silently disabling every future solve. There is currently no `set`
/// path to clear a key back to unset at all (a future `ana config
/// delete`/`--delete` would add one).
#[cfg(not(feature = "commercial-config"))]
pub fn config_set(key: ana_config::Key, values: &[String]) -> Result<(), Error> {
    if values.is_empty() {
        return Err(Error::ConfigSetArity {
            key,
            expected: "at least one value",
        });
    }
    let path = ana_config::config_path().ok_or(Error::NoConfigDir)?;
    let mut document = ana_config::ConfigDocument::read(&path)?;
    if key.is_uri() {
        let [value] = values else {
            return Err(Error::ConfigSetArity {
                key,
                expected: "exactly one value",
            });
        };
        let url = ana_config::parse_uri(value)?;
        document.set_uri(key, &url);
    } else {
        document.set_channels(key, values);
    }
    document.write(&path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Regression test for the bug where `ana config set default_channels`
    /// (with no values) wrote `default_channels = []` to `config.toml`,
    /// and `resolve_config` then treated that as an explicitly-empty (not
    /// absent) channel list -- silently disabling every future `ana
    /// run`/`ana sync` solve with no channels to search. `config_set`
    /// must now reject an empty value list outright, before ever
    /// touching `config.toml`.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_set_rejects_empty_values_for_a_channel_key() {
        let result = config_set(ana_config::Key::DefaultChannels, &[]);
        assert!(matches!(
            result,
            Err(Error::ConfigSetArity {
                key: ana_config::Key::DefaultChannels,
                expected: "at least one value",
            })
        ));
    }

    /// Same regression, for a key `config_set` treats as a URI rather
    /// than a channel list: the empty-values check must fire before the
    /// URI-specific `[value] = values else { .. }` arity check, with the
    /// same "at least one value" message, not the URI one.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_set_rejects_empty_values_for_a_uri_key() {
        let result = config_set(ana_config::Key::PypiToCondaUri, &[]);
        assert!(matches!(
            result,
            Err(Error::ConfigSetArity {
                key: ana_config::Key::PypiToCondaUri,
                expected: "at least one value",
            })
        ));
    }
}
