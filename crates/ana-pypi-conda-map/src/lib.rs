//! PyPI → conda package name mapping cache for `ana`.
//!
//! Fetches `{"pypi_name": "conda_name"}` from an internal API, keeps only
//! the entries where the two normalized names actually differ, and caches
//! that filtered table on disk as MessagePack so [`load`] can hand it back
//! to callers (chiefly the deferred name-mapping call site in
//! `ana-pep508-to-matchspec`) as a plain, already-in-memory `HashMap`
//! lookup.
//!
//! [`load`] never blocks the caller on network I/O except in the two cases
//! where there's genuinely nothing better to do (no cache at all, or a
//! cache stale beyond a week without [`LoadOptions::allow_stale_mapping`]),
//! and even then uses short timeouts so a bounded failure is fast.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod cache_dir;
mod envelope;
mod error;
mod fetch;
mod http;
mod load;
mod refresh;

pub use cache_dir::cache_dir;
pub use error::{FetchError, MappingError};
pub use http::HttpError;
pub use load::{load, LoadOptions, MappingHandle};
pub use refresh::RefreshOutcome;
