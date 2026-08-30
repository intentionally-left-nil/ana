//! Channel name -> real [`Channel`] resolution, with a hardcoded special
//! case for the classic Anaconda `"defaults"` meta-channel.
//!
//! `rattler_conda_types::Channel::from_str`'s alias resolution has no
//! special case for `"defaults"`: given this crate's [`ChannelConfig`] (a
//! bare, generic `conda.anaconda.org`-alias config),
//! `Channel::from_str("defaults", ..)` resolves to
//! `https://conda.anaconda.org/defaults`, which does not exist (verified
//! directly against the live server: a 404 -- `defaults` is not a
//! registered user/org channel there the way `conda-forge` is).
//!
//! The *real* `defaults` alias -- the one `conda` itself and `.condarc`
//! mean -- is Anaconda's own multi-channel `repo.anaconda.com/pkgs/*`
//! set: `pkgs/main` and `pkgs/r` on every platform, plus `pkgs/msys2` on
//! Windows only (MSYS2/Cygwin-derived packages that only make sense on a
//! Windows target). `ana_lockfile::DEFAULT_CHANNELS` is hardcoded to
//! `["defaults"]` today, so this module resolves `"defaults"` to what it
//! actually *means*, in the same channel order `conda`'s own default
//! `.condarc` lists them in, rather than resolving it generically and
//! 404ing -- reusing `ana_lockfile::channels`'s own `DEFAULTS_ALIAS`/
//! `DEFAULTS_BASE_URL`/`defaults_subchannels` as the single source of
//! truth for what `"defaults"` expands to (`ana_lockfile`'s own
//! `validate_locked_packages` checks a locked package's `url` against
//! the same expansion), rather than hardcoding a second copy here.

use ana_lockfile::{defaults_subchannels, DEFAULTS_ALIAS, DEFAULTS_BASE_URL};
use rattler_conda_types::{Channel, ChannelConfig, Platform};
use url::Url;

use crate::Error;

/// Resolves every one of `names` (a
/// [`ana_lockfile::SolveRequest::channels`] entry) into the real
/// [`Channel`]s it stands for when solving for `platform`. `"defaults"`
/// expands to more than one channel, in order (see the module docs);
/// every other name resolves to exactly one, via `channel_config`.
pub(crate) fn resolve(
    names: &[String],
    channel_config: &ChannelConfig,
    platform: Platform,
) -> Result<Vec<Channel>, Error> {
    let mut channels = Vec::with_capacity(names.len());
    for name in names {
        if name == DEFAULTS_ALIAS {
            for subchannel in defaults_subchannels(platform) {
                channels.push(defaults_channel(subchannel)?);
            }
        } else {
            channels.push(Channel::from_str(name, channel_config).map_err(|source| {
                Error::Channel {
                    name: name.clone(),
                    source,
                }
            })?);
        }
    }
    Ok(channels)
}

/// One `repo.anaconda.com/pkgs/<name>` constituent of the `defaults`
/// meta-channel, as a real [`Channel`] -- built directly from a URL, not
/// through [`Channel::from_str`]'s generic alias resolution (which has no
/// special case for it; see the module docs), so this never touches
/// `channel_config`'s own alias at all. `name` is always one of
/// [`ana_lockfile`]'s own hardcoded [`defaults_subchannels`], never
/// external input, so the URL parse below is not expected to fail in
/// practice -- but it is still propagated, not unwrapped, per the
/// workspace's own never-`unwrap`/`expect`-outside-tests policy.
fn defaults_channel(name: &str) -> Result<Channel, Error> {
    let url = Url::parse(&format!("{DEFAULTS_BASE_URL}/{name}"))?;
    Ok(Channel::from_url(url))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn urls(names: &[&str], platform: Platform) -> Vec<String> {
        let config = ChannelConfig::default_with_root_dir(std::path::PathBuf::new());
        resolve(
            &names.iter().map(ToString::to_string).collect::<Vec<_>>(),
            &config,
            platform,
        )
        .unwrap()
        .into_iter()
        .map(|channel| channel.base_url.as_str().to_string())
        .collect()
    }

    #[test]
    fn defaults_expands_to_main_and_r_on_non_windows() {
        assert_eq!(
            urls(&["defaults"], Platform::Linux64),
            vec![
                "https://repo.anaconda.com/pkgs/main/",
                "https://repo.anaconda.com/pkgs/r/",
            ]
        );
    }

    #[test]
    fn defaults_expands_to_main_r_and_msys2_on_windows() {
        assert_eq!(
            urls(&["defaults"], Platform::Win64),
            vec![
                "https://repo.anaconda.com/pkgs/main/",
                "https://repo.anaconda.com/pkgs/r/",
                "https://repo.anaconda.com/pkgs/msys2/",
            ]
        );
    }

    #[test]
    fn a_non_defaults_name_resolves_generically() {
        assert_eq!(
            urls(&["conda-forge"], Platform::Linux64),
            vec!["https://conda.anaconda.org/conda-forge/"]
        );
    }

    #[test]
    fn defaults_can_combine_with_an_ordinary_channel_name() {
        assert_eq!(
            urls(&["defaults", "conda-forge"], Platform::Linux64),
            vec![
                "https://repo.anaconda.com/pkgs/main/",
                "https://repo.anaconda.com/pkgs/r/",
                "https://conda.anaconda.org/conda-forge/",
            ]
        );
    }

    #[test]
    fn an_invalid_channel_name_is_a_channel_error() {
        let config = ChannelConfig::default_with_root_dir(std::path::PathBuf::new());
        // A relative-path-shaped channel with an empty (non-absolute)
        // configured root dir -- `Channel::from_str` cannot resolve it
        // to an absolute path, so this is guaranteed to fail to parse
        // regardless of host platform.
        let err = resolve(
            &["./not-a-real-channel".to_string()],
            &config,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Channel { .. }));
    }
}
