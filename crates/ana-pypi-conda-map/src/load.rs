//! Public entry point: [`load`] implements a four-case state machine,
//! returning a [`MappingHandle`] that's immediately usable. In the
//! background-refresh case, call [`MappingHandle::finish`] to wait for and
//! observe that refresh's outcome -- dropping the handle without calling
//! it never blocks, it just abandons the in-flight refresh (see `finish`'s
//! doc comment).
//!
//! `load` takes a `tokio::runtime::Handle` and a `rattler_networking::LazyClient`
//! from its caller rather than building either itself: `main.rs` builds
//! one runtime and one client for the whole process (shared with
//! `ana-installer`'s downloads and `ana-solver`'s repodata fetches), not a
//! private stack for every consumer that needs HTTP.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::thread::JoinHandle;

use rattler_networking::LazyClient;
use uv_normalize::PackageName;

use crate::cache_dir;
use crate::envelope;
use crate::error::{InvalidMappedName, MappingError};
use crate::http::{HttpClient, ReqwestHttpClient};
use crate::refresh::{self, Action, RefreshOutcome};

#[derive(Debug, Clone, Copy, Default)]
pub struct LoadOptions {
    /// Demotes a cache older than a week from "block for a fresh download"
    /// down to "use the stale data now, refresh in the background" -- the
    /// same treatment a 24h-1-week-old cache already gets.
    pub allow_stale_mapping: bool,
    /// Bypasses age and backoff entirely: always block for a fresh
    /// download right now.
    pub force_refresh: bool,
}

/// The loaded mapping, plus (in the background-refresh case) a handle to
/// the in-flight refresh.
#[derive(Debug)]
pub struct MappingHandle {
    map: HashMap<String, String>,
    pending: Option<JoinHandle<RefreshOutcome>>,
}

impl MappingHandle {
    /// Lightweight constructor for a caller that already has a resolved
    /// `pypi_name -> conda_name` table in hand (test fixtures) --
    /// bypasses [`load`]'s runtime/network/cache-directory machinery
    /// entirely. `pending` is always `None`: there is no in-flight
    /// refresh to join, since none was ever started.
    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self { map, pending: None }
    }

    /// `pypi_name` mapped through this table, or `pypi_name` itself
    /// unchanged if the table has no entry for it -- the identity
    /// mapping, correct for every name the table doesn't mention (see the
    /// crate's module docs). `Err` only when an entry *does* exist for
    /// `pypi_name` but its value fails the same PEP 503/CEP-26 shape
    /// check `fetch.rs::normalize_and_filter` already ran at fetch time
    /// (see [`InvalidMappedName`]'s docs for why that check can still
    /// fail here). Validates only the one entry actually being looked
    /// up, not the table as a whole: the vast majority of entries in a
    /// real table are never looked up in a given run (most PyPI names
    /// aren't among a project's dependencies), so checking every entry
    /// up front would be pure waste.
    pub fn get<'a>(&'a self, pypi_name: &'a str) -> Result<&'a str, InvalidMappedName> {
        let Some(conda_name) = self.map.get(pypi_name) else {
            return Ok(pypi_name);
        };
        if PackageName::from_str(conda_name).is_err() {
            return Err(InvalidMappedName {
                pypi_name: pypi_name.to_string(),
                conda_name: conda_name.clone(),
            });
        }
        Ok(conda_name.as_str())
    }

    /// Joins any in-flight background refresh and returns what happened --
    /// `RefreshOutcome::NotNeeded` if nothing was spawned. This is the
    /// only way to observe a background refresh's outcome: `MappingHandle`
    /// has no blocking `Drop` impl, matching `std::thread::JoinHandle`'s
    /// own convention of detaching rather than joining on drop. Dropping a
    /// handle without calling this never blocks -- the spawned thread
    /// keeps running independently (writing the cache via its own atomic
    /// rename if it finishes) or gets killed with the process; both are
    /// safe since `perform_refresh` only ever replaces the cache file
    /// wholesale, so an interrupted refresh just leaves the previous,
    /// complete version in place. Skipping `finish()` only costs the
    /// refresh's outcome (e.g. a warning after repeated failures) and its
    /// result reaching disk this run.
    pub fn finish(mut self) -> RefreshOutcome {
        match self.pending.take() {
            Some(handle) => handle.join().unwrap_or(RefreshOutcome::CheckFailed),
            None => RefreshOutcome::NotNeeded,
        }
    }
}

/// Synchronous entry point. `Err` when there's nothing usable to hand
/// back: `url` is empty (`pypi_to_conda_uri` is effectively unset --
/// [`MappingError::UrlNotConfigured`]), or a blocking path (no cache at
/// all, or a cache stale beyond a week without `allow_stale_mapping`, or
/// `force_refresh`) fails to reach the network. Every other path always
/// returns `Ok` with the best data available -- there is deliberately no
/// silent fallback to an empty (identity-mapping) map: a caller that gets
/// `Ok` back can rely on the mapping actually being current or
/// deliberately-stale-and-usable, never "we gave up and pretended every
/// name maps to itself."
///
/// `runtime` and `client` are supplied by the caller: `load` stays a
/// synchronous entry point (bridging via `Handle::block_on`, same pattern
/// `ana-solver` uses) even though its two blocking paths now drive real
/// async HTTP calls underneath.
///
/// `url` is the pypi-to-conda mapping endpoint to fetch from -- the
/// caller's resolved `ana_config::AnaConfig::pypi_to_conda_uri` (falling
/// back to `ana_config::DEFAULT_PYPI_TO_CONDA_URI` when unset; see
/// `ana::config::resolve_config`), not something this crate defaults or
/// overrides itself. If `url` ever changes between invocations, any
/// cache left over from a previous URL is treated as absent by this
/// function's own outer, unlocked read (`envelope::read_for_url`, which
/// never deletes) and is only actually discarded -- deleted from disk --
/// once a refresh reaches `perform_refresh`'s locked
/// `envelope::evict_if_mismatched` call.
///
/// `on_blocking_refresh` is called exactly once, synchronously, right
/// before the network call, if and only if `Action::BlockingRefresh` is
/// chosen -- the one case where this function makes the caller wait on
/// real network I/O with nothing usable to show in the meantime. This
/// crate does no printing/logging of its own; the callback is how a
/// caller (the CLI) surfaces that wait to the user without this crate
/// taking a hard dependency on any particular output mechanism. Never
/// called for `UseCached`/`UseCachedAndRefreshInBackground`, since
/// neither blocks the caller on the network.
pub fn load(
    runtime: &tokio::runtime::Handle,
    client: &LazyClient,
    url: &str,
    options: LoadOptions,
    on_blocking_refresh: impl FnOnce(),
) -> Result<MappingHandle, MappingError> {
    if url.trim().is_empty() {
        return Err(MappingError::UrlNotConfigured);
    }
    // Owned from here on: the background-refresh path below moves this
    // into a `'static` thread closure, which a borrowed `&str` tied to
    // the caller's stack frame could never satisfy.
    let url = url.to_string();

    let cache_path = cache_dir::cache_file_path().ok_or(MappingError::CacheDir)?;
    let lock_path = cache_dir::lock_file_path().ok_or(MappingError::CacheDir)?;
    let current = envelope::read_for_url(&cache_path, &url);
    let now = envelope::now_unix();

    let action = refresh::decide(
        current.as_ref(),
        now,
        options.allow_stale_mapping,
        options.force_refresh,
    );

    match action {
        Action::UseCached => Ok(MappingHandle {
            map: current.map(|env| env.mapping).unwrap_or_default(),
            pending: None,
        }),

        Action::UseCachedAndRefreshInBackground => {
            // The map returned to the caller right now comes from this outer,
            // unlocked read -- `perform_refresh` re-reads the authoritative
            // state itself once it acquires the lock, so this snapshot being
            // possibly stale by the time the background thread runs is
            // harmless (it's just what's handed back for immediate use,
            // never written anywhere).
            let map = current.map(|env| env.mapping).unwrap_or_default();
            let client: Arc<dyn HttpClient> = Arc::new(ReqwestHttpClient::new(client.clone()));
            // `Handle` is `Clone + Send + 'static`; the thread itself stays a
            // real OS thread, not a tokio task, so `MappingHandle::finish`'s
            // "join a `JoinHandle`" contract is unchanged.
            let handle = runtime.clone();
            let pending = std::thread::Builder::new()
                .name("ana-pypi-conda-map-refresh".to_string())
                .spawn(move || {
                    let result = handle.block_on(refresh::perform_refresh(
                        client.as_ref(),
                        &url,
                        &cache_path,
                        &lock_path,
                    ));
                    refresh::summarize(&result)
                })
                .ok(); // if the OS can't even spawn a thread, just skip the refresh
            Ok(MappingHandle { map, pending })
        }

        Action::BlockingRefresh => {
            on_blocking_refresh();
            let client = ReqwestHttpClient::new(client.clone());
            let result = runtime.block_on(refresh::perform_refresh(
                &client,
                &url,
                &cache_path,
                &lock_path,
            ));
            match result {
                Ok((env, _)) => Ok(MappingHandle {
                    map: env.mapping,
                    pending: None,
                }),
                Err(refresh::RefreshFailure::LockFailed(err)) => Err(MappingError::Lock(err)),
                Err(refresh::RefreshFailure::CheckFailed(err))
                | Err(refresh::RefreshFailure::DownloadFailed(err)) => {
                    Err(MappingError::Fetch(err))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// An empty (or all-whitespace) `url` is rejected immediately, before
    /// `load` ever touches the cache directory or the network -- this is
    /// the one branch of `load` testable without a real
    /// `tokio::runtime::Handle`/`LazyClient`, since every other path
    /// needs both to even begin.
    #[test]
    fn empty_url_is_rejected_before_any_io() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let client = LazyClient::default();
        for url in ["", "   "] {
            let result = load(
                runtime.handle(),
                &client,
                url,
                LoadOptions::default(),
                || {},
            );
            assert!(matches!(result, Err(MappingError::UrlNotConfigured)));
        }
    }

    /// An absent entry is the identity mapping -- not an error, and not
    /// dependent on the table being non-empty (see the crate's module
    /// docs: the table only ever holds entries that genuinely differ).
    #[test]
    fn get_of_an_absent_name_returns_the_name_unchanged() {
        let handle = MappingHandle::from_map(HashMap::from([(
            "opencv-python".to_string(),
            "py-opencv".to_string(),
        )]));
        assert_eq!(handle.get("requests"), Ok("requests"));
    }

    #[test]
    fn get_of_a_present_valid_entry_returns_the_mapped_name() {
        let handle = MappingHandle::from_map(HashMap::from([(
            "opencv-python".to_string(),
            "py-opencv".to_string(),
        )]));
        assert_eq!(handle.get("opencv-python"), Ok("py-opencv"));
    }

    /// The scenario [`InvalidMappedName`] exists for: a value that never
    /// would have survived `fetch.rs::normalize_and_filter` (a space is
    /// not a valid PEP 503/CEP-26 character) reaching `get` anyway --
    /// e.g. a cache file read back off disk with no re-normalization of
    /// its own. `from_map` makes this constructible directly for a test,
    /// without needing a real corrupted cache file on disk.
    #[test]
    fn get_of_a_present_invalid_entry_is_an_error() {
        let handle = MappingHandle::from_map(HashMap::from([(
            "some-pkg".to_string(),
            "not a valid name".to_string(),
        )]));
        let err = handle.get("some-pkg").unwrap_err();
        assert_eq!(err.pypi_name, "some-pkg");
        assert_eq!(err.conda_name, "not a valid name");
    }
}
