//! On-disk cache envelope: the single MessagePack-encoded file that holds
//! both HTTP cache-validator bookkeeping and the mapping payload together.
//! See `investigations/pypi_conda_map.md`, "One file, not a config file
//! plus a data file," for why these live in one struct instead of two
//! separately-written files -- atomically replacing two files individually
//! does not make replacing them *as a pair* atomic, and there's no benefit
//! to the split here since every hot-path read has to decode the whole
//! envelope into memory regardless.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Bump whenever [`CacheEnvelope`]'s shape changes in a way that isn't
/// forward/backward compatible. A file whose `schema_version` doesn't match
/// is treated identically to a missing or corrupt file by [`read`] -- never
/// a hard error, never a panic.
pub(crate) const SCHEMA_VERSION: u16 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub(crate) struct CacheEnvelope {
    pub schema_version: u16,

    /// Validators from the last successful GET, used to make the periodic
    /// freshness check conditional (HEAD, or a conditional-GET fallback)
    /// instead of an unconditional re-download every time.
    pub etag: Option<String>,
    pub last_modified: Option<String>,

    /// Last time the *payload* actually changed (a successful GET returned
    /// 200, not a HEAD/GET that reported "unchanged"). Informational only --
    /// not read by the state machine in `refresh`, kept for future
    /// diagnostics (e.g. an `ana cache info` command).
    pub fetched_at: Option<u64>,

    /// Last time `mapping` was CONFIRMED current: a fresh GET, or a
    /// HEAD/conditional check that reported "unchanged." This is the single
    /// field the 24h/1-week thresholds in `refresh::decide` are computed
    /// from. `None` means "never successfully confirmed," which `decide`
    /// treats identically to no cache file existing at all.
    pub last_checked_at: Option<u64>,

    /// Set the instant a HEAD/conditional check confirms the server has
    /// newer content than `mapping` reflects; cleared only once a follow-up
    /// download succeeds. Lets a run killed between "confirmed stale" and
    /// "downloaded the replacement" resume correctly: the next run sees
    /// this flag and skips straight to the download instead of redundantly
    /// re-checking.
    pub known_stale: bool,

    /// Network-level failures (timeout, DNS, unexpected status) since the
    /// last successful attempt of either kind. A check that successfully
    /// reports "stale" does NOT increment this -- that's a successful
    /// network operation with a business-level "you need new data" result,
    /// not a failure.
    pub consecutive_failures: u32,

    /// Last time ANY check or download was attempted, success or failure.
    /// Feeds `refresh`'s backoff cooldown; distinct from `last_checked_at`
    /// because a failed attempt bumps this without bumping that.
    pub last_attempt_at: Option<u64>,

    /// Normalized `pypi_name -> conda_name`. Only entries that differ.
    pub mapping: HashMap<String, String>,
}

impl CacheEnvelope {
    pub(crate) fn new_empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            ..Default::default()
        }
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads and decodes the cache file. Any problem at all (missing file, I/O
/// error, corrupt MessagePack, schema mismatch) is treated as "no usable
/// cache" -- this crate's contract is to degrade, never to error out of a
/// load because of a bad cache file.
pub(crate) fn read(path: &Path) -> Option<CacheEnvelope> {
    let bytes = fs::read(path).ok()?;
    let envelope: CacheEnvelope = rmp_serde::from_slice(&bytes).ok()?;
    if envelope.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(envelope)
}

/// Atomically replaces the cache file's contents, via the shared
/// [`ana_fs_util::write_atomic`] (tempfile-in-same-directory + rename,
/// fsynced on both sides of the rename). A reader that opened the previous
/// version before this call completes keeps reading that complete, valid
/// old version; nothing is ever visible half-written, and a crash leaves
/// the old or the new complete file, never a torn one.
pub(crate) fn write_atomic(path: &Path, envelope: &CacheEnvelope) -> io::Result<()> {
    let bytes =
        rmp_serde::to_vec(envelope).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    ana_fs_util::write_atomic(path, &bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn round_trips_through_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut envelope = CacheEnvelope::new_empty();
        envelope
            .mapping
            .insert("foo-bar".to_string(), "foo_bar".to_string());
        envelope.etag = Some("\"abc123\"".to_string());
        envelope.last_checked_at = Some(1_000);

        write_atomic(&path, &envelope).unwrap();
        let read_back = read(&path).expect("just-written file should be readable");
        assert_eq!(read_back, envelope);
    }

    #[test]
    fn missing_file_is_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.msgpack");
        assert!(read(&path).is_none());
    }

    #[test]
    fn corrupt_file_is_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        fs::write(&path, b"not valid msgpack at all").unwrap();
        assert!(read(&path).is_none());
    }

    #[test]
    fn schema_version_mismatch_is_no_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut envelope = CacheEnvelope::new_empty();
        envelope.schema_version = SCHEMA_VERSION + 1;
        let bytes = rmp_serde::to_vec(&envelope).unwrap();
        fs::write(&path, bytes).unwrap();

        assert!(read(&path).is_none());
    }

    #[test]
    fn write_atomic_replaces_previous_contents_wholesale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut first = CacheEnvelope::new_empty();
        first.mapping.insert("a".to_string(), "b".to_string());
        write_atomic(&path, &first).unwrap();

        let mut second = CacheEnvelope::new_empty();
        second.mapping.insert("c".to_string(), "d".to_string());
        write_atomic(&path, &second).unwrap();

        let read_back = read(&path).unwrap();
        assert_eq!(read_back, second);
    }
}
