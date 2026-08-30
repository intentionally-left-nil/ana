//! Config resolution. In a `commercial-config` build, the compiled-in
//! config *is* the config -- `config.toml` is never read for any field
//! (see `build.rs`). Otherwise, everything comes from `config.toml` on
//! disk.

#[cfg(feature = "commercial-config")]
include!(concat!(env!("OUT_DIR"), "/compiled_config.rs"));

use ana_config::AnaConfig;

use crate::Error;

/// The config this invocation actually runs with: the compiled-in config
/// in a `commercial-config` build (disk untouched), otherwise
/// `config.toml`. `default_channels` and `pypi_to_conda_uri` are always
/// populated -- an unset or explicitly empty `default_channels` falls
/// back to `ana_config::DEFAULT_CHANNELS`, and a missing
/// `pypi_to_conda_uri` falls back to `ana_config::DEFAULT_PYPI_TO_CONDA_URI`.
pub fn resolve_config() -> Result<ResolvedConfig, Error> {
    resolve(raw_config()?)
}

/// [`resolve_config`]'s pure half, taking the raw config directly so
/// tests can exercise the default-application logic without going
/// through `ANA_CONFIG_PATH`/disk or a `commercial-config` build.
fn resolve(raw: AnaConfig) -> Result<ResolvedConfig, Error> {
    Ok(ResolvedConfig {
        // An explicitly-empty list is treated identically to unset --
        // otherwise a project with no channel override would have
        // nothing to solve against.
        default_channels: match raw.default_channels {
            Some(channels) if !channels.is_empty() => channels,
            _ => ana_config::DEFAULT_CHANNELS
                .iter()
                .map(ToString::to_string)
                .collect(),
        },
        allowed_channels: raw.allowed_channels,
        dry_solve_channels: raw.dry_solve_channels,
        pypi_to_conda_uri: match raw.pypi_to_conda_uri {
            Some(uri) => uri,
            None => ana_config::parse_uri(ana_config::DEFAULT_PYPI_TO_CONDA_URI)?,
        },
    })
}

/// The four config fields as `ana config get`/`ana run`/`ana sync`
/// actually see them, with `default_channels` and `pypi_to_conda_uri`
/// fallbacks applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub default_channels: Vec<String>,
    pub allowed_channels: Option<Vec<String>>,
    pub dry_solve_channels: Option<Vec<String>>,
    pub pypi_to_conda_uri: url::Url,
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
/// shape `load_from_disk` returns. A `pypi_to_conda_uri` parse failure
/// here means `build.rs` baked in something it hadn't actually
/// validated -- a `build.rs` bug, not user input.
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
        PypiToCondaUri => format!("{:?}", config.pypi_to_conda_uri.as_str()),
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
/// `commercial-config` build; `cli.rs` also hides the `set` subcommand
/// from `--help` under this feature, so this is the runtime backstop.
#[cfg(feature = "commercial-config")]
pub fn config_set(_key: ana_config::Key, _values: &[String]) -> Result<(), Error> {
    Err(Error::ConfigSetDisabled)
}

/// There is currently no `set` path to clear a key back to unset. A
/// `file://` channel value is rejected before ever touching
/// `config.toml` -- see `ana_config::reject_file_channel`.
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
        for value in values {
            ana_config::reject_file_channel(key, value)?;
        }
        document.set_channels(key, values);
    }
    document.write(&path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// `config_set` rejects an empty value list before ever touching
    /// `config.toml`.
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

    /// The empty-values check must fire before the URI-specific
    /// `[value] = values else { .. }` arity check, with the same
    /// "at least one value" message, not the URI one.
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

    #[test]
    fn resolve_defaults_default_channels_when_unset() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(
            resolved.default_channels,
            ana_config::DEFAULT_CHANNELS
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    /// `default_channels = []` in `config.toml` must not leave a
    /// project with nothing to solve against -- `resolve` treats this
    /// exactly like the field being absent.
    #[test]
    fn resolve_treats_an_empty_default_channels_the_same_as_absent() {
        let raw = AnaConfig {
            default_channels: Some(Vec::new()),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(
            resolved.default_channels,
            ana_config::DEFAULT_CHANNELS
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn resolve_respects_an_explicit_nonempty_default_channels() {
        let raw = AnaConfig {
            default_channels: Some(vec!["conda-forge".to_string()]),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(resolved.default_channels, vec!["conda-forge".to_string()]);
    }

    #[test]
    fn resolve_defaults_pypi_to_conda_uri_when_unset() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(
            resolved.pypi_to_conda_uri.as_str(),
            ana_config::DEFAULT_PYPI_TO_CONDA_URI
        );
    }

    #[test]
    fn resolve_respects_an_explicit_pypi_to_conda_uri() {
        let raw = AnaConfig {
            pypi_to_conda_uri: Some(
                url::Url::parse("https://custom.invalid/mapping.json").unwrap(),
            ),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(
            resolved.pypi_to_conda_uri.as_str(),
            "https://custom.invalid/mapping.json"
        );
    }
}
