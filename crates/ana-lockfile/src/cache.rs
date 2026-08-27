//! The stage-1 staleness cache: `pyproject_hash.json`, inside `env_path`.
//!
//! Per `investigations/lock_generation_algorithm.md`'s "Decision: the
//! stage-1 hash lives in a separate, local, gitignored cache file": this
//! file is never committed (it lives inside a directory `env_storage.md`
//! already gitignores), is scoped to exactly one platform by the directory
//! it lives in (no `platforms.<subdir>` nesting here -- a foreign-platform
//! `env_path` gets rebuilt from scratch, cache included), and holds exactly
//! two hashes. Losing it, deleting it, or having it go stale relative to
//! `ana.lock` is always safe: any doubt is a stage-1 miss and a fall
//! through to stage 2 against `ana.lock`'s real content, never an
//! incorrect "valid" verdict.
//!
//! Writes are best-effort and always overwrite the whole file (it's a
//! single scalar record -- there is nothing else in it to preserve, so no
//! read-modify-write), with the same tempfile-then-rename atomicity as
//! every other file this crate writes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fs_util::write_atomic;

/// The cache file's name within `env_path`.
const CACHE_FILE_NAME: &str = "pyproject_hash.json";

/// The two stage-1 hashes. Both must match for a stage-1 hit:
///
/// - `pyproject_hash`: SHA-256 of the whole `pyproject.toml` file.
///   Deliberately whole-file, not a dependency-subset hash: any edit
///   causes a miss on the next check, and a miss only costs a stage-2
///   recheck plus this tiny write -- never a committed-file change.
/// - `ana_lock_hash`: SHA-256 of the current platform's *parsed* section
///   of `ana.lock` ([`crate::lock_file::PlatformSection::hash`]), so
///   "`pyproject.toml` unchanged but the lock moved" (branch switch, `git
///   pull`, a teammate's re-resolve) is a miss that falls through to stage
///   2 instead of wrongly trusting the cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CacheFile {
    pub pyproject_hash: String,
    pub ana_lock_hash: String,
}

/// The cache file's path within `env_path`.
pub(crate) fn cache_path(env_path: &Path) -> PathBuf {
    env_path.join(CACHE_FILE_NAME)
}

/// Read the cache file. Missing, unreadable, or corrupt all return `None`
/// -- a stage-1 miss, never an error.
pub(crate) fn read(env_path: &Path) -> Option<CacheFile> {
    let bytes = fs::read(cache_path(env_path)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Overwrite the cache file, creating `env_path` if needed. Best-effort:
/// a failure (disk full, permissions) is swallowed, since a lost cache
/// write only ever costs a stage-1 miss on the next invocation -- the same
/// "never block or fail the caller over cache-writing trouble" contract as
/// `ana-pypi-conda-map`'s cache persistence.
pub(crate) fn write(env_path: &Path, cache: &CacheFile) {
    let bytes = match serde_json::to_vec_pretty(cache) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let _ = write_atomic(&cache_path(env_path), &bytes);
}

/// Delete the cache file if it exists. Called the moment stage 1 misses,
/// before any stage-2/regenerate work: if the process then crashes
/// mid-solve, no stale cache survives to claim validity next time.
pub(crate) fn delete(env_path: &Path) {
    match fs::remove_file(cache_path(env_path)) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cache() -> CacheFile {
        CacheFile {
            pyproject_hash: "aa".to_string(),
            ana_lock_hash: "bb".to_string(),
        }
    }

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("env");
        write(&env, &cache());
        assert_eq!(read(&env), Some(cache()));
    }

    #[test]
    fn missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn corrupt_is_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(CACHE_FILE_NAME), b"not json").unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn partial_content_is_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(CACHE_FILE_NAME),
            br#"{"pyproject_hash": "aa"}"#,
        )
        .unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn delete_removes_and_tolerates_absence() {
        let dir = tempfile::tempdir().unwrap();
        delete(dir.path()); // no file: not an error
        write(dir.path(), &cache());
        delete(dir.path());
        assert_eq!(read(dir.path()), None);
    }
}
