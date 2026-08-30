//! PyPI → conda package name mapping cache for `ana`.
//!
//! Fetches `{"pypi_name": "conda_name"}` from an internal API, keeps only
//! the entries where the two normalized names actually differ, and caches
//! that filtered table on disk as MessagePack so [`load`] can hand it back
//! to callers (chiefly `ana-pep508-to-matchspec::convert`'s name-mapping
//! lookup, via [`MappingHandle::get`]) as an in-memory lookup.
//!
//! [`load`] blocks on network I/O only when there's no cache at all, or
//! the cache is stale beyond a week without
//! [`LoadOptions::allow_stale_mapping`], and uses short timeouts even
//! then. There is no silent identity-mapping fallback: an empty/unset
//! mapping URL, or a blocking path that can't reach the network, is
//! [`MappingError`], not an empty map. A cache is only ever valid for the
//! URL it was fetched from; a stale-URL cache is discarded and deleted
//! rather than reused (see `envelope::read_for_url`).
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod cache_dir;
mod envelope;
mod error;
mod fetch;
mod http;
mod load;
mod refresh;

pub use cache_dir::cache_dir;
pub use error::{FetchError, InvalidMappedName, MappingError};
pub use http::HttpError;
pub use load::{load, LoadOptions, MappingHandle};
pub use refresh::RefreshOutcome;
