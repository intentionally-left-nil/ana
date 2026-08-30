//! This crate's error type.

/// Everything channel-policy validation can fail on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// One or more channel/url overrides (a project's own
    /// `conda-channels`/`# ana-channels:`, or an individual package's
    /// `channel::`/url override) named a channel not present in
    /// `default_channels ∪ allowed_channels`. Every violation found is
    /// listed, not just the first.
    #[error("the following channels are not allowed:\n{0}")]
    ChannelNotAllowed(String),

    /// An entry of `default_channels`, `allowed_channels`, or a
    /// project's own `conda-channels`/`# ana-channels:` did not parse as
    /// a channel name or URL at all -- an admin/project typo, not a
    /// policy violation.
    #[error("invalid channel {name:?}: {source}")]
    InvalidChannel {
        name: String,
        #[source]
        source: rattler_conda_types::ParseChannelError,
    },

    /// An entry of `default_channels`, `allowed_channels`, or a
    /// project's own `conda-channels`/`# ana-channels:` resolved to a
    /// local filesystem path (a `file://` URL, or a bare absolute/`~/`
    /// path) rather than a remote channel. Local filesystem channels are
    /// not supported, regardless of source.
    #[error("channel {name:?} resolves to a local filesystem path, which ana does not support")]
    LocalChannelNotSupported { name: String },
}
