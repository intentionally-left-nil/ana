//! Public entry point: [`load`] implements the four-case state machine
//! documented in `investigations/pypi_conda_map.md`, returning a
//! [`MappingHandle`] that's immediately usable. In the background-refresh
//! case, call [`MappingHandle::finish`] to wait for and observe that
//! refresh's outcome -- dropping the handle without calling it never
//! blocks, it just abandons the in-flight refresh (see `finish`'s doc
//! comment).

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::cache_dir;
use crate::envelope;
use crate::error::MappingError;
use crate::http::{HttpClient, UreqHttpClient};
use crate::refresh::{self, Action, RefreshOutcome};

/// Build-time default, overridable at runtime via `ANA_PYPI_MAPPING_URL`
/// for testing/staging -- see `investigations/pypi_conda_map.md`,
/// "Build-time URL." No `build.rs`: this is a single string constant, not
/// something that needs compile-time machinery.
const DEFAULT_MAPPING_URL: &str = "https://example.invalid/pypi_mapping";

fn mapping_url() -> String {
    std::env::var("ANA_PYPI_MAPPING_URL").unwrap_or_else(|_| DEFAULT_MAPPING_URL.to_string())
}

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
pub struct MappingHandle {
    map: HashMap<String, String>,
    pending: Option<JoinHandle<RefreshOutcome>>,
}

impl MappingHandle {
    pub fn get(&self, pypi_name: &str) -> Option<&str> {
        self.map.get(pypi_name).map(String::as_str)
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.map
    }

    /// Joins any in-flight background refresh and returns what happened --
    /// `RefreshOutcome::NotNeeded` if nothing was spawned. This is the
    /// *only* way to wait for or observe a background refresh's outcome:
    /// `MappingHandle` deliberately has no blocking `Drop` impl, matching
    /// `std::thread::JoinHandle`'s own convention of detaching rather than
    /// joining on drop. A caller that drops a handle without calling this
    /// never blocks -- the spawned thread just keeps running independently
    /// (writing the cache via its own atomic rename if it finishes) or
    /// gets killed with the process, either of which is safe since
    /// `perform_refresh` only ever replaces the cache file wholesale, so
    /// anything interrupted mid-refresh just leaves the previous, complete
    /// version in place. Skipping `finish()` costs nothing but silently
    /// discarding that refresh's outcome (e.g. a warning after repeated
    /// failures) and its result reaching disk this run.
    pub fn finish(mut self) -> RefreshOutcome {
        match self.pending.take() {
            Some(handle) => handle.join().unwrap_or(RefreshOutcome::CheckFailed),
            None => RefreshOutcome::NotNeeded,
        }
    }
}

/// Synchronous entry point. `Err` only from the blocking paths (no cache at
/// all, or a cache stale beyond a week without `allow_stale_mapping`, or
/// `force_refresh`) failing outright with nothing usable to fall back to --
/// every other path always returns `Ok` with the best data available.
pub fn load(options: LoadOptions) -> Result<MappingHandle, MappingError> {
    let cache_path = cache_dir::cache_file_path().ok_or(MappingError::CacheDir)?;
    let lock_path = cache_dir::lock_file_path().ok_or(MappingError::CacheDir)?;
    let current = envelope::read(&cache_path);
    let now = envelope::now_unix();
    let url = mapping_url();

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
            // The map returned to the caller right now comes from this
            // outer, unlocked read -- perform_refresh re-reads the
            // authoritative state itself once it acquires the lock, so
            // this snapshot being possibly a moment stale by the time the
            // background thread runs is harmless (it's just what's handed
            // back for immediate use, never written anywhere).
            let map = current.map(|env| env.mapping).unwrap_or_default();
            let client: Arc<dyn HttpClient> = Arc::new(UreqHttpClient::new());
            let pending = std::thread::Builder::new()
                .name("ana-pypi-conda-map-refresh".to_string())
                .spawn(move || {
                    let result =
                        refresh::perform_refresh(client.as_ref(), &url, &cache_path, &lock_path);
                    refresh::summarize(&result)
                })
                .ok(); // if the OS can't even spawn a thread, just skip the refresh
            Ok(MappingHandle { map, pending })
        }

        Action::BlockingRefresh => {
            let client = UreqHttpClient::new();
            let result = refresh::perform_refresh(&client, &url, &cache_path, &lock_path);
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
