//! This crate's error type.

use rattler_conda_types::ParseChannelError;
use url::Url;

/// Which part of a channel/artifact URL made it unsafe to write into
/// durable output. Named only on [`normalize_channel`](crate::normalize_channel)'s
/// failure path, so identifying the offense never costs anything on the
/// (common) success path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialOffense {
    /// A username and/or password in the URL's userinfo.
    Userinfo,
    /// A `/t/<token>/` path segment.
    Token,
    /// A query string.
    Query,
    /// A fragment that is not a `md5:<hex>`/`sha256:<hex>` artifact digest.
    Fragment,
}

impl std::fmt::Display for CredentialOffense {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CredentialOffense::Userinfo => "userinfo (a username and/or password)",
            CredentialOffense::Token => "a /t/<token>/ path segment",
            CredentialOffense::Query => "a query string",
            CredentialOffense::Fragment => "a fragment that is not an artifact digest",
        })
    }
}

/// Everything channel normalization and policy validation can fail on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A channel name/URL did not parse as a channel at all -- an
    /// admin/project/package typo, not a policy violation.
    #[error("invalid channel {name:?}: {source}")]
    InvalidChannel {
        name: String,
        #[source]
        source: ParseChannelError,
    },

    /// An entry named a meta-channel (`"defaults"`) where a single channel
    /// is required -- a meta-channel is a channel-*list* concept, so it
    /// never reaches [`normalize_channel`](crate::normalize_channel) as a
    /// value. `members` names the real channels it expands to, so the
    /// error is actionable.
    #[error(
        "{name:?} is a meta-channel (expands to {members:?}), not a single channel; \
         name one of its members directly"
    )]
    MetaChannelNotASingleChannel { name: String, members: Vec<String> },

    /// A channel URL is not already in its serialization-safe form -- see
    /// [`normalize_channel`](crate::normalize_channel)'s module docs for
    /// why this equality check is the complete gate.
    #[error("channel url {url} carries {offense}, which ana does not support")]
    CredentialedChannelNotSupported {
        url: Url,
        offense: CredentialOffense,
    },

    /// A channel resolved to a local filesystem path (a `file://` URL, or
    /// a bare absolute/`~/` path, which resolves to the same thing).
    #[error("channel url {url} is a local filesystem path, which ana does not support")]
    LocalChannelNotSupported { url: Url },

    /// A `/*` wildcard pattern appeared somewhere other than
    /// `allowed_channels`: a search list (`default_channels`, a project's
    /// own `conda-channels`, `dry_solve_channels`) or a per-package pin,
    /// none of which admit a pattern -- each names a single channel.
    #[error("{entry:?} is a wildcard pattern (`/*`), only allowed in allowed_channels")]
    WildcardNotAllowedHere { entry: String },

    /// One or more channel/url overrides (a project's own
    /// `conda-channels`/`# ana-channels:`, or an individual package's
    /// `channel::`/url override) named a channel not present in the
    /// authorized set. Every violation found is listed, not just the
    /// first.
    #[error("the following channels are not allowed:\n{0}")]
    ChannelNotAllowed(String),

    /// Internal: a static alias-table URL failed to parse. Never expected
    /// in practice -- the table is fixed, compiled-in data -- but
    /// propagated rather than unwrapped, per this workspace's own rule.
    #[error("internal error: alias url {url:?} does not parse: {source}")]
    AliasUrl {
        url: &'static str,
        #[source]
        source: url::ParseError,
    },
}
