//! Error types. [`MappingError`] is the only public one -- it's reachable
//! solely from [`crate::load`]'s blocking paths (no cache at all, or a
//! cache stale beyond a week without `--allow-stale-mapping`) failing with
//! nothing usable to fall back to. Every other path in this crate degrades
//! silently (stale-but-usable data, or an empty map) instead of surfacing
//! an error.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MappingError {
    #[error("could not determine or create the ana cache directory")]
    CacheDir,

    #[error("could not acquire the ana-pypi-conda-map cache lock: {0}")]
    Lock(#[from] io::Error),

    #[error("failed to download the pypi/conda name mapping: {0}")]
    Fetch(#[from] FetchError),
}

/// Reachable only as the payload of [`MappingError::Fetch`] -- not
/// constructed directly by callers, but `pub` (rather than `pub(crate)`) so
/// that payload is actually nameable/matchable from outside this crate,
/// same as `MappingError` itself.
#[derive(Debug, Error)]
pub enum FetchError {
    #[error("request failed: {0}")]
    Http(#[from] crate::http::HttpError),

    #[error("failed to parse mapping response as JSON: {0}")]
    Json(#[from] serde_json::Error),
}
