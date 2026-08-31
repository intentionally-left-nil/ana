//! [`normalize_channel`]: the only code in `ana` that produces a canonical
//! channel URL. Every [`Channel`] anywhere in `ana` has been through it.
//!
//! A channel's identity is its [`rattler_conda_types::ChannelUrl`]: an
//! absolute URL with a forced trailing slash, which already derives
//! `Hash`/`Eq`/`Ord`. No newtype wraps it.
//!
//! This is sound because [`normalize_channel`] rejects any channel URL
//! carrying credentials, so `Channel::canonical_name()` -- which redacts --
//! is byte-identical to `base_url.as_str()`, which is what the gateway
//! writes into `RepoDataRecord::channel` and what `rattler_solve`'s
//! resolvo backend compares a pinned spec's channel against. The
//! rejection is one comparison against one upstream function:
//!
//! ```text
//! rattler_redaction::strip_url_for_serialization(url) == *url
//! ```
//!
//! `strip_url_for_serialization` removes userinfo, a `/t/<token>/` path
//! prefix, the query string, and a non-digest fragment -- the complete set
//! of things that must not appear in a channel base URL, tracked upstream
//! rather than re-enumerated here. On the failure path only, the URL is
//! inspected to name which part offends -- userinfo, token segment, query,
//! or fragment -- so the error is specific without a second enumeration
//! deciding whether to reject.

use rattler_conda_types::Channel;
use url::Url;

use crate::alias::{meta_channel_members, ALIASES};
use crate::error::{CredentialOffense, Error};

/// Rewrites `channel` to its canonical location. Idempotent.
///
/// - A channel whose `name` is a meta-channel name (`"defaults"`) is
///   [`Error::MetaChannelNotASingleChannel`], listing the members. A
///   meta-channel is a channel-list concept; it never reaches this
///   function as a value.
/// - A channel URL carrying credentials is
///   [`Error::CredentialedChannelNotSupported`].
/// - A `file://` base URL is [`Error::LocalChannelNotSupported`].
/// - An alias-table hit is rewritten to that entry's canonical URL,
///   setting both `base_url` and `name`. A hit is either
///   `channel.name == Some(entry.name)` or `channel.base_url == entry.url`.
///   The first arm covers a bare name and the equivalent
///   `conda.anaconda.org` URL together, because rattler's
///   `strip_channel_alias` already gives both `name: Some("main")`; the
///   second makes the function idempotent.
/// - Anything else passes through unchanged: `conda-forge` keeps resolving
///   to `conda.anaconda.org/conda-forge`, and an explicit URL is taken
///   literally and never rewritten.
pub fn normalize_channel(channel: Channel) -> Result<Channel, Error> {
    if let Some(name) = channel.name.as_deref() {
        if let Some(members) = meta_channel_members(name) {
            return Err(Error::MetaChannelNotASingleChannel {
                name: name.to_string(),
                members: members
                    .iter()
                    .map(|member| member.alias.name.to_string())
                    .collect(),
            });
        }
    }

    let url = channel.base_url.as_ref();
    credential_gate(url)?;
    if url.scheme() == "file" {
        return Err(Error::LocalChannelNotSupported { url: url.clone() });
    }

    match alias_hit(&channel)? {
        Some((name, url)) => Ok(Channel {
            base_url: url.into(),
            name: Some(name.to_string()),
            platforms: channel.platforms,
        }),
        None => Ok(channel),
    }
}

/// The [`crate::alias::ALIASES`] entry `channel` resolves to, if any --
/// see [`normalize_channel`]'s docs for the two ways a hit is recognized.
fn alias_hit(channel: &Channel) -> Result<Option<(&'static str, Url)>, Error> {
    for entry in ALIASES {
        let entry_url = parse_alias_url(entry.url)?;
        if channel.name.as_deref() == Some(entry.name)
            || urls_match_ignoring_trailing_slash(channel.base_url.as_ref(), &entry_url)
        {
            return Ok(Some((entry.name, entry_url)));
        }
    }
    Ok(None)
}

pub(crate) fn parse_alias_url(url: &'static str) -> Result<Url, Error> {
    Url::parse(url).map_err(|source| Error::AliasUrl { url, source })
}

fn urls_match_ignoring_trailing_slash(a: &Url, b: &Url) -> bool {
    a.as_str().trim_end_matches('/') == b.as_str().trim_end_matches('/')
}

fn classify_offense(url: &Url) -> CredentialOffense {
    if !url.username().is_empty() || url.password().is_some() {
        return CredentialOffense::Userinfo;
    }
    let has_token = url.path_segments().is_some_and(|mut segments| {
        matches!((segments.next(), segments.next()), (Some("t"), Some(token)) if !token.is_empty())
    });
    if has_token {
        return CredentialOffense::Token;
    }
    if url.query().is_some() {
        return CredentialOffense::Query;
    }
    CredentialOffense::Fragment
}

/// Rejects `url` unless it is already in its serialization-safe form --
/// see the module docs for why this equality check is the complete gate.
fn credential_gate(url: &Url) -> Result<(), Error> {
    let stripped = rattler_redaction::strip_url_for_serialization(url);
    if stripped == *url {
        return Ok(());
    }
    Err(Error::CredentialedChannelNotSupported {
        url: url.clone(),
        offense: classify_offense(url),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::PathBuf;

    use rattler_conda_types::ChannelConfig;

    use super::*;

    fn config() -> ChannelConfig {
        ChannelConfig::default_with_root_dir(PathBuf::new())
    }

    fn channel(text: &str) -> Channel {
        Channel::from_str(text, &config()).unwrap()
    }

    #[test]
    fn bare_alias_name_resolves_to_its_canonical_url() {
        let result = normalize_channel(channel("main")).unwrap();
        assert_eq!(
            result.base_url.as_str(),
            "https://repo.anaconda.com/pkgs/main/"
        );
        assert_eq!(result.name, Some("main".to_string()));
    }

    #[test]
    fn equivalent_conda_anaconda_org_url_resolves_the_same_way() {
        let result = normalize_channel(channel("https://conda.anaconda.org/main")).unwrap();
        assert_eq!(
            result.base_url.as_str(),
            "https://repo.anaconda.com/pkgs/main/"
        );
        assert_eq!(result.name, Some("main".to_string()));
    }

    #[test]
    fn already_canonical_url_is_unchanged() {
        let result = normalize_channel(channel("https://repo.anaconda.com/pkgs/main")).unwrap();
        assert_eq!(
            result.base_url.as_str(),
            "https://repo.anaconda.com/pkgs/main/"
        );
        assert_eq!(result.name, Some("main".to_string()));
    }

    #[test]
    fn a_non_table_name_passes_through_unchanged() {
        let result = normalize_channel(channel("conda-forge")).unwrap();
        assert_eq!(
            result.base_url.as_str(),
            "https://conda.anaconda.org/conda-forge/"
        );
    }

    #[test]
    fn an_unrelated_url_passes_through_unchanged() {
        let result = normalize_channel(channel("https://repo.mycompany.com/conda")).unwrap();
        assert_eq!(
            result.base_url.as_str(),
            "https://repo.mycompany.com/conda/"
        );
    }

    #[test]
    fn a_file_scheme_channel_is_rejected() {
        let err = normalize_channel(channel("file:///tmp/local-channel")).unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_absolute_path_is_rejected_as_a_local_channel() {
        let err = normalize_channel(channel("/tmp/local-channel")).unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_defaults_name_is_a_meta_channel_error() {
        let err = normalize_channel(channel("defaults")).unwrap_err();
        match err {
            Error::MetaChannelNotASingleChannel { name, members } => {
                assert_eq!(name, "defaults");
                assert_eq!(members, vec!["main", "r", "msys2"]);
            }
            other => panic!("expected MetaChannelNotASingleChannel, got {other:?}"),
        }
    }

    #[test]
    fn a_token_prefixed_url_is_rejected() {
        let err = normalize_channel(channel("https://conda.anaconda.org/t/abc123/conda-forge"))
            .unwrap_err();
        assert!(matches!(
            err,
            Error::CredentialedChannelNotSupported {
                offense: CredentialOffense::Token,
                ..
            }
        ));
    }

    #[test]
    fn a_userinfo_url_is_rejected() {
        let err = normalize_channel(channel("https://user:pass@example.com/channel")).unwrap_err();
        assert!(matches!(
            err,
            Error::CredentialedChannelNotSupported {
                offense: CredentialOffense::Userinfo,
                ..
            }
        ));
    }

    #[test]
    fn a_query_string_url_is_rejected() {
        let err = normalize_channel(channel("https://example.com/channel?tok=abc")).unwrap_err();
        assert!(matches!(
            err,
            Error::CredentialedChannelNotSupported {
                offense: CredentialOffense::Query,
                ..
            }
        ));
    }

    #[test]
    fn a_non_digest_fragment_url_is_rejected() {
        let err =
            normalize_channel(channel("https://example.com/channel#not-a-digest")).unwrap_err();
        assert!(matches!(
            err,
            Error::CredentialedChannelNotSupported {
                offense: CredentialOffense::Fragment,
                ..
            }
        ));
    }

    #[test]
    fn an_artifact_digest_fragment_is_not_a_credential() {
        let result = normalize_channel(channel(
            "https://example.com/channel#sha256:aabbccddeeff00112233445566778899aabbccddeeff0011223344556677",
        ));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn a_token_less_url_with_a_trailing_slash_is_never_rejected_by_the_gate() {
        let result = normalize_channel(channel("https://example.com/channel/"));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn normalize_channel_is_idempotent() {
        let once = normalize_channel(channel("main")).unwrap();
        let twice = normalize_channel(once.clone()).unwrap();
        assert_eq!(once, twice);

        let once = normalize_channel(channel("conda-forge")).unwrap();
        let twice = normalize_channel(once.clone()).unwrap();
        assert_eq!(once, twice);
    }

    /// `Channel::canonical_name()` -- what `rattler_solve`'s resolvo
    /// backend compares a pinned spec's channel against -- must be
    /// byte-identical to `base_url.as_str()` for every channel this
    /// function can return, since that's the exact expression
    /// `rattler_repodata_gateway`'s `sparse/mod.rs` stamps into
    /// `RepoDataRecord::channel`. If the two ever diverge for a channel
    /// `normalize_channel` allows through, a pinned spec would never match
    /// any candidate for its own channel.
    #[test]
    fn canonical_name_matches_base_url_for_every_normalized_channel() {
        for text in [
            "main",
            "https://conda.anaconda.org/main",
            "https://repo.anaconda.com/pkgs/main",
            "conda-forge",
            "https://repo.mycompany.com/conda",
        ] {
            let result = normalize_channel(channel(text)).unwrap();
            assert_eq!(result.canonical_name(), result.base_url.as_str());
        }
    }

    #[test]
    fn a_channel_with_an_explicit_subdir_keeps_it() {
        let result = normalize_channel(channel("main[linux-64]")).unwrap();
        assert_eq!(
            result.base_url.as_str(),
            "https://repo.anaconda.com/pkgs/main/"
        );
        assert_eq!(
            result.platforms,
            Some(vec![rattler_conda_types::Platform::Linux64])
        );
    }
}
