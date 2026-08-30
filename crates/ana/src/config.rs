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
/// `config.toml`. `default_channels` and `pypi_to_conda_uri` are always
/// populated -- see the module docs on why an unset (or explicitly
/// empty) `default_channels` still resolves to `ana_config::
/// DEFAULT_CHANNELS` rather than staying empty; `pypi_to_conda_uri` gets
/// the same treatment, falling back to `ana_config::
/// DEFAULT_PYPI_TO_CONDA_URI`. This applies uniformly in both builds: a
/// `commercial-config` deployment is expected to set `pypi_to_conda_uri`
/// in its own compiled `config.toml` (see `build.rs`), but nothing here
/// special-cases that build to force the default -- it's the same
/// fallback either way, only reached if the field is genuinely absent or
/// empty.
pub fn resolve_config() -> Result<ResolvedConfig, Error> {
    resolve(raw_config()?)
}

/// [`resolve_config`]'s pure half, taking the raw config directly rather
/// than re-reading it -- the seam this module's own tests exercise, so
/// they can assert the default-application logic itself without going
/// through `ANA_CONFIG_PATH`/disk or a `commercial-config` build.
fn resolve(raw: AnaConfig) -> Result<ResolvedConfig, Error> {
    Ok(ResolvedConfig {
        // `Some(vec![])` is treated identically to `None`: an
        // unconditionally-empty search list would leave any project
        // that declares no channel override of its own with nothing to
        // solve against, and a hand-edited (or otherwise externally
        // produced) `config.toml` has no guard against writing
        // `default_channels = []` the way `config_set`'s own arity
        // check does for the CLI write path -- so this fallback has to
        // hold at read time regardless of how the empty list got there.
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

/// The four fields as `ana config get`/`ana run`/`ana sync` actually see
/// them -- `default_channels` and `pypi_to_conda_uri` are the two fields
/// with a real fallback applied; the rest are exactly what `AnaConfig`
/// has.
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
/// channel key can never be written as an explicit `key = []` at all.
/// `resolve_config` treats an empty `default_channels` the same as
/// absent (falling back to `ana_config::DEFAULT_CHANNELS`) regardless of
/// how the empty list got into `config.toml`, so this check is only
/// about giving `ana config set default_channels` (called with no
/// values) an immediate, explicit error instead of a silent no-op --
/// not the last line of defense against an empty list disabling
/// solving. There is currently no `set` path to clear a key back to
/// unset at all (a future `ana config delete`/`--delete` would add
/// one). A `file://` channel value is rejected the same way, before
/// ever touching `config.toml` -- see `ana_config::reject_file_channel`.
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

    /// `config_set` rejects an empty value list outright, before ever
    /// touching `config.toml` -- giving `ana config set default_channels`
    /// (called with no values) an explicit error rather than a silent
    /// no-op. (`resolve` also falls back to `ana_config::
    /// DEFAULT_CHANNELS` for an explicitly-empty `default_channels`
    /// regardless of how it got into `config.toml` -- see
    /// `resolve_treats_an_empty_default_channels_the_same_as_absent` --
    /// so this check is about `config_set`'s own UX, not the only thing
    /// standing between an empty list and a solve with nothing to
    /// search.)
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

    /// `default_channels` falls back to `ana_config::DEFAULT_CHANNELS`
    /// when `AnaConfig` doesn't set it at all -- the absent case
    /// `resolve_treats_an_empty_default_channels_the_same_as_absent`
    /// complements by covering the explicitly-empty one.
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

    /// A hand-edited (or otherwise externally produced) `config.toml`
    /// with `default_channels = []` must not leave a project that
    /// declares no channel override of its own with nothing to solve
    /// against -- `resolve` treats this exactly like the field being
    /// absent, not like a deliberate "search nothing" instruction.
    /// `config_set`'s own empty-values rejection only guards its own
    /// write path; this is the guarantee that holds regardless of how
    /// the empty list got there.
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

    /// An explicitly-set, non-empty `default_channels` is used as-is --
    /// the default is only ever a fallback for the absent/empty case,
    /// never applied on top of (or instead of) a value that's actually
    /// present.
    #[test]
    fn resolve_respects_an_explicit_nonempty_default_channels() {
        let raw = AnaConfig {
            default_channels: Some(vec!["conda-forge".to_string()]),
            ..AnaConfig::default()
        };
        let resolved = resolve(raw).unwrap();
        assert_eq!(resolved.default_channels, vec!["conda-forge".to_string()]);
    }

    /// `pypi_to_conda_uri` falls back to
    /// `ana_config::DEFAULT_PYPI_TO_CONDA_URI` when `AnaConfig` doesn't
    /// set it -- the same "if not set" fallback `default_channels`
    /// already gets, exercised directly against [`resolve`] rather than
    /// through `ANA_CONFIG_PATH`/disk so this test can't race any other
    /// test over that process-wide env var.
    #[test]
    fn resolve_defaults_pypi_to_conda_uri_when_unset() {
        let resolved = resolve(AnaConfig::default()).unwrap();
        assert_eq!(
            resolved.pypi_to_conda_uri.as_str(),
            ana_config::DEFAULT_PYPI_TO_CONDA_URI
        );
    }

    /// An explicitly-set `pypi_to_conda_uri` is used as-is -- the default
    /// is only ever a fallback for the absent case, never applied on top
    /// of (or instead of) a value that's actually present.
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
