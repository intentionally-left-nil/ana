# PyPI → conda name mapping (`ana-pypi-conda-map`)

Scope: the crate that gives `ana-pep508-to-matchspec`'s deferred `map_name`
call site (see `investigations/pep508_to_matchspec_api.md`, "Deferred: name
mapping") a real lookup table instead of the identity mapping it uses today.
This doc supersedes that section's assumption of a compile-time `phf::Map` --
see "Why not a compile-time table after all" below -- but keeps the same
call-site contract: a synchronous, infallible, zero-surprise lookup that the
matchspec conversion path can call without knowing or caring how the data
got there.

## Why not a compile-time table after all

The original doc assumed the PyPI→conda name diffs could be baked in via
`phf` at build time, on the reasoning that a static table has "zero runtime
construction cost, no `open()`/`close()` lifecycle." That's still true of the
*lookup*, but the table's *contents* are not static in the way a fixed conda
subdir list is: names get added and renamed on the conda side on an ongoing
basis, independent of `ana` releases. Baking it in at compile time means
every affected package is wrong until the next `ana` release ships --
unacceptable for a mapping whose entire job is correctness for exactly the
packages that would otherwise silently resolve to the wrong (usually
nonexistent) conda name. This crate keeps the "zero runtime construction
cost on the hot path" property by loading a pre-fetched table from disk
(see "Hot path stays synchronous and network-free" below) rather than by
fixing the table's contents at compile time.

## Source data and on-disk shape

An internal API serves the full PyPI→conda name table as `GET
/pypi_mapping` → `{"pypi_name": "conda_name", ...}` for every known package,
including the (large) majority where the two names are identical. This
crate:

1. Fetches that JSON.
2. Normalizes both sides through the same PEP 503/CEP-26 normalization
   `uv_normalize`/`rattler_conda_types` already apply elsewhere in `ana`,
   then keeps only the entries where the normalized names actually differ
   -- this is almost always a small fraction of the full table, and it's
   what makes the call site in `ana-pep508-to-matchspec` a plain
   `HashMap::get` with an already-normalized key, no extra normalization
   step needed at lookup time.
3. Encodes the result as MessagePack (`rmp-serde`) and writes it, alongside
   HTTP cache-validator state and retry bookkeeping, to a single file in the
   OS cache directory: `$CACHE/ana/pypi_mapping.msgpack`.

### One file, not a config file plus a data file

An earlier draft of this design kept a separate small config/metadata file
(ETag, Last-Modified, download timestamp) next to the MessagePack data file.
That doesn't hold up under a crash: atomically replacing each file
individually (tempfile + rename, one per file) does not make replacing *both*
atomic as a pair. A crash between the two renames leaves validator headers
describing content that was never actually written, or freshly-written
content whose "when did we last check" bookkeeping still says days ago.
Since `load_mapping` (below) has to deserialize the whole map into memory on
every hot-path call regardless, decoding a small header alongside it costs
nothing extra -- there's no benefit to splitting them that offsets the
lost atomicity. So: one envelope struct, one file, one `tempfile::NamedTempFile::new_in(cache_dir)`
→ `.persist(path)` call replaces the previous version wholesale.

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CacheEnvelope {
    schema_version: u16,

    // Validators from the last successful GET; used to make the periodic
    // "is this still current" check conditional (HEAD, or a conditional GET
    // fallback) instead of a full unconditional re-download every time.
    etag: Option<String>,
    last_modified: Option<String>,

    // Last time the *payload* actually changed (a successful GET returned
    // 200, not 304/HEAD-unchanged). Informational only -- not read by the
    // state machine below, but useful for a future `ana cache info` command.
    fetched_at: Option<u64>, // unix seconds

    // Last time `mapping` was CONFIRMED current: either a fresh GET, or a
    // HEAD/conditional check that reported "unchanged." This is the single
    // field the 24h/1-week thresholds below are computed from. `None` means
    // "never successfully confirmed," treated identically to no cache file
    // existing at all.
    last_checked_at: Option<u64>,

    // Set the instant a HEAD/conditional check confirms the server has
    // newer content than `mapping` reflects; cleared only once a follow-up
    // download succeeds. Exists so a run killed between "confirmed stale"
    // and "downloaded the replacement" doesn't redundantly re-check on the
    // next run -- it already knows to go straight to the download.
    known_stale: bool,

    // Network-level failures (timeout, DNS, unexpected status) since the
    // last successful attempt of either kind. A HEAD check that
    // successfully reports "stale" does NOT increment this -- that's a
    // successful network operation with a business-level "you need new
    // data" result, not a failure.
    consecutive_failures: u32,

    // Last time ANY check or download was attempted, success or failure.
    // Feeds the backoff cooldown below; distinct from `last_checked_at`
    // because a failed attempt bumps this without bumping that.
    last_attempt_at: Option<u64>,

    // Normalized pypi_name -> conda_name. Only entries that differ.
    mapping: HashMap<String, String>,
}
```

A `schema_version` mismatch (from a future `ana` release changing this
shape) is treated identically to a missing or corrupt file: never a hard
error, never a panic, just "no usable cache" -- this crate's failure mode is
always "fall back to less-fresh or absent data," never "block `ana` from
running."

## Hot path stays synchronous and network-free

`load_mapping` never blocks the caller on network I/O by itself. The only
network-touching entry point is `load`'s internal decision to either run a
refresh inline (blocking) or hand it to a background `std::thread`, per the
state machine below -- and even the blocking cases use short client
timeouts so "block until it completes or errors" resolves in a bounded time
on a genuinely offline host rather than hanging on a default HTTP timeout.

## The four-case state machine

Driven entirely by `last_checked_at`'s age, with one override
(`--allow-stale-mapping`) and one bypass (`force_refresh`):

| Cache state | Behavior |
|---|---|
| No cache, or `last_checked_at` never set | Block until a fresh download completes or fails outright. This is the one case `load` can return `Err` from. |
| Age < 24h | Use cached data as-is. No network call at all. |
| 24h ≤ age < 1 week | Use cached data immediately; spawn a background check (HEAD, falling back to conditional GET) subject to the backoff cooldown below. The caller must join this before the process exits (see "Why `finish()` isn't optional" below) for the result to ever reach disk. |
| Age ≥ 1 week | Same as "no cache": block for a fresh download -- *unless* `--allow-stale-mapping` is passed, in which case this row behaves exactly like the 24h–1-week row (use stale data now, background-check subject to backoff). |
| any state, `force_refresh` requested | Bypass all of the above: block for a fresh download immediately, ignoring age and backoff. |

### One state-mutating primitive, two call sites

The "background check" and "blocking download" cases are not two separate
implementations -- they're the same `perform_refresh` function, called
either inline (blocking cases) or inside a spawned thread (background
case). `perform_refresh` is the only code that talks to the network or
writes the cache file:

1. If there's a prior envelope and it isn't already `known_stale`, do a HEAD
   check (fallback: conditional GET) against the stored `etag`/`last_modified`.
   - Server says unchanged → persist `last_checked_at = now`,
     `consecutive_failures = 0`, `mapping` untouched. Done.
   - Server says changed → persist `known_stale = true` immediately (before
     attempting the download, for the resume case above), then fall through.
   - Network failure → persist `consecutive_failures += 1`,
     `last_attempt_at = now`, `mapping`/`last_checked_at` untouched. Done.
   - Conditional-GET fallback specifically: if that check's `200` response
     already carries the full new body (which it does, since a conditional
     GET either 304s or returns the complete resource), skip the separate
     download step entirely and persist the new data directly.
2. Otherwise (no prior envelope, or already `known_stale`): download and
   parse directly.
   - Success → persist the full new envelope (`mapping`, `etag`,
     `last_modified`, `fetched_at = now`, `last_checked_at = now`,
     `known_stale = false`, `consecutive_failures = 0`).
   - Failure → persist `consecutive_failures += 1`, `last_attempt_at = now`;
     `known_stale` (if already true) is left set so the next attempt skips
     straight back to this step instead of re-checking.

A failed *first-ever* attempt (no prior cache at all) still writes this
minimal envelope -- empty `mapping`, failure counters set -- rather than
leaving no file behind. The four-case table above always treats a
never-successfully-confirmed cache as "no cache" regardless (so this doesn't
change *decision* behavior), but it gives the next invocation real failure
history to reason about, and is what the backoff cooldown below is keyed
off of from the very first failure onward.

### Backoff: a simple attempt budget, not exponential decay

This only ever runs once per `ana` invocation (there's no long-lived
process to schedule retries within), so a simple counter is enough: up to
**10 consecutive failures** are retried freely (every eligible invocation
attempts the background check with no extra delay), then a **1-hour
cooldown** where the background check is skipped entirely regardless of how
long it's been, after which the budget resets and the next 10 failures are
again retried freely. Concretely: if `consecutive_failures >= 10`, skip
unless `now - last_attempt_at >= 1h`; if that hour has passed, reset
`consecutive_failures` to 0 before attempting, restarting the burst. This
backoff only ever gates the *background* check -- the blocking cases (no
cache, week-stale without the flag, `force_refresh`) always attempt
immediately, since the caller has explicitly said it needs an answer now;
they still update the same counters for whoever checks next.

## Public API

```rust
pub struct LoadOptions {
    pub allow_stale_mapping: bool,
    pub force_refresh: bool,
}

/// Synchronous entry point. `Err` only from the no-cache/week-stale blocking
/// paths failing outright with nothing to fall back to -- every other path
/// always returns `Ok` with the best data available.
pub fn load(options: LoadOptions) -> Result<MappingHandle, MappingError>;

pub struct MappingHandle { /* map + Option<JoinHandle<RefreshOutcome>> */ }

impl MappingHandle {
    pub fn get(&self, pypi_name: &str) -> Option<&str>;
    pub fn as_map(&self) -> &HashMap<String, String>;

    /// Joins any in-flight background refresh and returns what happened.
    /// `RefreshOutcome::NotNeeded` if nothing was spawned.
    pub fn finish(self) -> RefreshOutcome;
}
```

### Why `finish()` isn't optional (and the `Drop` safety net)

`std::thread::JoinHandle`s are not auto-joined: if `main()` returns without
joining, the spawned thread is simply killed, possibly before it has
written anything. So calling `finish()` before the CLI exits isn't
politeness, it's required for the background-check case's disk write to
happen at all. `MappingHandle` additionally joins its background thread in
`Drop` as a safety net -- the same pattern `tracing`'s `WorkerGuard` uses --
so a caller that forgets to call `finish()` explicitly still gets the
refresh persisted (just without the `RefreshOutcome` to act on); the worst
case from *that* bug is a wasted attempt, never a corrupted cache, since the
atomic rename means anything killed mid-refresh just leaves the previous
file untouched.

## Build-time URL

A `const DEFAULT_MAPPING_URL: &str = "..."` compiled into the crate, with an
`ANA_PYPI_MAPPING_URL` environment variable checked at runtime (not baked in
via `build.rs`) as an override for testing/staging. No `build.rs` is
introduced for this -- matches the rest of the workspace's preference for
plain, synchronous code over build-time machinery for a single string
constant.

## Dependencies

New, direct workspace dependencies (none of these exist elsewhere in the
workspace yet):

- `reqwest`, blocking feature only, default-features off aside from that
  (no tokio pulled in -- matches the workspace's fully synchronous
  convention elsewhere).
- `serde_json` -- parsing the upstream API response.
- `rmp-serde` -- encoding/decoding `CacheEnvelope`.
- `tempfile` -- already resolvable transitively (via the git-pinned
  `uv-fs` crate) but not a direct dependency of any `ana-*` crate yet;
  promoted to direct here for `NamedTempFile::new_in`/`persist`.
- `directories` -- OS-appropriate cache directory resolution.

## Testing

The HTTP layer is behind a small trait (`HttpClient` or similar) so the
state machine (`decide`/`perform_refresh`) can be tested against an
in-memory fake returning canned 200/304/error responses, without adding a
mock-HTTP-server dependency to the workspace just for this. Coverage
target: every state-machine transition in the table above, the
known_stale-resume path, the 10-failures/1-hour backoff boundary, and the
atomic-replace behavior (a reader that opens the file before a concurrent
writer's rename still sees a complete, valid old version -- never a
truncated one).
