//! On-disk cache envelope: the single MessagePack-encoded file that holds
//! both HTTP cache-validator bookkeeping and the mapping payload together,
//! rather than a config file plus a separate data file -- atomically
//! replacing two files individually doesn't make replacing them *as a
//! pair* atomic, and every hot-path read decodes the whole envelope into
//! memory anyway, so splitting them buys nothing.

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
pub(crate) const SCHEMA_VERSION: u16 = 2;

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub(crate) struct CacheEnvelope {
    pub schema_version: u16,

    /// The mapping endpoint this envelope's `etag`/`last_modified`/`mapping`
    /// were fetched from. Checked by [`read_for_url`]/[`evict_if_mismatched`]
    /// on every read: a cache written for one URL is never valid for
    /// another (reusing its `mapping`, or sending its validators to a
    /// different server, would silently serve or negotiate wrong data), so
    /// a mismatch here is always treated as no cache at all. `""` for an
    /// envelope built by [`CacheEnvelope::new_empty`] with no URL in hand
    /// (test fixtures only in practice -- every real envelope is
    /// constructed with the URL it was actually fetched from).
    pub url: String,

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

/// [`read`], plus one additional invalidation rule: an envelope cached for
/// a URL other than `url` is never usable (see [`CacheEnvelope::url`]'s
/// docs), so it's treated identically to no cache at all. Unlike
/// [`evict_if_mismatched`], this never touches the file on a mismatch --
/// safe to call without holding the refresh lock (`load`'s own unlocked
/// fast-path read goes through this one), since a read-only check can
/// never race a concurrent, correctly-locked write and delete a file out
/// from under it.
pub(crate) fn read_for_url(path: &Path, url: &str) -> Option<CacheEnvelope> {
    let envelope = read(path)?;
    (envelope.url == url).then_some(envelope)
}

/// [`read_for_url`], plus deletion of a mismatched cache file: an envelope
/// cached for a URL other than `url` is deleted on the spot -- best-effort;
/// a failed delete just means the next call re-discovers the same mismatch
/// and tries again. Only safe to call while holding `perform_refresh`'s
/// exclusive lock: deleting here is what actually reclaims a stale,
/// wrong-URL cache file, so doing it without the lock could delete a
/// different process's freshly-written, correctly-tagged envelope for the
/// current URL in the window between that process's read and this
/// function's own read.
pub(crate) fn evict_if_mismatched(path: &Path, url: &str) -> Option<CacheEnvelope> {
    let envelope = read(path)?;
    if envelope.url == url {
        return Some(envelope);
    }
    let _ = fs::remove_file(path);
    None
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

    /// A URL mismatch on the non-evicting read path must never touch the
    /// file -- this is the whole reason [`read_for_url`] and
    /// [`evict_if_mismatched`] are two separate functions: an unlocked
    /// caller (`load`'s own fast path) must be able to detect "this cache
    /// isn't usable" without risking a delete that could race a
    /// concurrent, correctly-locked writer.
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

    /// The scenario this whole function exists for: `pypi_to_conda_uri`
    /// gets reconfigured to point somewhere else. The old cache -- built
    /// for the old URL, `mapping` and `etag`/`last_modified` included --
    /// must never be handed back as if it were current for the new URL,
    /// and must not survive on disk to be misread (or to leak its
    /// validators to the new URL's server) on a later run either. Only
    /// safe to rely on here (as opposed to [`read_for_url`]) because this
    /// function is only ever called under `perform_refresh`'s exclusive
    /// lock.
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
        // Confirmed independently of `evict_if_mismatched`'s own return
        // value: even a direct, unfiltered `read` now finds nothing.
        assert!(read(&path).is_none());
    }

    #[test]
    fn evict_if_mismatched_of_a_missing_file_is_no_cache_and_does_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.msgpack");
        assert!(evict_if_mismatched(&path, "https://example.invalid/mapping.json").is_none());
    }
}
