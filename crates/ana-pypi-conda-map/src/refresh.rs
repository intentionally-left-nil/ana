//! The four-case state machine from `investigations/pypi_conda_map.md`:
//! decide whether to use the cache as-is, refresh it in the background, or
//! block for a fresh download -- then perform that refresh via the single
//! state-mutating primitive [`perform_refresh`], which is the only code in
//! this crate that talks to the network or writes the cache file, and
//! which serializes against other processes doing the same via a
//! dedicated lock file (see [`perform_refresh`]'s own doc comment).

use std::io;
use std::path::Path;
use std::time::Duration;

use ana_fs_util::AdvisoryLock;

use crate::envelope::{self, CacheEnvelope};
use crate::error::FetchError;
use crate::fetch::{fetch_full, FetchedMapping};
use crate::http::{HeadResponse, HttpClient};

/// Up to this many consecutive network-level failures are retried on every
/// eligible invocation with no extra delay. Beyond that, a cooldown kicks
/// in before the budget resets and another burst of free retries begins.
/// Deliberately a simple attempt-budget rather than exponential backoff:
/// this only ever runs once per `ana` invocation, so there's no long-lived
/// process to schedule graduated retries within -- see
/// investigations/pypi_conda_map.md, "Backoff: a simple attempt budget."
pub(crate) const BACKOFF_BUDGET: u32 = 10;
pub(crate) const BACKOFF_COOLDOWN: Duration = Duration::from_secs(60 * 60);

pub(crate) const FRESH_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const STALE_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Age < 24h: use the cache as-is, no network call at all.
    UseCached,
    /// 24h <= age < 1 week, or (age >= 1 week and `--allow-stale-mapping`):
    /// use the cache now, refresh it on a background thread subject to the
    /// backoff cooldown -- the caller must join that thread before exit for
    /// the refresh to ever reach disk.
    UseCachedAndRefreshInBackground,
    /// No cache, never successfully confirmed, `force_refresh`, or age >= 1
    /// week without the flag: block for a fresh download right now.
    BlockingRefresh,
}

/// Pure decision function, no I/O -- see the state machine table in
/// `investigations/pypi_conda_map.md`.
pub(crate) fn decide(
    envelope: Option<&CacheEnvelope>,
    now: u64,
    allow_stale_mapping: bool,
    force_refresh: bool,
) -> Action {
    if force_refresh {
        return Action::BlockingRefresh;
    }

    let Some(env) = envelope.filter(|e| e.last_checked_at.is_some()) else {
        return Action::BlockingRefresh;
    };

    let last_checked_at = env.last_checked_at.unwrap_or(0);
    let age = now.saturating_sub(last_checked_at);

    if age < FRESH_WINDOW.as_secs() {
        return Action::UseCached;
    }

    if age < STALE_WINDOW.as_secs() || allow_stale_mapping {
        if backed_off(env, now) {
            return Action::UseCached;
        }
        return Action::UseCachedAndRefreshInBackground;
    }

    Action::BlockingRefresh
}

fn backed_off(env: &CacheEnvelope, now: u64) -> bool {
    if env.consecutive_failures < BACKOFF_BUDGET {
        return false;
    }
    let last_attempt = env.last_attempt_at.unwrap_or(0);
    now.saturating_sub(last_attempt) < BACKOFF_COOLDOWN.as_secs()
}

/// What a successful [`perform_refresh`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshSuccess {
    /// A check (HEAD, or the conditional-GET fallback) confirmed the
    /// cached data is still current. `mapping` is unchanged.
    ConfirmedFresh,
    /// New data was downloaded and persisted.
    Updated,
}

/// Why a [`perform_refresh`] call failed. The `Check`/`Download`-prefixed
/// variants both carry the underlying [`FetchError`] so blocking callers
/// can propagate a real [`crate::error::MappingError`] instead of a bare
/// "it didn't work."
#[derive(Debug)]
#[allow(clippy::enum_variant_names)] // "Failed" is the meaningful, intentional common suffix for an error enum's variants, not accidental repetition
pub(crate) enum RefreshFailure {
    /// Couldn't even acquire the cross-process lock (couldn't open/create
    /// the lock file -- e.g. an unwritable cache directory). No network
    /// call was attempted and nothing was written.
    LockFailed(io::Error),
    CheckFailed(FetchError),
    DownloadFailed(FetchError),
}

/// Public (crate-visible outside this module via `pub` re-export at the
/// crate root) summary of what a refresh attempt did, for
/// `MappingHandle::finish`'s telemetry. Deliberately drops the error detail
/// from [`RefreshFailure`] -- this is for a caller deciding whether to log
/// a warning, not for programmatic error handling (that only exists on the
/// blocking path, via [`crate::error::MappingError`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// Nothing was spawned (cache was fresh, or backed off).
    NotNeeded,
    ConfirmedFresh,
    Updated,
    LockFailed,
    CheckFailed,
    DownloadFailed,
}

pub(crate) fn summarize(
    result: &Result<(CacheEnvelope, RefreshSuccess), RefreshFailure>,
) -> RefreshOutcome {
    match result {
        Ok((_, RefreshSuccess::ConfirmedFresh)) => RefreshOutcome::ConfirmedFresh,
        Ok((_, RefreshSuccess::Updated)) => RefreshOutcome::Updated,
        Err(RefreshFailure::LockFailed(_)) => RefreshOutcome::LockFailed,
        Err(RefreshFailure::CheckFailed(_)) => RefreshOutcome::CheckFailed,
        Err(RefreshFailure::DownloadFailed(_)) => RefreshOutcome::DownloadFailed,
    }
}

enum CheckOutcome {
    UpToDate,
    /// The conditional-GET fallback (used when HEAD isn't supported) came
    /// back with a full `200` body already in hand -- no separate download
    /// step needed.
    StaleWithData(FetchedMapping),
    /// A real HEAD confirmed the server has newer content, but HEAD has no
    /// body -- the caller still needs to download it.
    StaleNeedsDownload,
}

fn check_for_update(
    client: &dyn HttpClient,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<CheckOutcome, FetchError> {
    match client.head(url, etag, last_modified)? {
        HeadResponse::NotModified => Ok(CheckOutcome::UpToDate),
        HeadResponse::Changed => Ok(CheckOutcome::StaleNeedsDownload),
        HeadResponse::Unsupported => match fetch_full(client, url, etag, last_modified)? {
            Some(fetched) => Ok(CheckOutcome::StaleWithData(fetched)),
            None => Ok(CheckOutcome::UpToDate),
        },
    }
}

fn envelope_from_fetch(fetched: FetchedMapping, now: u64) -> CacheEnvelope {
    CacheEnvelope {
        schema_version: envelope::SCHEMA_VERSION,
        etag: fetched.etag,
        last_modified: fetched.last_modified,
        fetched_at: Some(now),
        last_checked_at: Some(now),
        known_stale: false,
        consecutive_failures: 0,
        last_attempt_at: Some(now),
        mapping: fetched.mapping,
    }
}

/// Best-effort persistence of `envelope`: see `perform_refresh`'s doc
/// comment for why a write failure here (disk full, permissions) is
/// swallowed rather than escalated. Factored out so all of
/// `perform_refresh`'s branches share one persistence call instead of
/// repeating (and risking drift in) the same `let _ = write_atomic(...)`
/// line seven times over.
fn persist(cache_path: &Path, envelope: &CacheEnvelope) {
    let _ = envelope::write_atomic(cache_path, envelope);
}

/// Persists `envelope` and packages it as a successful `perform_refresh`
/// return value in one step.
fn persist_ok(
    cache_path: &Path,
    envelope: CacheEnvelope,
    success: RefreshSuccess,
) -> Result<(CacheEnvelope, RefreshSuccess), RefreshFailure> {
    persist(cache_path, &envelope);
    Ok((envelope, success))
}

/// Persists `envelope` and packages the given failure as a
/// `perform_refresh` return value in one step.
fn persist_err(
    cache_path: &Path,
    envelope: &CacheEnvelope,
    failure: RefreshFailure,
) -> Result<(CacheEnvelope, RefreshSuccess), RefreshFailure> {
    persist(cache_path, envelope);
    Err(failure)
}

/// The single function that talks to the network and writes the cache
/// file, called either inline (blocking cases) or from a spawned thread
/// (the background case) -- see `investigations/pypi_conda_map.md`, "One
/// state-mutating primitive, two call sites."
///
/// The entire read-check-network-write sequence below runs while holding
/// an exclusive advisory lock on `lock_path` (a dedicated file, never the
/// cache file itself -- see [`crate::cache_dir::lock_file_path`]'s doc
/// comment for why). This makes the whole thing a real critical section
/// across processes, not just within one: `current` is (re-)read from
/// disk *after* acquiring the lock, deliberately ignoring whatever the
/// caller may have read earlier and outside the lock, so this function
/// always builds its decision and its eventual write on the actual
/// latest on-disk state -- never on a snapshot that could already be
/// stale because another process's `perform_refresh` ran (and wrote)
/// while this one was waiting for the lock or doing its own network I/O.
/// Without this, two concurrent `ana` invocations could each read the
/// same starting envelope, race their independent network calls, and
/// have whichever finishes last silently overwrite the other's result
/// (a fresh fetch discarded, or a success reverted back to a failure
/// count) purely because it started from older data.
///
/// Every branch persists the envelope it returns/fails with before
/// returning, via the [`persist`]/[`persist_ok`]/[`persist_err`] helpers
/// (themselves backed by [`envelope::write_atomic`]) -- including the
/// intermediate `known_stale = true` write, which happens *before*
/// attempting the download so a run killed in between resumes correctly
/// on the next attempt instead of redundantly re-checking. A persist
/// failure (disk full, permissions) is swallowed here rather than
/// escalated: this crate's contract is "never block or fail the caller
/// over cache-writing trouble," at the cost of that one refresh's result
/// not surviving to the next invocation. `write_atomic`'s tempfile+rename
/// stays even under this function's exclusive lock: the lock only
/// serializes this function against other processes' writers, it doesn't
/// protect against `load()`'s deliberately-unlocked fast-path reads
/// observing a write in progress, and it doesn't survive a crash
/// mid-write -- only an atomic rename guarantees the previous complete
/// version is what's left behind if this process dies partway through.
pub(crate) fn perform_refresh(
    client: &dyn HttpClient,
    url: &str,
    cache_path: &Path,
    lock_path: &Path,
) -> Result<(CacheEnvelope, RefreshSuccess), RefreshFailure> {
    let mut lock = AdvisoryLock::open(lock_path).map_err(RefreshFailure::LockFailed)?;
    // Blocks until any other process's `perform_refresh` for this same
    // cache releases the lock (i.e. finishes its own network I/O and
    // write) -- bounded in practice by that other call's own HTTP
    // timeouts, not unbounded.
    let _guard = lock.write().map_err(RefreshFailure::LockFailed)?;

    let now = envelope::now_unix();

    // Read the authoritative current state now, under the lock -- not
    // whatever `current` the caller may have read before acquiring it.
    let current = envelope::read(cache_path);

    // `decide()` only lets a call reach here with `consecutive_failures >=
    // BACKOFF_BUDGET` once `backed_off()` has already judged the cooldown
    // elapsed (see `decide`'s `UseCachedAndRefreshInBackground` branch).
    // Reset the counter *now*, at the start of this fresh attempt, so the
    // budget genuinely resets and another burst of up-to-`BACKOFF_BUDGET`
    // free retries begins, matching `BACKOFF_BUDGET`'s doc comment --
    // instead of the counter staying pinned at-or-above the budget forever
    // and collapsing every future attempt back down to one retry per
    // cooldown regardless of how this one turns out.
    let current = current.map(|env| {
        if env.consecutive_failures >= BACKOFF_BUDGET {
            CacheEnvelope {
                consecutive_failures: 0,
                ..env
            }
        } else {
            env
        }
    });

    // Skip the check entirely if there's no prior envelope to compare
    // against, or if we already know (from a previous, possibly
    // interrupted, run) that the data is stale.
    let after_check = match current {
        Some(env) if !env.known_stale => {
            match check_for_update(
                client,
                url,
                env.etag.as_deref(),
                env.last_modified.as_deref(),
            ) {
                Ok(CheckOutcome::UpToDate) => {
                    let mut confirmed = env;
                    confirmed.last_checked_at = Some(now);
                    confirmed.last_attempt_at = Some(now);
                    confirmed.consecutive_failures = 0;
                    return persist_ok(cache_path, confirmed, RefreshSuccess::ConfirmedFresh);
                }
                Ok(CheckOutcome::StaleWithData(fetched)) => {
                    let updated = envelope_from_fetch(fetched, now);
                    return persist_ok(cache_path, updated, RefreshSuccess::Updated);
                }
                Ok(CheckOutcome::StaleNeedsDownload) => {
                    let mut marked = env;
                    marked.known_stale = true;
                    marked.last_attempt_at = Some(now);
                    marked.consecutive_failures = 0;
                    persist(cache_path, &marked);
                    Some(marked)
                }
                Err(err) => {
                    let mut failed = env;
                    failed.consecutive_failures += 1;
                    failed.last_attempt_at = Some(now);
                    return persist_err(cache_path, &failed, RefreshFailure::CheckFailed(err));
                }
            }
        }
        other => other,
    };

    let etag = after_check.as_ref().and_then(|e| e.etag.as_deref());
    let last_modified = after_check
        .as_ref()
        .and_then(|e| e.last_modified.as_deref());

    match fetch_full(client, url, etag, last_modified) {
        Ok(Some(fetched)) => {
            let updated = envelope_from_fetch(fetched, now);
            persist_ok(cache_path, updated, RefreshSuccess::Updated)
        }
        Ok(None) => {
            // A 304 here is only reachable via a `known_stale` envelope
            // whose validators turned out to still match after all (e.g. an
            // upstream rollback between the check and this download).
            // Treat it the same as a confirmed-fresh check.
            let mut confirmed = after_check.unwrap_or_else(CacheEnvelope::new_empty);
            confirmed.known_stale = false;
            confirmed.last_checked_at = Some(now);
            confirmed.last_attempt_at = Some(now);
            confirmed.consecutive_failures = 0;
            persist_ok(cache_path, confirmed, RefreshSuccess::ConfirmedFresh)
        }
        Err(err) => {
            let mut failed = after_check.unwrap_or_else(CacheEnvelope::new_empty);
            failed.consecutive_failures += 1;
            failed.last_attempt_at = Some(now);
            persist_err(cache_path, &failed, RefreshFailure::DownloadFailed(err))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::sync::Mutex;

    use crate::http::{GetResponse, HttpError};

    use super::*;

    fn envelope_with_age(now: u64, age_secs: u64) -> CacheEnvelope {
        let mut env = CacheEnvelope::new_empty();
        env.last_checked_at = Some(now.saturating_sub(age_secs));
        env
    }

    #[test]
    fn no_cache_blocks() {
        assert_eq!(decide(None, 1_000, false, false), Action::BlockingRefresh);
    }

    #[test]
    fn never_confirmed_cache_is_treated_as_no_cache() {
        let mut env = CacheEnvelope::new_empty();
        env.last_checked_at = None;
        env.consecutive_failures = 3;
        assert_eq!(
            decide(Some(&env), 1_000, false, false),
            Action::BlockingRefresh
        );
    }

    #[test]
    fn fresh_cache_does_nothing() {
        let env = envelope_with_age(100_000, 60);
        assert_eq!(decide(Some(&env), 100_000, false, false), Action::UseCached);
    }

    #[test]
    fn moderately_stale_cache_refreshes_in_background() {
        let env = envelope_with_age(1_000_000, FRESH_WINDOW.as_secs() + 1);
        assert_eq!(
            decide(Some(&env), 1_000_000, false, false),
            Action::UseCachedAndRefreshInBackground
        );
    }

    #[test]
    fn week_stale_cache_blocks_without_the_flag() {
        let env = envelope_with_age(10_000_000, STALE_WINDOW.as_secs() + 1);
        assert_eq!(
            decide(Some(&env), 10_000_000, false, false),
            Action::BlockingRefresh
        );
    }

    #[test]
    fn week_stale_cache_refreshes_in_background_with_the_flag() {
        let env = envelope_with_age(10_000_000, STALE_WINDOW.as_secs() + 1);
        assert_eq!(
            decide(Some(&env), 10_000_000, true, false),
            Action::UseCachedAndRefreshInBackground
        );
    }

    #[test]
    fn force_refresh_always_blocks_even_when_fresh() {
        let env = envelope_with_age(100_000, 60);
        assert_eq!(
            decide(Some(&env), 100_000, false, true),
            Action::BlockingRefresh
        );
    }

    #[test]
    fn backoff_suppresses_background_refresh_until_cooldown_elapses() {
        let now = 1_000_000;
        let mut env = envelope_with_age(now, FRESH_WINDOW.as_secs() + 1);
        env.consecutive_failures = BACKOFF_BUDGET;
        env.last_attempt_at = Some(now - 10); // just tried, well within cooldown

        assert_eq!(decide(Some(&env), now, false, false), Action::UseCached);

        env.last_attempt_at = Some(now - BACKOFF_COOLDOWN.as_secs() - 1); // cooldown elapsed
        assert_eq!(
            decide(Some(&env), now, false, false),
            Action::UseCachedAndRefreshInBackground
        );
    }

    #[test]
    fn under_budget_failures_never_backoff() {
        let now = 1_000_000;
        let mut env = envelope_with_age(now, FRESH_WINDOW.as_secs() + 1);
        env.consecutive_failures = BACKOFF_BUDGET - 1;
        env.last_attempt_at = Some(now); // just attempted, but under budget

        assert_eq!(
            decide(Some(&env), now, false, false),
            Action::UseCachedAndRefreshInBackground
        );
    }

    /// Canned in-memory [`HttpClient`] for exercising `perform_refresh`
    /// without any real network I/O.
    struct FakeHttpClient {
        head_responses: Mutex<Vec<Result<HeadResponse, HttpError>>>,
        get_responses: Mutex<Vec<Result<GetResponse, HttpError>>>,
    }

    impl FakeHttpClient {
        fn new() -> Self {
            Self {
                head_responses: Mutex::new(Vec::new()),
                get_responses: Mutex::new(Vec::new()),
            }
        }

        fn then_head(self, response: Result<HeadResponse, HttpError>) -> Self {
            self.head_responses.lock().unwrap().push(response);
            self
        }

        fn then_get(self, response: Result<GetResponse, HttpError>) -> Self {
            self.get_responses.lock().unwrap().push(response);
            self
        }
    }

    impl HttpClient for FakeHttpClient {
        fn head(
            &self,
            _url: &str,
            _etag: Option<&str>,
            _last_modified: Option<&str>,
        ) -> Result<HeadResponse, HttpError> {
            self.head_responses.lock().unwrap().remove(0)
        }

        fn get(
            &self,
            _url: &str,
            _etag: Option<&str>,
            _last_modified: Option<&str>,
        ) -> Result<GetResponse, HttpError> {
            self.get_responses.lock().unwrap().remove(0)
        }
    }

    fn sample_body() -> Vec<u8> {
        serde_json::to_vec(&HashMap::from([(
            "opencv-python".to_string(),
            "py-opencv".to_string(),
        )]))
        .unwrap()
    }

    #[test]
    fn no_prior_cache_downloads_directly_without_a_head_check() {
        let client = FakeHttpClient::new().then_get(Ok(GetResponse::Ok {
            body: sample_body(),
            etag: Some("v1".to_string()),
            last_modified: None,
        }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");

        let (env, success) =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap();

        assert_eq!(success, RefreshSuccess::Updated);
        assert_eq!(
            env.mapping.get("opencv-python"),
            Some(&"py-opencv".to_string())
        );
        assert!(!env.known_stale);
        assert_eq!(env.consecutive_failures, 0);
    }

    #[test]
    fn head_confirms_fresh_resets_the_clock_without_downloading() {
        let mut prior = CacheEnvelope::new_empty();
        prior.etag = Some("v1".to_string());
        prior.last_checked_at = Some(1);
        prior.mapping.insert("a".to_string(), "b".to_string());

        let client = FakeHttpClient::new().then_head(Ok(HeadResponse::NotModified));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let (env, success) =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap();

        assert_eq!(success, RefreshSuccess::ConfirmedFresh);
        assert_eq!(env.mapping, prior.mapping); // untouched
        assert!(env.last_checked_at.unwrap() > prior.last_checked_at.unwrap());
    }

    #[test]
    fn head_confirms_stale_marks_known_stale_before_downloading() {
        let mut prior = CacheEnvelope::new_empty();
        prior.etag = Some("v1".to_string());
        prior.last_checked_at = Some(1);

        let client = FakeHttpClient::new()
            .then_head(Ok(HeadResponse::Changed))
            .then_get(Ok(GetResponse::Ok {
                body: sample_body(),
                etag: Some("v2".to_string()),
                last_modified: None,
            }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let (env, success) =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap();

        assert_eq!(success, RefreshSuccess::Updated);
        assert_eq!(env.etag, Some("v2".to_string()));
        assert!(!env.known_stale);
    }

    #[test]
    fn known_stale_envelope_skips_the_head_check() {
        let mut prior = CacheEnvelope::new_empty();
        prior.known_stale = true;
        prior.etag = Some("v1".to_string());

        // Only a GET response queued -- if `perform_refresh` tried a HEAD
        // first, the fake client's empty head_responses queue would panic.
        let client = FakeHttpClient::new().then_get(Ok(GetResponse::Ok {
            body: sample_body(),
            etag: Some("v2".to_string()),
            last_modified: None,
        }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let (_, success) =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap();

        assert_eq!(success, RefreshSuccess::Updated);
    }

    #[test]
    fn head_unsupported_falls_back_to_conditional_get_and_uses_its_body_directly() {
        let mut prior = CacheEnvelope::new_empty();
        prior.etag = Some("v1".to_string());

        let client = FakeHttpClient::new()
            .then_head(Ok(HeadResponse::Unsupported))
            .then_get(Ok(GetResponse::Ok {
                body: sample_body(),
                etag: Some("v2".to_string()),
                last_modified: None,
            }));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let (env, success) =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap();

        assert_eq!(success, RefreshSuccess::Updated);
        assert_eq!(env.etag, Some("v2".to_string()));
    }

    #[test]
    fn network_failure_during_check_bumps_failure_counters_and_preserves_mapping() {
        let mut prior = CacheEnvelope::new_empty();
        prior.etag = Some("v1".to_string());
        prior.mapping.insert("a".to_string(), "b".to_string());
        prior.consecutive_failures = 2;

        let client = FakeHttpClient::new().then_head(Err(HttpError::UnexpectedStatus(500)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let err =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap_err();

        assert!(matches!(err, RefreshFailure::CheckFailed(_)));
        let persisted = envelope::read(&path).unwrap();
        assert_eq!(persisted.consecutive_failures, 3);
        assert_eq!(persisted.mapping.get("a"), Some(&"b".to_string()));
    }

    #[test]
    fn post_cooldown_attempt_resets_the_budget_for_a_fresh_burst() {
        // A prior envelope that's already over budget -- this only ever
        // reaches `perform_refresh` once `decide()`'s `backed_off()` check
        // has judged the cooldown elapsed, so this call represents the
        // start of a new burst, not a continuation of the exhausted one.
        let mut prior = CacheEnvelope::new_empty();
        prior.etag = Some("v1".to_string());
        prior.consecutive_failures = BACKOFF_BUDGET + 5;

        // The fresh attempt itself fails again.
        let client = FakeHttpClient::new().then_head(Err(HttpError::UnexpectedStatus(500)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let err =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap_err();

        assert!(matches!(err, RefreshFailure::CheckFailed(_)));
        let persisted = envelope::read(&path).unwrap();
        // Budget reset to 0 before this attempt, then bumped by exactly
        // this one failure -- not left at (or above) `BACKOFF_BUDGET`,
        // which would immediately re-trigger `backed_off()` and collapse
        // every subsequent invocation back to one retry per cooldown
        // forever instead of a genuine fresh burst.
        assert_eq!(persisted.consecutive_failures, 1);
    }

    #[test]
    fn network_failure_during_download_preserves_known_stale_for_next_attempt() {
        let mut prior = CacheEnvelope::new_empty();
        prior.known_stale = true;
        prior.etag = Some("v1".to_string());

        let client = FakeHttpClient::new().then_get(Err(HttpError::UnexpectedStatus(500)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");
        envelope::write_atomic(&path, &prior).unwrap();

        let err =
            perform_refresh(&client, "http://example.invalid", &path, &lock_path).unwrap_err();

        assert!(matches!(err, RefreshFailure::DownloadFailed(_)));
        let persisted = envelope::read(&path).unwrap();
        assert!(persisted.known_stale);
        assert_eq!(persisted.consecutive_failures, 1);
    }

    #[test]
    fn concurrent_refreshes_serialize_instead_of_losing_an_update() {
        // Two threads race `perform_refresh` against the same cache_path
        // and lock_path, simulating two concurrent `ana` processes.
        // Without the lock (and the fresh re-read under it), a naive
        // read-then-write could let the slower thread's stale-relative
        // write clobber the faster thread's successful one. With the
        // lock, they serialize: the second to acquire it always re-reads
        // the first's already-persisted result before deciding what to
        // do, so nothing is silently lost.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pypi_mapping.msgpack");
        let lock_path = dir.path().join("pypi_mapping.lock");

        let mut prior = CacheEnvelope::new_empty();
        prior.etag = Some("v1".to_string());
        envelope::write_atomic(&path, &prior).unwrap();

        // Thread A: HEAD confirms fresh.
        let client_a = FakeHttpClient::new().then_head(Ok(HeadResponse::NotModified));
        // Thread B: HEAD says changed, GET returns new data.
        let client_b = FakeHttpClient::new()
            .then_head(Ok(HeadResponse::Changed))
            .then_get(Ok(GetResponse::Ok {
                body: sample_body(),
                etag: Some("v2".to_string()),
                last_modified: None,
            }));

        let path_a = path.clone();
        let lock_path_a = lock_path.clone();
        let handle_a = std::thread::spawn(move || {
            perform_refresh(&client_a, "http://example.invalid", &path_a, &lock_path_a)
        });
        let handle_b = std::thread::spawn(move || {
            perform_refresh(&client_b, "http://example.invalid", &path, &lock_path)
        });

        let result_a = handle_a.join().unwrap();
        let result_b = handle_b.join().unwrap();

        // The lock serializes the two attempts; it doesn't fail either one.
        assert!(result_a.is_ok());
        assert!(result_b.is_ok());
    }
}
