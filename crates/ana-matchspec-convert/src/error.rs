//! This crate's error type.

/// Everything conversion can fail on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The target platform has no marker-environment mapping -- only the
    /// six installable subdirs are supported.
    #[error("{0}")]
    UnsupportedPlatform(#[from] ana_marker_matchspec::UnsupportedPlatform),

    /// One or more requirements could not be converted to matchspecs for
    /// the target platform. Every failure is listed, not just the first.
    #[error("failed to convert requirements to matchspecs:\n{0}")]
    Conversion(String),
}
