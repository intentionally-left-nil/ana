//! Config resolution. In a `commercial-config` build, the compiled-in
//! config *is* the config -- `config.toml` is never read for any field
//! (see `build.rs`). Otherwise, everything comes from `config.toml` on
//! disk. `allowed_channels`/`dry_solve_channels`/`sandboxed_channels`
//! additionally get a built-in default, but only in the latter
//! (community) case -- see [`default_allowed_channels`]/
//! [`default_dry_solve_channels`]/[`default_sandboxed_channels`].

#[cfg(feature = "commercial-config")]
include!(concat!(env!("OUT_DIR"), "/compiled_config.rs"));

use std::path::Path;

use ana_config::AnaConfig;

use crate::Error;

/// The config this invocation runs with: the compiled-in config in a
/// `commercial-config` build (disk untouched, `config_path` ignored),
/// otherwise the `config.toml` at `config_path` (`None` reads as
/// all-unset), with built-in defaults applied to unset fields.
pub fn resolve_config(config_path: Option<&Path>) -> Result<ResolvedConfig, Error> {
    resolve(raw_config(config_path)?)
}

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
        allowed_channels: default_allowed_channels(raw.allowed_channels),
        dry_solve_channels: default_dry_solve_channels(raw.dry_solve_channels),
        pypi_to_conda_uri: match raw.pypi_to_conda_uri {
            Some(uri) => uri,
            None => ana_config::parse_uri(ana_config::DEFAULT_PYPI_TO_CONDA_URI)?,
        },
        sandboxed_channels: default_sandboxed_channels(raw.sandboxed_channels),
        sandbox_policy: raw
            .sandbox_policy
            .unwrap_or_else(|| crate::sandbox::DEFAULT_POLICY.to_string()),
    })
}

/// `allowed_channels` as a community build resolves it: an *absent*
/// value (never an explicitly-empty one -- that stays a deliberate
/// opt-out, authorizing nothing beyond `default_channels`) falls back to
/// `ana_config::DEFAULT_ALLOWED_CHANNELS`.
#[cfg(not(feature = "commercial-config"))]
fn default_allowed_channels(raw: Option<Vec<String>>) -> Option<Vec<String>> {
    Some(raw.unwrap_or_else(|| {
        ana_config::DEFAULT_ALLOWED_CHANNELS
            .iter()
            .map(ToString::to_string)
            .collect()
    }))
}

/// `allowed_channels` as a `commercial-config` build resolves it: the
/// compiled-in config is authoritative on this field, so an absent value
/// stays absent rather than picking up the community default.
#[cfg(feature = "commercial-config")]
fn default_allowed_channels(raw: Option<Vec<String>>) -> Option<Vec<String>> {
    raw
}

/// `dry_solve_channels` as a community build resolves it: an *absent*
/// value (never an explicitly-empty one -- that stays a deliberate
/// opt-out of `ana sync --dry` widening) falls back to
/// `ana_config::DEFAULT_DRY_SOLVE_CHANNELS`.
#[cfg(not(feature = "commercial-config"))]
fn default_dry_solve_channels(raw: Option<Vec<String>>) -> Option<Vec<String>> {
    Some(raw.unwrap_or_else(|| {
        ana_config::DEFAULT_DRY_SOLVE_CHANNELS
            .iter()
            .map(ToString::to_string)
            .collect()
    }))
}

/// `dry_solve_channels` as a `commercial-config` build resolves it: the
/// compiled-in config is authoritative on this field, so an absent value
/// stays absent rather than picking up the community default.
#[cfg(feature = "commercial-config")]
fn default_dry_solve_channels(raw: Option<Vec<String>>) -> Option<Vec<String>> {
    raw
}

/// `sandboxed_channels` as a community build resolves it: an *absent*
/// value (never an explicitly-empty one -- that stays a deliberate
/// opt-out of sandboxing) falls back to
/// `ana_config::DEFAULT_SANDBOXED_CHANNELS`.
#[cfg(not(feature = "commercial-config"))]
fn default_sandboxed_channels(raw: Option<Vec<String>>) -> Option<Vec<String>> {
    Some(raw.unwrap_or_else(|| {
        ana_config::DEFAULT_SANDBOXED_CHANNELS
            .iter()
            .map(ToString::to_string)
            .collect()
    }))
}

/// `sandboxed_channels` as a `commercial-config` build resolves it: the
/// compiled-in config is authoritative on this field, so an absent value
/// stays absent rather than picking up the community default.
#[cfg(feature = "commercial-config")]
fn default_sandboxed_channels(raw: Option<Vec<String>>) -> Option<Vec<String>> {
    raw
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
    pub sandboxed_channels: Option<Vec<String>>,
    pub sandbox_policy: String,
}

#[cfg(feature = "commercial-config")]
fn raw_config(_config_path: Option<&Path>) -> Result<AnaConfig, Error> {
    compiled_config()
}

#[cfg(not(feature = "commercial-config"))]
fn raw_config(config_path: Option<&Path>) -> Result<AnaConfig, Error> {
    match config_path {
        Some(path) => Ok(ana_config::load(path)?),
        None => Ok(AnaConfig::default()),
    }
}

/// `COMPILED_CONFIG`'s `&'static str`s, turned into the owned `AnaConfig`
/// shape `ana_config::load` returns. A `pypi_to_conda_uri` parse failure
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
        sandboxed_channels: COMPILED_CONFIG.sandboxed_channels.map(owned),
        sandbox_policy: COMPILED_CONFIG.sandbox_policy.map(ToString::to_string),
    })
}

#[cfg(feature = "commercial-config")]
fn owned(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

/// `ana config get [KEY]`. `config_path` is the already-resolved
/// `config.toml` location -- see [`resolve_config`].
pub fn config_get(
    key: Option<ana_config::Key>,
    config_path: Option<&Path>,
) -> Result<String, Error> {
    let config = resolve_config(config_path)?;
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
        SandboxedChannels => format_optional_channels(&config.sandboxed_channels),
        SandboxPolicy => format!("{:?}", config.sandbox_policy),
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
pub fn config_set(
    _key: ana_config::Key,
    _values: &[String],
    _config_path: Option<&Path>,
) -> Result<(), Error> {
    Err(Error::ConfigSetDisabled)
}

/// There is currently no `set` path to clear a key back to unset. A
/// `file://` channel value is rejected before ever touching
/// `config.toml` -- see `ana_config::validate_channel`. A channel-list
/// value that looks like TOML list syntax (`[...]`) is rejected too,
/// with the space-separated spelling shown -- `set` takes multiple
/// values, not one list literal. `config_path` is the already-resolved
/// `config.toml` location (see [`resolve_config`]); `None` is
/// [`Error::NoConfigDir`].
#[cfg(not(feature = "commercial-config"))]
pub fn config_set(
    key: ana_config::Key,
    values: &[String],
    config_path: Option<&Path>,
) -> Result<(), Error> {
    if values.is_empty() {
        return Err(Error::ConfigSetArity {
            key,
            expected: "at least one value",
        });
    }
    let path = config_path.ok_or(Error::NoConfigDir)?;
    let mut document = ana_config::ConfigDocument::read(path)?;
    if key.is_uri() {
        let [value] = values else {
            return Err(Error::ConfigSetArity {
                key,
                expected: "exactly one value",
            });
        };
        let url = ana_config::parse_uri(value)?;
        document.set_uri(key, &url);
    } else if key.is_json() {
        let [value] = values else {
            return Err(Error::ConfigSetArity {
                key,
                expected: "exactly one value",
            });
        };
        ana_config::validate_sandbox_policy(key, value)?;
        document.set_json_string(key, value);
    } else {
        for value in values {
            if value.trim_start().starts_with('[') {
                return Err(Error::Config(ana_config::ConfigError::InvalidField {
                    key,
                    message: format!(
                        "{value:?} looks like TOML list syntax; `config set` takes \
                         space-separated values instead (e.g. `ana config set {key} \
                         conda-forge bioconda`)"
                    ),
                }));
            }
            ana_config::validate_channel(key, value)?;
        }
        document.set_channels(key, values);
    }
    document.write(path)?;
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
        let result = config_set(ana_config::Key::DefaultChannels, &[], None);
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
        let result = config_set(ana_config::Key::PypiToCondaUri, &[], None);
        assert!(matches!(
            result,
            Err(Error::ConfigSetArity {
                key: ana_config::Key::PypiToCondaUri,
                expected: "at least one value",
            })
        ));
    }

    /// `config_set` rejects an invalid channel value before ever writing
    /// `config.toml`.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_set_rejects_invalid_channel_values_with_the_key_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // A `/*` wildcard is legal in `allowed_channels` but not in
        // `default_channels`.
        for value in [
            "file:///tmp/local-channel",
            "https://example.com/pkgs/main/*",
        ] {
            let result = config_set(
                ana_config::Key::DefaultChannels,
                &[value.to_string()],
                Some(&path),
            );
            assert!(
                matches!(
                    result,
                    Err(Error::Config(ana_config::ConfigError::InvalidField {
                        key: ana_config::Key::DefaultChannels,
                        ..
                    }))
                ),
                "{value} must be rejected: {result:?}"
            );
            assert!(!path.exists(), "a rejected value must never be written");
        }
    }

    /// A value that looks like TOML list syntax (`'[]'`,
    /// `'["conda-forge"]'`) must be rejected with the space-separated
    /// spelling shown -- never stored as a literal channel.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_set_rejects_toml_list_syntax_with_an_example() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let result = config_set(
            ana_config::Key::AllowedChannels,
            &["[]".to_string()],
            Some(&path),
        );
        match result {
            Err(Error::Config(ana_config::ConfigError::InvalidField {
                key: ana_config::Key::AllowedChannels,
                message,
            })) => {
                assert!(message.contains("\"[]\""), "{message}");
                assert!(
                    message.contains("ana config set allowed_channels conda-forge bioconda"),
                    "{message}"
                );
            }
            other => panic!("expected an InvalidField naming the TOML-list mistake: {other:?}"),
        }
        assert!(!path.exists(), "a rejected value must never be written");
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn config_set_writes_a_valid_value_that_resolve_config_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        config_set(
            ana_config::Key::DefaultChannels,
            &["conda-forge".to_string()],
            Some(&path),
        )
        .unwrap();

        let resolved = resolve_config(Some(&path)).unwrap();
        assert_eq!(resolved.default_channels, vec!["conda-forge".to_string()]);
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

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_defaults_allowed_channels_when_unset() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(
            resolved.allowed_channels,
            Some(
                ana_config::DEFAULT_ALLOWED_CHANNELS
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            )
        );
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_treats_an_explicitly_empty_allowed_channels_as_opted_out() {
        let raw = AnaConfig {
            allowed_channels: Some(Vec::new()),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(resolved.allowed_channels, Some(Vec::new()));
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_respects_an_explicit_allowed_channels() {
        let raw = AnaConfig {
            allowed_channels: Some(vec!["bioconda".to_string()]),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(
            resolved.allowed_channels,
            Some(vec!["bioconda".to_string()])
        );
    }

    #[cfg(feature = "commercial-config")]
    #[test]
    fn resolve_leaves_allowed_channels_unset_in_a_commercial_config_build() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(resolved.allowed_channels, None);
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_defaults_dry_solve_channels_when_unset() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(
            resolved.dry_solve_channels,
            Some(
                ana_config::DEFAULT_DRY_SOLVE_CHANNELS
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            )
        );
    }

    /// An explicit `dry_solve_channels = []` is a deliberate opt-out of
    /// `ana sync --dry` widening, not "unset" -- unlike `default_channels`,
    /// it must not be replaced by the built-in default.
    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_treats_an_explicitly_empty_dry_solve_channels_as_opted_out() {
        let raw = AnaConfig {
            dry_solve_channels: Some(Vec::new()),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(resolved.dry_solve_channels, Some(Vec::new()));
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_respects_an_explicit_dry_solve_channels() {
        let raw = AnaConfig {
            dry_solve_channels: Some(vec!["bioconda".to_string()]),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(
            resolved.dry_solve_channels,
            Some(vec!["bioconda".to_string()])
        );
    }

    /// A `commercial-config` build's compiled-in config is authoritative
    /// on `dry_solve_channels`: an absent value stays absent rather than
    /// picking up the community-only default.
    #[cfg(feature = "commercial-config")]
    #[test]
    fn resolve_leaves_dry_solve_channels_unset_in_a_commercial_config_build() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(resolved.dry_solve_channels, None);
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_defaults_sandboxed_channels_when_unset() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(
            resolved.sandboxed_channels,
            Some(
                ana_config::DEFAULT_SANDBOXED_CHANNELS
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            )
        );
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_treats_an_explicitly_empty_sandboxed_channels_as_opted_out() {
        let raw = AnaConfig {
            sandboxed_channels: Some(Vec::new()),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(resolved.sandboxed_channels, Some(Vec::new()));
    }

    #[cfg(not(feature = "commercial-config"))]
    #[test]
    fn resolve_respects_an_explicit_sandboxed_channels() {
        let raw = AnaConfig {
            sandboxed_channels: Some(vec!["bioconda".to_string()]),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(
            resolved.sandboxed_channels,
            Some(vec!["bioconda".to_string()])
        );
    }

    #[cfg(feature = "commercial-config")]
    #[test]
    fn resolve_leaves_sandboxed_channels_unset_in_a_commercial_config_build() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(resolved.sandboxed_channels, None);
    }
}
