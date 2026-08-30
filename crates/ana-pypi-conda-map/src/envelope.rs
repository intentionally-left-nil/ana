//! On-disk cache envelope: a single MessagePack-encoded file holding both
//! HTTP cache-validator bookkeeping and the mapping payload together,
//! rather than a config file plus a separate data file, since replacing
//! two files individually isn't atomic as a pair.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Bump whenever [`CacheEnvelope`]'s shape changes in a way that isn't
/// forward/backward compatible. A file whose `schema_version` doesn't
/// match is treated identically to a missing or corrupt file by [`read`].
pub(crate) const SCHEMA_VERSION: u16 = 2;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub(crate) struct CacheEnvelope {
    pub schema_version: u16,

    /// The mapping endpoint this envelope's `etag`/`last_modified`/`mapping`
    /// were fetched from. Checked by [`read_for_url`]/[`evict_if_mismatched`]
    /// on every read: a cache written for one URL is never valid for
    /// another, so a mismatch is treated as no cache at all. `""` for an
    /// envelope built by [`CacheEnvelope::new_empty`] with no URL in hand
    /// (test fixtures only -- every real envelope carries the URL it was
    /// fetched from).
    pub url: String,

    /// Validators from the last successful GET, used to make the periodic
    /// freshness check conditional (HEAD, or a conditional-GET fallback)
    /// instead of an unconditional re-download every time.
    pub etag: Option<String>,
    pub last_modified: Option<String>,

    /// Last time the *payload* actually changed (a successful GET returned
    /// 200, not a HEAD/GET that reported "unchanged"). Informational only
    /// -- not read by the state machine in `refresh`.
    pub fetched_at: Option<u64>,

    /// Last time `mapping` was CONFIRMED current: a fresh GET, or a
    /// HEAD/conditional check that reported "unchanged." This is the
    /// field the 24h/1-week thresholds in `refresh::decide` are computed
    /// from. `None` means "never successfully confirmed," treated
    /// identically to no cache file existing at all.
    pub last_checked_at: Option<u64>,

    /// Set the instant a HEAD/conditional check confirms the server has
    /// newer content than `mapping` reflects; cleared only once a
    /// follow-up download succeeds. Lets a run killed between "confirmed
    /// stale" and "downloaded the replacement" resume by skipping
    /// straight to the download instead of re-checking.
    pub known_stale: bool,

    /// Network-level failures (timeout, DNS, unexpected status) since the
    /// last successful attempt of either kind. A check that successfully
    /// reports "stale" does NOT increment this -- that's a successful
    /// network operation, not a failure.
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
/// cache" -- never a hard error.
pub(crate) fn read(path: &Path) -> Option<CacheEnvelope> {
    let bytes = fs::read(path).ok()?;
    let envelope: CacheEnvelope = rmp_serde::from_slice(&bytes).ok()?;
    if envelope.schema_version != SCHEMA_VERSION {
        return None;
    }
    Some(envelope)
}

/// [`read`], plus: an envelope cached for a URL other than `url` is
/// treated as no cache at all. Never touches the file on a mismatch, so
/// it's safe to call without holding the refresh lock (`load`'s own
/// unlocked fast-path read goes through this).
pub(crate) fn read_for_url(path: &Path, url: &str) -> Option<CacheEnvelope> {
    let envelope = read(path)?;
    (envelope.url == url).then_some(envelope)
}

/// [`read_for_url`], plus: a mismatched cache file is deleted on the spot
/// (best-effort). Only safe to call while holding `perform_refresh`'s
/// exclusive lock -- without it, this could delete a different process's
/// freshly-written envelope for the current URL in the window between
/// that process's read and this function's own.
pub(crate) fn evict_if_mismatched(path: &Path, url: &str) -> Option<CacheEnvelope> {
    let envelope = read(path)?;
    if envelope.url == url {
        return Some(envelope);
    }
    let _ = fs::remove_file(path);
    None
}

/// Atomically replaces the cache file's contents via
/// [`ana_fs_util::write_atomic`] (tempfile-in-same-directory + rename,
/// fsynced on both sides). A reader never sees a half-written file, and a
/// crash leaves the old or the new complete file, never a torn one.
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

    #[test]
    fn read_for_url_returns_the_envelope_when_the_url_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut envelope = CacheEnvelope::new_empty();
        envelope.url = "https://example.invalid/mapping.json".to_string();
        envelope
            .mapping
            .insert("opencv-python".to_string(), "py-opencv".to_string());
        write_atomic(&path, &envelope).unwrap();

        let read_back = read_for_url(&path, "https://example.invalid/mapping.json")
            .expect("same URL should still be usable");
        assert_eq!(read_back, envelope);
        assert!(path.exists(), "a matching URL must never delete the cache");
    }

    /// `read_for_url` must never delete a mismatched file -- that's what
    /// distinguishes it from [`evict_if_mismatched`], and lets an
    /// unlocked caller check usability without racing a concurrent
    /// writer.
    #[test]
    fn read_for_url_leaves_a_mismatched_cache_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut envelope = CacheEnvelope::new_empty();
        envelope.url = "https://old.invalid/mapping.json".to_string();
        envelope
            .mapping
            .insert("opencv-python".to_string(), "py-opencv".to_string());
        write_atomic(&path, &envelope).unwrap();

        assert!(
            read_for_url(&path, "https://new.invalid/mapping.json").is_none(),
            "a cache built for a different URL must never be usable"
        );
        assert!(
            path.exists(),
            "read_for_url must never delete the file, even on a URL mismatch"
        );
    }

    #[test]
    fn read_for_url_of_a_missing_file_is_no_cache_and_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.msgpack");
        assert!(read_for_url(&path, "https://example.invalid/mapping.json").is_none());
    }

    #[test]
    fn evict_if_mismatched_returns_the_envelope_when_the_url_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut envelope = CacheEnvelope::new_empty();
        envelope.url = "https://example.invalid/mapping.json".to_string();
        envelope
            .mapping
            .insert("opencv-python".to_string(), "py-opencv".to_string());
        write_atomic(&path, &envelope).unwrap();

        let read_back = evict_if_mismatched(&path, "https://example.invalid/mapping.json")
            .expect("same URL should still be usable");
        assert_eq!(read_back, envelope);
        assert!(path.exists(), "a matching URL must never delete the cache");
    }

    /// A cache built for a different URL must be discarded and deleted,
    /// not just ignored in memory -- only safe to rely on here (unlike
    /// [`read_for_url`]) because this is only called under
    /// `perform_refresh`'s exclusive lock.
    #[test]
    fn evict_if_mismatched_discards_and_deletes_a_cache_for_a_different_url() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");

        let mut envelope = CacheEnvelope::new_empty();
        envelope.url = "https://old.invalid/mapping.json".to_string();
        envelope.etag = Some("\"old-etag\"".to_string());
        envelope.last_checked_at = Some(1_000);
        envelope
            .mapping
            .insert("opencv-python".to_string(), "py-opencv".to_string());
        write_atomic(&path, &envelope).unwrap();

        assert!(
            evict_if_mismatched(&path, "https://new.invalid/mapping.json").is_none(),
            "a cache built for a different URL must never be usable"
        );
        assert!(
            !path.exists(),
            "the stale, wrong-URL cache file must be deleted, not merely ignored in memory"
        );
        assert!(read(&path).is_none());
    }

    #[test]
    fn evict_if_mismatched_of_a_missing_file_is_no_cache_and_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.msgpack");
        assert!(evict_if_mismatched(&path, "https://example.invalid/mapping.json").is_none());
    }
}
