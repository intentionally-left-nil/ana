//! Error types. [`MappingError`] is the only public one -- it's reachable
//! from [`crate::load`]'s blocking paths (no mapping URL configured at
//! all, no cache and nothing downloadable, or a cache stale beyond a week
//! without `--allow-stale-mapping`) failing with nothing usable to fall
//! back to. Every other path in this crate degrades silently
//! (stale-but-usable data, or an empty map) instead of surfacing an
//! error.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MappingError {
    /// [`crate::load::load`] was called with an empty (or all-whitespace)
    /// mapping URL -- the caller's `pypi_to_conda_uri` configuration is
    /// effectively unset. There is no sensible mapping to fetch, cache,
    /// or fall back to in that case: correctly converting a PyPI
    /// requirement to a conda matchspec depends on this lookup table, so
    /// this is a hard error rather than a silent identity-mapping
    /// fallback.
    #[error(
        "no pypi-to-conda mapping URL is configured; set pypi_to_conda_uri (see `ana config`)"
    )]
    UrlNotConfigured,

    #[error("could not determine or create the ana cache directory")]
    CacheDir,

    #[error("could not acquire the ana-pypi-conda-map cache lock: {0}")]
    Lock(#[from] io::Error),

    #[error("failed to download the pypi/conda name mapping: {0}")]
    Fetch(#[from] FetchError),
}

/// [`MappingHandle::get`]'s error: the entry `pypi_name` mapped to exists,
/// but `conda_name` itself doesn't pass the same PEP 503/CEP-26 shape
/// check `fetch.rs::normalize_and_filter` already runs at fetch time.
/// Reachable despite that upstream check because a value that satisfied
/// it at fetch time can still reach `get` through a different, unchecked
/// path -- most notably a cache file read back off disk (`envelope::read`
/// deserializes `mapping: HashMap<String, String>` with no re-validation)
/// -- so `get` re-checks the one entry actually being looked up rather
/// than trusting every entry already made it through fetch-time
/// filtering. Absence of an entry for a name is never this error: [`get`]
/// treats a name the table doesn't mention as the identity mapping,
/// exactly like a genuinely empty table would.
///
/// [`MappingHandle::get`]: crate::MappingHandle::get
/// [`get`]: crate::MappingHandle::get
#[derive(Debug, Error, PartialEq, Eq)]
#[error("mapped conda name {conda_name:?} for pypi package {pypi_name:?} is not a valid conda package name")]
pub struct InvalidMappedName {
    pub pypi_name: String,
    pub conda_name: String,
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
