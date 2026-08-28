# Downloading v3-repodata wheels and conda packages, and installing them fast

Scope: `sync_algorithm.md` already decided *that* steps 3-4 (read live prefix,
diff, act) are two rattler library calls
(`Transaction::from_current_and_desired` +
`Installer::install`) and not hand-rolled code. This doc goes one level
deeper — into how those calls actually move bytes off the network and onto
disk — because the task now is writing the code that does the downloading,
and the prompt that produced this doc specifically asked for that to be as
fast as possible for **both** conda packages and v3-repodata wheels.

Everything below is checked directly against source, not against docs or
general familiarity:

- The project's own rattler fork, `intentionally-left-nil/rattler`, at the
  workspace's pinned rev `66ffb3a7e4083629a7df8e6e41d6d54da037b742`
  (`~/.cargo/git/checkouts/rattler-2dc2526ba04bceeb/66ffb3a`).
- `prefix-dev/pixi`, `main` branch, cloned locally at `~/scratch/pixi`.

File:line references below are relative to those two checkouts unless
stated otherwise.

**2026-08 revision notice**: the wheel-cache section of this doc (originally
written against the *pinned* fork rev above, before any wheel-cache fixes
existed) has since been superseded by a review of an in-progress fork
*branch* that isn't merged yet. That review found the actual gaps were
different, and in one case worse, than what this doc originally diagnosed:
wheels are currently downloaded and extracted **twice** per install (a real
bug, not a design gap), and have **zero** hash verification against
repodata (a supply-chain-relevant gap, not the "weaker guarantee" this doc
originally called it). It also corrected this doc's read of cross-process
locking (already incidentally adequate) and rejected part of
`rattler_wheel_cache_parity_spec.md`'s recommended refactor (a
collision-avoidance change for a collision that can't occur, and a
lock-file relocation that would have broken mutual exclusion during
rollout). The sections below marked "(revised)" reflect the corrected
understanding; see "Amendment history" at the end for the full account of
what changed and why.

**2026-08 supersession notice**: recommendation 5 and "Pixi's fingerprint +
lock short-circuit, exact mechanics" below describe a `.ana-install-marker`
fingerprint/interruption scheme that `investigations/env_state_implementation_plan.md`
has since superseded end to end: that state now lives in `ana-lockfile`'s
env lock (`<env_path>/ana.lock`, a `dirty` bit plus the last-reconciled
section) rather than a dedicated marker file inside `ana-installer`, and
"was the last install interrupted" is answered by a recursive wipe of
`env_path` rather than a forced `with_reinstall_packages` call. Everything
else in this doc — the download/install pipeline mechanics, cache
location, the global cache lock, wheel visibility — is unaffected and
still accurate; only the two sections called out above (and the
"Suggested shape" sketch's fingerprint-check bullet) describe a design
that is no longer what's implemented. See the new plan's own doc for the
current algorithm.

## Summary of recommendations

1. **Build a real HTTP client with retry middleware and pass it explicitly**
   to both `Gateway::builder().with_client(...)` and
   `Installer::new().with_download_client(...)`. Neither defaults to one —
   see "Gap: `ana-solver` currently has no retry middleware at all" below.
   This is not a nice-to-have; it's the single easiest, highest-leverage fix
   available and `ana-solver` doesn't have it today.
2. **Do not call `Installer::with_max_concurrent_requests`/
   `with_concurrent_requests_semaphore` at all**, at least initially — mirror
   pixi's own, verified behavior (see "Pixi's actual concurrency knobs").
   **These methods are a throttle, not a concurrency mechanism** — rattler
   already fetches every package in a transaction concurrently by default
   (one `tokio::spawn`ed task per package, no gate in front of it; see "The
   install/download pipeline"); calling either method installs a semaphore
   permit *in front of* that default and makes installs slower on purpose,
   in exchange for bounding resource usage. Leaving them unset is what gets
   the *most* concurrency out of rattler, not less. Only add a cap
   later if a real symptom (connection exhaustion, server rate-limiting)
   shows up in practice.
3. **Set an explicit `io_concurrency_semaphore`** (filesystem/link
   concurrency, not network) sized to the machine, the same knob pixi
   exposes as `max_io_concurrency` — this is a real, separate concern from
   download concurrency and rattler's own default (100) is a reasonable
   starting point, not something to leave to chance if `ana` ever installs
   more than one environment's worth of packages concurrently in the same
   process.
4. **Decided: wheels are sourced exclusively from channels that publish
   sharded (CEP-16) repodata — no legacy-`repodata.json` fallback for
   wheels.** The plain-`repodata.json` path silently drops any `v3.whl`
   entries regardless of what's in the file (see "Wheel visibility requires
   sharded repodata" below), so rather than fixing that path to see wheels
   too, `ana` sidesteps it: any channel `ana` expects to serve wheels from
   must publish sharded repodata, full stop. Conda-package channels are
   unaffected and keep using whichever repodata format they already do.
5. **Superseded (2026-08).** Was: "adopt pixi's fingerprint+lock
   short-circuit," layered inside the per-environment advisory lock. This
   is now `ana-lockfile`'s env lock instead (a `dirty` bit plus the
   last-reconciled section, compared against `ana.lock`'s current section
   by the caller) -- see `investigations/env_state_implementation_plan.md`
   and this doc's top-of-file supersession notice.
6. **Decided: use rattler's own generic, cross-tool cache location as-is —
   for repodata and for packages (conda and wheel alike) — rather than a
   separate `ana`-branded cache root.** Concretely: one root,
   `rattler_cache::default_cache_dir()` (honors `$RATTLER_CACHE_DIR`, else
   `dirs::cache_dir().join("rattler/cache")`), with `repodata`/`pkgs`/`wheel-pkgs`
   as its standard subdirectories — the exact same location `pixi`,
   `rattler-build`, and this fork's own `rattler-bin` reference CLI already
   use. `ana` currently does *not* do this (see "Cache location" below for
   the concrete gap and fix); this closes it. This buys real disk and
   network savings — not just within `ana` across projects and
   `.ana/<hash>/env` selections, but across *every* rattler-based tool on
   the machine — and it comes bundled with a consequence that isn't a
   separate choice: `rattler_cache::PackageCache::acquire_global_lock` is a
   single *exclusive*, whole-cache lock held for an entire `install()` call,
   scoped to wherever the cache root is, so one shared root necessarily
   means one shared lock. See "The global cache lock is a mechanical
   consequence of the shared-cache decision, not a separate choice" — there
   is no further decision to make there; installing into `.env/` and
   installing into `.ana/<hash>/env/` for the same project already
   serialize on this, and that's what "one shared cache" means, not an
   independent risk to weigh.
7. **Wheel-path parity with the conda path is being implemented on a fork
   branch, not yet merged** — track it, don't route around it, and **do not
   ship or benchmark `ana` wheel installs against pre-fix rattler**. A
   direct review of that branch (not just the pinned base rev) found the
   actual gaps are worse than this doc originally characterized: wheels are
   currently downloaded and extracted **twice** per install (a bug, not a
   design gap), and have **zero** hash verification against repodata (a
   supply-chain-relevant correctness gap). See the revised "What's missing
   on the wheel path" below for the full, corrected list and which fixes
   are load-bearing gates versus nice-to-haves.
8. **Decide `ana`'s pip-interoperability requirement now, and hand it to
   the rattler side as a requirement, not an implementation detail**:
   whether an `ana`-managed, wheel-containing environment needs to be
   legible to a bare `pip list`/`pip uninstall`/`importlib.metadata` (or
   `uv`) run inside it. Rattler owns *how* (an `INSTALLER` file, a `RECORD`
   that reflects real installed paths, optionally `direct_url.json`) but
   only `ana` can decide *whether* — see "Decisions `ana` needs to make
   before building an installer" below.
9. **Cache pruning is safe whenever you get around to building it, and
   nothing about that needs deciding now.** `rattler_cache` has no
   TTL/LRU/prune/size-query API for either the conda or the wheel cache —
   but *removing* a cache entry is provably safe (see "Decisions `ana`
   needs to make," cache-GC bullet, for why), so there is no size-tracking
   or eviction machinery `ana` needs to build alongside the installer
   itself. This is a "build it later, whenever cache size actually becomes
   a problem" item, not a design constraint on anything in this doc.

## What "v3 repodata" and "wheel as a conda package" mean here, verified

`rattler_repodata_gateway::reporter::SUPPORTED_REPODATA_REVISION` is
`RepodataRevision::V3` (`rattler_repodata_gateway/src/reporter.rs:21`) — "v3"
is a real, versioned top-level layout of `repodata.json` this fork
understands, not shorthand for "new-ish." Concretely
(`rattler_repodata_gateway/src/sparse/mod.rs:568-601`), a `repodata.json` can
carry a top-level `v3` map with three sub-maps, keyed by an
*extension-less* archive identifier (`{name}-{version}-{build}`, no
`.conda`/`.tar.bz2`/`.whl` suffix):

```
v3.tar.bz2   -- same PackageRecord shape as the legacy `packages` map
v3.conda     -- same PackageRecord shape as the legacy `packages.conda` map
v3.whl       -- a WhlPackageRecord: PackageRecord (flattened) + a required `url`
```

`WhlPackageRecord` (`rattler_conda_types/src/repo_data/mod.rs:693-700`) is
just a `PackageRecord` (name, version, build, `depends`, `sha256`, `size`,
...) plus a `url: UrlOrPath` that can be absolute or relative-to-channel.
`depends` on that record has to already be conda matchspec syntax — this is
the same shape `ana-pep508-to-matchspec`/`ana-marker-matchspec` produce, so
whatever publishes `v3.whl` repodata for a PyPI package is expected to have
already run that same PEP 508 -> matchspec conversion `ana` uses on the solve
side.

On the type side, `DistArchiveType` (`rattler_conda_types/src/package/archive_type.rs:64-121`)
is `Conda(CondaArchiveType) | Wheel(WheelArchiveType)` — a wheel-origin
record and a conda-origin record are the *same* `RepoDataRecord`/
`PrefixRecord` type, distinguished only by this one field, exactly as
`sync_algorithm.md` already established for the install side. What's new
here is the *fetch* side: how a wheel candidate gets from a channel's
repodata into that unified `RepoDataRecord` list in the first place.

Format consolidation across `.tar.bz2`/`.conda`/`.whl` for the *same*
name-version-build is controlled by `PackageFormatSelection`
(`rattler_repodata_gateway/src/sparse/mod.rs:40-74`). The variant that
surfaces wheels is `PreferCondaWithWhl`: "`.tar.bz2`, `.conda` and `.whl`
packages are used, but if a `.conda` exists that represents the same content
... the `.conda` package is selected and the `.tar.bz2` [or `.whl`] is
discarded" (`DistArchiveType::cmp_preference`,
`rattler_conda_types/src/package/archive_type.rs:76-93`: `.conda` > `.tar.bz2`
> `.whl`). So even once wheels are visible at all, a wheel candidate never
wins a solve over a `.conda`/`.tar.bz2` build of the identical
name-version-build published in the same repodata — it only matters for
names that have *no* conda-native build, which is the actual PyPI-only case
`ana` cares about.

## Wheel visibility requires sharded repodata

`PackageFormatSelection` (`sparse/mod.rs:40-74`) controls whether a subdir
fetch surfaces `.whl` records at all, and only one of the `Gateway`'s three
subdir-client implementations ever asks for the variant that does
(`PreferCondaWithWhl`):

| Subdir source | `PackageFormatSelection` used |
|---|---|
| `file://` channel (`gateway/local_subdir.rs:87,112`) | Hardcoded `PreferConda` — never sees `v3.whl` |
| plain `http(s)://` `repodata.json` (`gateway/remote_subdir/tokio.rs`, same `local_subdir.rs` code underneath) | Hardcoded `PreferConda` — never sees `v3.whl` |
| sharded (CEP-16) repodata (`gateway/sharded_subdir/`) | Reads `v3.whl` shards directly (`tokio/mod.rs:134-143`) |

**Decision: `ana` sources wheels exclusively from sharded-repodata
channels.** Rather than patching the two non-sharded paths to see wheels
too (a real, small fork change — the sparse-repodata parser already
supports `PreferCondaWithWhl` fully, only the wiring into
`local_subdir.rs`/`remote_subdir/` is missing — but still a fork PR `ana`
doesn't need if it just requires sharding), any channel `ana` expects wheel
candidates from is required to publish sharded repodata. Conda-only
channels are unaffected either way, since `PreferConda` already sees every
conda-native record regardless of repodata layout.

One sharp edge to keep in mind even with that requirement in place: sharding
is an *opportunistic* path, not an enforced one — `subdir_builder.rs:65-93`
falls back to the non-sharded (and therefore wheel-blind) client on
`GatewayError::SubdirNotFoundError` or `ShardedIndexNotCached`, silently,
with no error surfaced to the caller. A channel that normally publishes
sharded repodata but is transiently missing its shard index (or is being
queried in cache-only mode before anything's been cached) degrades to
"wheels invisible" rather than failing loudly — worth knowing before
treating "the solve came back with no wheel candidates" as proof a channel
has none, rather than proof the sharded path didn't load this time.

## The install/download pipeline, verified end to end

`rattler::install::Installer::install()` (`rattler/src/install/installer/mod.rs`)
is the single call `sync_algorithm.md` already pointed at for steps 3-4.
Concretely, once the `Transaction` is built (`mod.rs:627-641`):

1. **One global cache lock, once, for the whole call**
   (`mod.rs:657-662`): `package_cache.acquire_global_lock()` — an OS-level
   *exclusive* `flock`-style lock on a single file at the cache root
   (`rattler_cache/src/package_cache/cache_lock.rs:126-160`, `file.lock()` is
   `fs4`'s exclusive lock, not a shared one), held until `install()` returns.
   The comment at the call site says why: "reduces overhead by avoiding
   per-package locking." See "The global cache lock is a mechanical
   consequence of the shared-cache decision, not a separate choice" for what
   that means for `ana` specifically.

2. **Removals are queued as plain futures; installs are queued as spawned
   tasks — in that order, but installs' network fetch starts immediately
   regardless** (`mod.rs:704-891`). The remove-operations loop
   (`mod.rs:706-738`) builds an `async move` per removal but does not spawn
   it — nothing runs until the stream is polled. The install-operations loop
   (`mod.rs:742-891`) `tokio::spawn`s the fetch half of each operation
   immediately as the loop runs (`mod.rs:777`), so every install's download
   is already in flight in the background by the time `pending_unlink_futures`
   is drained (`mod.rs:894-897`) — removals and downloads overlap for free,
   not because of any deliberate ordering trick.

3. **Install operations are processed largest-first**
   (`mod.rs:742-751`): `.sorted_by_key(|op| size).rev()` before spawning.
   This is a real, deliberate scheduling choice (not incidental) — starting
   the biggest downloads first avoids the classic straggler problem where a
   handful of huge packages end up serialized after a wave of small ones
   because they were scheduled last.

4. **Fetch = stream-download + decompress + extract + hash-verify in one
   pass, not download-then-extract**: `populate_cache`
   (`mod.rs:1000-1071`) calls `PackageCache::get_or_fetch_from_url_with_retry`
   (`rattler_cache/src/package_cache/mod.rs:806-948`), whose fetch closure
   calls `rattler_package_streaming::reqwest::tokio::extract` — the HTTP
   response body is decompressed and untarred as it arrives, and the SHA256
   (falling back to MD5) is checked against the expected hash from repodata
   once the stream completes (`mod.rs:864-911`); a mismatch deletes the
   partial extraction and is treated as a retryable error. There is no
   separate "download whole file to a temp path, then unpack" step for
   conda packages — the two are one pipelined operation.

5. **Per-package concurrency is real but two-tiered.** Network fetch runs
   inside a `tokio::spawn`, one task per install operation
   (`mod.rs:777-814`); the *linking* step that follows (writing files into
   the prefix, building the `PrefixRecord`) runs via `rayon::spawn_fifo`
   (`mod.rs:962-995`) — CPU/filesystem-bound work handed to the rayon pool
   rather than tying up a tokio worker thread. `io_concurrency_semaphore`
   (`InstallDriver::acquire_io_permit`, `driver.rs:156-161`) gates this
   filesystem side; it has nothing to do with network concurrency.

6. **Per-package locking is in-process and cheap, not per-file-on-disk**:
   within one `PackageCache`, concurrent fetches of the *same* cache key
   (e.g. two operations that happen to resolve to an identical package) are
   deduplicated by a `DashMap<CacheKey, Arc<Mutex<...>>>`
   (`rattler_cache/src/package_cache/mod.rs:320-329`, used in both
   `try_validate` and `validate_or_fetch`, `mod.rs:378-451`) — the second
   caller blocks on the same in-process mutex rather than re-downloading.
   Cross-process safety for the *same key* is not separately locked at this
   level — it's covered by the one global lock from step 1, which is process-
   wide by construction (an OS file lock), not by any per-entry file lock.

7. **`concurrent_requests_semaphore` is optional and, when set, gates
   conda-package fetch+extract as one unit** — the permit
   (`mod.rs:836-844` inside `get_or_fetch_from_url_with_retry`) is acquired
   before the download starts and held until extraction finishes, so it
   bounds "downloads and extractions in flight," not raw HTTP connections.
   When unset (`Installer`'s default), there is no cap at this layer at all.

8. **Retries happen at two independent layers**, and `ana` needs both:
   the extraction closure above retries *stream failures during
   download/extract* (truncated body, hash mismatch) using
   `rattler_networking::retry_policies::default_retry_policy()`
   (`ExponentialBackoff::builder().build_with_max_retries(3)`,
   `rattler_networking/src/retry_policies.rs:23-25`) — this is baked into
   `PackageCache` itself and needs no `ana` wiring. But the *initial HTTP
   request* (connection refused, 5xx before any body arrives) is only
   retried if the `LazyClient` passed in was itself built with a
   `reqwest-retry` middleware — the streaming-extract retry loop happens
   *after* a response is already in hand and doesn't reissue a request that
   never got one. See the gap called out below: `ana-solver`'s `Gateway`
   currently supplies no such client, so today it has neither retry layer.

## What's missing on the wheel path (revised — corrected against an in-progress fork branch)

The original version of this section, written against the pinned base rev
with no wheel-cache work in progress, characterized the wheel path as
"structurally sound but missing two mechanical protections." A direct
review of an actual in-progress fork branch (not yet merged) found that
framing was too generous: there are two real bugs, one of them
security-relevant, underneath the mechanical gaps. Ranked by what actually
needs to gate shipping wheel support, most severe first:

- **Confirmed real, and correctly diagnosed by this doc originally**:
  `populate_wheel_cache` never touches `concurrent_requests_semaphore` — a
  large wheel-heavy transaction ignores whatever download-concurrency cap
  `ana` might set on the `Installer`, opening as many concurrent connections
  as there are wheels regardless. Real, but P1 next to the two bugs above,
  not P0.
- **Corrected, in `ana`'s favor**: this doc originally said the wheel cache
  has "no cross-process lock." That's true of a *dedicated* wheel-cache
  lock, but it overstates the actual exposure: `Installer::install()`
  already acquires the conda package cache's global exclusive lock once,
  for the *entire* call, and wheel population happens inside that same
  call, inside that same held lock. Two concurrent `Installer::install()`
  calls — from two different processes, or even two calls in the same
  process — already fully serialize against each other today, which
  incidentally covers wheel population too, as long as every wheel fetch
  continues to happen only via `Installer::install()` (true today, and not
  expected to change). This is "adequate by accident," not "by design" —
  worth knowing precisely rather than assuming either "it's unsafe" (this
  doc's original claim) or "it's properly designed" (it isn't; it's a
  side effect of the outer lock's scope) — but it means in-process/cross-process
  locking is **not** something `ana` needs to wait on or work around.

## Pixi's actual concurrency knobs (verified against `~/scratch/pixi`, not assumed)

The natural assumption going in was "pixi obviously throttles concurrent
package downloads via its `max-concurrent-downloads` config setting." That
assumption is **wrong** for the actual install step, checked exhaustively:

Every `Installer::new()` construction site in the repo —
`crates/pixi_command_dispatcher/src/install_pixi/ext.rs:292`,
`crates/pixi_command_dispatcher/src/install_binary.rs:44`, and
`crates/pixi_cli/src/exec.rs:352` — sets `.with_download_client(...)`,
`.with_package_cache(...)`, `.with_execute_link_scripts(...)`, and
(the first two) `.with_io_concurrency_semaphore(...)` from
`HasIoConcurrencySemaphore`. **None of the three ever calls
`.with_max_concurrent_requests` or `.with_concurrent_requests_semaphore` on
the `Installer`.** Package artifact downloads during an actual
`pixi install`/`pixi exec`/binary install therefore run with **no
concurrency cap at the `Installer` layer** — bounded only by however many
packages are in the transaction, the reqwest connection pool's
`pool_max_idle_per_host = 20` (`pixi_utils/src/reqwest.rs:69`, which bounds
*idle* connections kept warm, not concurrent in-flight requests), and OS
file-descriptor limits.

`config.max_concurrent_downloads()` (default `50`,
`pixi_config::default_max_concurrent_downloads`,
`rattler_config/src/config/concurrency.rs:16`) is real and does get used —
but every one of its four call sites feeds a `Gateway`'s
`.with_max_concurrent_requests(...)` (`pixi_core/src/workspace/repodata.rs:15`,
`pixi_global/src/project/mod.rs:1892`, plus the `CommandDispatcherBuilder`'s
`.with_max_download_concurrency` at `command_dispatcher/builder.rs:385`,
which — confirmed by reading `builder.rs:340-385` — is *itself* only wired
into `Gateway::builder().with_max_concurrent_requests(...)`, never into an
`Installer`). **Pixi's one user-facing "max concurrent downloads" setting
governs repodata/shard-fetch concurrency only; it has no effect on package
artifact download concurrency at install time.** The only concurrency knob
pixi actually applies at install time is `max_io_concurrency`
(`pixi_command_dispatcher/src/util/limits.rs:28-31`, "bounds the file
descriptors held during installation... installing several environments
concurrently multiplies that") → `io_concurrency_semaphore`, and that's
filesystem/linking concurrency, not network.

Whether this is deliberate design or an oversight in pixi isn't something
the source alone answers, but it's the actual, current, verified behavior
of the reference implementation the prompt asked to mirror — which is why
recommendation 2 above is "don't add a download cap `ana` doesn't need,"
not "copy pixi's (nonexistent) cap."

### Pixi's HTTP client construction (`pixi_utils/src/reqwest.rs`)

The shared client pixi hands to both the `Gateway` and every `Installer` is
built once (`build_reqwest_clients`/`build_lazy_reqwest_clients`,
`reqwest.rs:257-298`), not left at `reqwest`/`LazyClient` defaults:

- `pool_max_idle_per_host(20)`, `read_timeout(5 minutes)`,
  a `pixi/<version>` user agent (`reqwest.rs:66-69,131-135`).
- TLS: `rustls` by default (webpki bundled roots) unless built with the
  `native-tls` feature, in which case it uses the OS trust store
  (`reqwest.rs:78-129`).
- Middleware stack, in this exact order, and the ordering is load-bearing
  per the inline comments (`reqwest.rs:203-255`):
  `OfflineMiddleware` (if offline; must be first so nothing else even tries
  the network) -> `RetryTransientMiddleware::new_with_policy(ExponentialBackoff::builder().build_with_max_retries(3))`
  (must precede mirror selection so a retried request can pick a different
  mirror) -> `MirrorMiddleware` (only if mirrors configured) ->
  `OciMiddleware` (unconditional, no-op for non-`oci://` URLs) ->
  `GCSMiddleware` -> `S3Middleware` -> auth middleware -> `AuthChallengeMiddleware`
  (reacts to `WWW-Authenticate` challenges).

### Gap: `ana-solver` currently has no retry middleware at all

Checked against both defaults that apply when a caller doesn't supply its
own client:

- `Gateway::builder().finish()`'s default client
  (`rattler_repodata_gateway/src/gateway/builder.rs:209-215`) is
  `ClientWithMiddleware::from(Client::builder().user_agent(USER_AGENT).build())`
  — an empty middleware stack.
- `Installer::new()`'s default `downloader` is `LazyClient::default()`
  (`rattler/src/install/installer/mod.rs:643`), and
  `impl Default for LazyClient` (`rattler_networking/src/lazy_client.rs:23-25`)
  is `ClientWithMiddleware::default().into()` — also empty.

`ana-solver::Solver::new` builds its `Gateway` with
`Gateway::builder().with_cache_dir(cache_dir).finish()`
(`crates/ana-solver/src/lib.rs:107`) — no `.with_client(...)` call. So
**today, every repodata fetch `ana-solver` makes has zero retry behavior**:
a single dropped connection or transient 5xx fails the whole solve. This
predates and is independent of the download-pipeline work this doc is
about, but it's the same client-construction gap that would otherwise get
carried forward into a new installer crate, so it belongs in the same fix.

## Pixi's fingerprint + lock short-circuit, exact mechanics (verified; superseded 2026-08)

**Superseded.** `ana` did not end up adopting this mechanism as its own
install-short-circuit -- `investigations/env_state_implementation_plan.md`
replaces it with `ana-lockfile`'s env lock (`<env_path>/ana.lock`: a
`dirty` bit plus the last-reconciled section), decided by the caller
comparing packages, not a dedicated fingerprint file inside
`ana-installer`. This section is kept as the verified research record of
pixi's actual mechanism (still useful provenance for *why* a
short-circuit-plus-crash-recovery scheme is worth having at all), not as
a description of what `ana` currently does.

`sync_algorithm.md` already flagged that this exists and is worth adopting;
here is the precise algorithm, read directly from
`pixi_utils/src/environment_fingerprint.rs` and
`pixi_utils/src/environment_lock.rs` (not the earlier, less specific
characterization):

- **Fingerprint** (`environment_fingerprint.rs:28-50`): collect
  `(name.as_normalized(), sha256)` for every `RepoDataRecord`, **sort by
  name** (order-independence), then fold through an `xxh3` hasher (not
  SHA-256 — this is a cache key, not a security digest, and `xxh3` is
  materially cheaper for something computed on every invocation) and format
  the 64-bit digest as 16 lowercase hex characters.
- **Lock file** (`environment_lock.rs`): one file at
  `<prefix>/conda-meta/.pixi-environment-fingerprint`, opened
  read+write+create and locked exclusively via the `async_fd_lock` crate
  (`environment_lock.rs:68-71`, `156-166`). The file's *content* doubles as
  a tiny state machine, always written as one fixed-width (16-byte) write at
  offset 0 (`write_marker`, `environment_lock.rs:146-151`) — small and
  in-place enough that the module's own doc comment calls out this exact
  width as load-bearing for torn-read safety against unlocked readers
  (`EnvironmentFingerprint::read`, a lock-free peek used by e.g. an
  activation cache).
  - Empty file (never written) -> `Fresh`.
  - 16 bytes that are all ASCII hex digits -> `Installed(fingerprint)`.
  - Anything else (in practice, the fixed sentinel
    `b"pixi:installing!"`, also exactly 16 bytes) -> `Interrupted`.
- **Usage** (`install_pixi/ext.rs:236-283`): acquire the lock up front
  (`EnvironmentLock::acquire_with_progress`, warns every 30s if blocked on a
  peer); if `matches(&installed_fingerprint)` and there's no forced
  reinstall, return a synthetic empty `Transaction` without reading
  `conda-meta`, building a real `Transaction`, or calling `Installer` at
  all. If `was_interrupted()`, force every package to reinstall (don't trust
  a prefix a crashed process may have left half-written). Otherwise:
  `begin()` (write the in-progress sentinel), run the real install, then
  `finish(&fingerprint)` (write the real fingerprint, still under the same
  lock) — best-effort; a failed write just costs the next process one extra
  reinstall, not correctness.

For `ana`: **this paragraph describes the superseded design** (see the
section header above and the doc's top-of-file supersession notice). What
`ana` actually built is `ana-lockfile`'s env lock: no per-record
fingerprint hash at all, a plain `packages` equality check (both sides
sorted into canonical order) against the env lock's last-reconciled
section, with a `dirty` bit -- set before a reconcile starts, cleared
after it succeeds -- standing in for `Interrupted`/`Fresh`/`Installed`.
The mechanism differs from what's described below, but the *reason* for
having a short-circuit and a crash-recovery bit at all is the same one
this section documents.

## Cache location: use rattler's generic location as-is

**Decision (recommendation 6): repodata and packages both live under
rattler's own default cache location — the same one `pixi`, `rattler-build`,
and this fork's own `rattler-bin` reference CLI already use on the same
machine — not an `ana`-branded location.** This section verifies exactly
what that location is, how the two existing consumers (`Gateway`,
`Installer`) actually resolve it (they don't do it identically — see the
asymmetry below), and what has to change in `ana`'s own code to line up
with it, since **`ana` currently does not do this**.

### What "rattler's generic location" actually resolves to, verified

`rattler_cache::default_cache_dir()` (`rattler_cache/src/lib.rs:19-36`) is
the single function that defines it: the `RATTLER_CACHE_DIR` environment
variable if set, otherwise `dirs::cache_dir().join("rattler/cache")` — which
is `~/Library/Caches/rattler/cache` on macOS, `~/.cache/rattler/cache` (or
`$XDG_CACHE_HOME/rattler/cache`) on Linux, `%LOCALAPPDATA%\rattler\cache` on
Windows. `rattler::default_cache_dir()` (`rattler/src/lib.rs:44-46`, what
`ana-solver` would import today if it used this at all) is a direct,
one-line passthrough to the same function. Three fixed subdirectory names
live under that one root, as constants in the same crate
(`rattler_cache/src/consts.rs`): `PACKAGE_CACHE_DIR = "pkgs"`,
`WHEEL_CACHE_DIR = "wheel-pkgs"`, `REPODATA_CACHE_DIR = "repodata"`.

`rattler_cache::ensure_cache_dir(path)` (`rattler_cache/src/lib.rs:44-51`)
is the companion function every caller of this convention is expected to
run once against the root before using it: `create_dir_all` plus
`rattler_conda_types::backup::exclude_from_backups` (writes a
`CACHEDIR.TAG` so generic backup tools skip it, and on macOS additionally
marks the directory excluded from Time Machine). This is part of what
"use it as-is" means in practice — not just the same path, but the same
one-time hygiene step every other rattler-based tool already does to it.

### The asymmetry between the two consumers, verified against `rattler-bin`

`Gateway` and `Installer` do **not** resolve this convention identically,
confirmed by reading the fork's own reference CLI (`rattler-bin`), which
exists in the same repo specifically to demonstrate intended usage:

- **`Gateway::builder().finish()`'s own bare default does not append
  `REPODATA_CACHE_DIR`, and does not check `$RATTLER_CACHE_DIR`.** Its
  default (`gateway/builder.rs:217-221`, already covered under "Gap:
  `ana-solver` currently has no retry middleware at all") computes
  `dirs::cache_dir().join("rattler/cache")` directly, inline, independent
  of `rattler_cache::default_cache_dir()` — and hands that raw root
  straight to the repodata-fetching code with no `repodata/` subdirectory
  appended at all. **The caller is expected to be explicit.** Every
  `rattler-bin` command that builds a `Gateway`
  (`rattler-bin/src/commands/{create,exec,solve}.rs`) does exactly that:
  ```rust
  let cache_dir = default_cache_dir()?;           // rattler_cache::default_cache_dir()
  rattler_cache::ensure_cache_dir(&cache_dir)?;
  let gateway = Gateway::builder()
      .with_cache_dir(cache_dir.join(rattler_cache::REPODATA_CACHE_DIR))
      .with_package_cache(PackageCache::new(cache_dir.join(rattler_cache::PACKAGE_CACHE_DIR)))
      // ...
      .finish();
  ```
  (`create.rs:154-186`, `exec.rs:227`, `solve.rs:152`, condensed). `ana`
  needs to follow this exact pattern for its `Gateway` construction — the
  bare `.finish()` default is not "the generic location," it's a narrower,
  env-var-blind fallback for callers who don't care.
- **`Installer::new().install(...)`'s own bare default, by contrast,
  already resolves correctly on its own.** `installer/mod.rs:643-655`
  (already cited under "The install/download pipeline") calls
  `crate::default_cache_dir()` — the real, env-var-honoring one — and joins
  `PACKAGE_CACHE_DIR`/`WHEEL_CACHE_DIR` itself when the caller doesn't
  supply `.with_package_cache(...)`/`.with_wheel_cache_dir(...)`. Confirmed
  by reading `rattler-bin/src/commands/create.rs:318-330`'s own
  `Installer::new()...install(...)` call: it sets `.with_download_client`,
  `.with_target_platform`, `.with_installed_packages`,
  `.with_execute_link_scripts`, `.with_requested_specs`, `.with_reporter` —
  and nothing cache-related at all, relying entirely on the built-in
  default, which lands at the identical path the same command's explicit
  `Gateway`/`PackageCache` construction two lines earlier computed by hand.
  **`ana` could rely on this default too and it would already be correct
  and generic** — the recommendation below to be explicit anyway is about
  centralizing `ana`'s own cache-path logic in one place for testability,
  not about correcting a rattler-side default that's actually fine here.

### The lock-granularity asymmetry this creates when the location is shared

Sharing the repodata cache and sharing the package cache across tools do
not carry the same risk, and it's worth being precise about which is which
now that both are being pointed at the same machine-wide location:

- **Repodata caching locks per cache key, not globally**: the fetch path
  behind `Gateway` takes a lock scoped to one specific channel-subdir's
  cache entry (`cache_path.join(format!("{cache_key}.lock"))`,
  `rattler_repodata_gateway/src/fetch/with_cache.rs:196-198`, using the
  shared/exclusive `LockedFile` primitive in `utils/flock.rs`) — `ana`
  fetching `conda-forge/linux-64` at the same moment `pixi` also fetches
  `conda-forge/linux-64` contend on that one entry; `ana` fetching
  `conda-forge/noarch` while `pixi` fetches `conda-forge/linux-64` don't
  contend at all. Sharing this cache location across tools is close to a
  free lunch.
- **Package caching locks the entire cache, for the entire install, per
  "The global cache lock is a mechanical consequence of the shared-cache
  decision, not a separate choice" below**: this is not
  per-entry, so pointing `ana` at the same package-cache root every other
  rattler-based tool on the machine defaults to means `ana`'s installs now
  contend with *those tools'* installs too, not just with `ana`'s own
  concurrent invocations. The section below was originally written about
  `ana`-internal concurrency; read it now as covering cross-tool
  concurrency as well, since this decision is what puts `ana` in that
  shared blast radius in the first place.

### What actually needs to change in `ana`'s own code

Checked against `ana`'s current state, not hypothetically — this is a real,
present-day gap, not a preemptive recommendation:

- **`ana`'s repodata cache does not currently use this convention at all.**
  `crates/ana/src/main.rs:20-23`'s `repodata_cache_dir()` derives its root
  from `ana_pypi_conda_map::cache_dir()` —
  `directories::ProjectDirs::from("", "", "ana")`
  (`ana-pypi-conda-map/src/cache_dir.rs:22-23`) — which is an
  **`ana`-branded** cache root (`~/Library/Caches/ana` on macOS,
  `~/.cache/ana` on Linux), not rattler's `~/Library/Caches/rattler/cache`/
  `~/.cache/rattler/cache`. The existing doc comment justifies this as
  "one shared root, one subdirectory per consumer, rather than this crate
  re-deriving its own `ProjectDirs` triple" — a reasonable thing to want,
  just aimed at the wrong shared root now that the actual decision is to
  match rattler's own tools, not to minimize the number of `ProjectDirs`
  calls inside `ana`. This needs to change to compute
  `rattler_cache::default_cache_dir()` instead, call
  `rattler_cache::ensure_cache_dir(&root)` once, and join
  `rattler_cache::REPODATA_CACHE_DIR` — i.e. adopt the `rattler-bin` pattern
  above verbatim. `ana-solver::Solver::new`'s signature (`cache_dir: PathBuf,
  root_dir: PathBuf`) does not need to change at all for this — it already
  takes the fully-resolved repodata-subdirectory path as a parameter; only
  what `main.rs` computes and passes in changes.
- **`ana_pypi_conda_map::cache_dir()` itself should NOT change.** The
  PyPI-name-to-conda-package-name mapping cache it manages is genuinely
  `ana`-specific data (an API response cache for a lookup no other
  rattler-based tool needs or would benefit from sharing) — keeping it under
  `ana`'s own `ProjectDirs` root is correct as-is. Only the repodata (and,
  once the installer crate exists, package/wheel) caches move to rattler's
  shared root; this one deliberately does not.
- **New workspace dependencies needed**: `rattler` (for `Installer`,
  `default_cache_dir`, and the `Transaction` type `sync_algorithm.md`
  already scoped) and `rattler_cache` (for `default_cache_dir`,
  `ensure_cache_dir`, and the three `*_CACHE_DIR` consts, used directly
  rather than only through `rattler`'s one-line re-export) — both pinned as
  git dependencies at the same fork `rev` as the four rattler crates already
  in `Cargo.toml`, per that file's existing convention.
- **The future `ana-installer` crate's `Downloader` (see "Suggested shape"
  below) should compute the shared root once and derive every subdirectory
  from it explicitly** — `PackageCache::new(root.join(PACKAGE_CACHE_DIR))`,
  `.with_wheel_cache_dir(root.join(WHEEL_CACHE_DIR))` — rather than omitting
  them and trusting `Installer`'s own (correct, per above) built-in
  defaults. Not required for correctness, since the built-in defaults
  already land at the same path; recommended so `ana`'s cache-path
  computation lives in exactly one place (testable, and ready for a future
  `ana`-level override if one is ever needed) instead of being split
  between "`ana` computes this explicitly" for repodata and "`ana` trusts
  rattler's internal default" for packages.
- **No new `ana`-specific cache-location env var or flag is recommended.**
  The whole point of this decision is to piggyback on the one, shared
  `$RATTLER_CACHE_DIR` convention every rattler-based tool already
  respects — including in contexts `ana` doesn't control, like a CI runner
  or sandbox that already points `$RATTLER_CACHE_DIR` at a scratch volume
  for other tools. Inventing a separate `ANA_CACHE_DIR` would mean `ana`
  silently ignores whatever cache-placement control the user already has
  for everything else. If a real need for `ana`-only isolation from other
  tools' caches ever shows up, that's a deliberate, separate feature to add
  later — not a default to build in now under the banner of "use it
  as-is."

## The global cache lock is a mechanical consequence of the shared-cache decision, not a separate choice

`PackageCache::acquire_global_lock` (`rattler_cache/src/package_cache/cache_lock.rs:126-160`)
is a single OS-level *exclusive* lock (`fs4`'s `file.lock()`, not
`lock_shared()`) on one file — `<cache root>/.cache.lock`, where "cache
root" is literally whatever path was passed to `PackageCache::new(...)` (or,
if none was, `rattler_cache::default_cache_dir().join(PACKAGE_CACHE_DIR)`).
`Installer::install` holds it for the *entire* call — from before the first
download starts to after the last file is linked
(`installer/mod.rs:659-662`, bound to `_global_cache_lock` which lives until
the function returns).

**This is not a knob separate from the cache-location decision — it is that
decision, mechanically.** The lock's path is derived from the cache's path;
there is no independent way to configure "one shared cache, but somehow
multiple locks for different sub-populations of it." Rattler's current
locking granularity is *the entire cache, for the entire install call*,
full stop — nothing finer exists to opt into today. So once "Cache location:
use rattler's generic location as-is" settled on **one** cache root, shared
across every environment in every project (and, deliberately, with every
other rattler-based tool on the machine), it settled this too:

**Any two `Installer::install()` calls that share that one cache root fully
serialize against each other, for the whole duration of the slower one,
regardless of which prefix each is installing into and regardless of how
disjoint their package sets are.** Concretely: `ana`'s default `.env/`
install and its `.ana/<hash>/env/` (`--group dev`) install for the *same*
project — two entirely separate prefix directories that never touch each
other — still contend on the exact same `.cache.lock`, because both look up
and populate packages from the same shared cache root, and that's the only
place the lock lives. `env_storage.md`'s per-environment advisory lock
(guarding two invocations against racing on the *same* `env_path`) is a
different resource and doesn't touch this at all — it was never meant to,
and nothing about it changes here.

This isn't rattler being sloppy: the lock exists because a cache entry's
staleness bookkeeping (a revision number + hash, in a plain file with no
lock of its own — see `cache_lock.rs`'s own doc comment) is only kept
consistent by having every reader and writer of the *whole* cache serialize
through one gate, and a narrower lock would have to solve a real
reader-vs-concurrent-invalidation race (a linking process reading files out
of an entry while another process deletes-and-rewrites that same entry) that
nothing in rattler solves today. It is a real, deliberate trade rattler
(and pixi, which lives with the identical limitation) makes: "reduces
overhead by avoiding per-package locking," at the cost of exactly the
serialization described above.

**There is nothing to decide here beyond what's already been decided.**
Given the shared-cache-location decision — which stays the right call, for
the disk and network savings it buys across every environment and every
rattler-based tool on the machine — accepting this serialization isn't a
separate policy choice `ana` is making; it's the direct, unavoidable
consequence of that first choice, given what rattler's `PackageCache`
supports today. The only way this stops being true is a **rattler-side**
change — finer-grained (e.g. shared-read + per-entry) locking inside
`rattler_cache::PackageCache` itself, replacing the single whole-cache
exclusive lock with something closer to the per-`BucketKey` in-process
mutex that already exists for de-duplication, extended to be cross-process
— and that's core-module surgery with no timeline, not a config flag `ana`
can flip. Nothing in `ana`'s own design should assume that fix exists yet.

**This is pre-existing and entirely orthogonal to wheels** — a direct
review of the in-progress wheel-cache fork branch called this out
independently as "`ana`'s biggest throughput problem," not a wheel-specific
one: every wheel operation added to a transaction is more work done inside
the same already-coarse lock, not a new kind of contention.

**What this means concretely for anything `ana` builds that installs more
than one environment at a time** (an agent running `ana run --group dev`
and `ana run --group doc` concurrently against the same checkout, or a CI
matrix materializing several `.ana/<hash>/env`s at once): the *solve* half
of reconciling each environment (steps 1-2 of `sync_algorithm.md`) can run
fully in parallel today, for free — it only touches the `Gateway`'s
repodata cache, which locks per channel-subdir, not globally (see "Cache
location" above). The *install* half cannot, today, under the cache-location
decision already made. Any scheduler `ana` builds should assume that,
rather than discover it: parallelizing environment installs buys real
wall-clock savings on the solve step and none at all on the download/link
step, for as long as rattler's locking stays this coarse.

## Decisions `ana` needs to make before building an installer

These are genuinely `ana`'s to decide — not answerable from rattler/pixi
source, because they're about what `ana`'s environments are for, not how
rattler moves bytes. Surfaced here because each one changes what "done"
looks like for the installer work this doc is otherwise scoping.

- **Cache GC/eviction ownership — safety resolved, so this doesn't need
  building now.** `rattler_cache` has no TTL, LRU, atime tracking, prune
  API, or size-query API for *either* the conda or the wheel package
  cache — `CacheIndex` only records `{sha256, filter}` per entry, not
  enough to build eviction on top of without independently walking the
  cache directories. That's still true, but it's no longer a blocker,
  because the question that actually mattered — *is it ever unsafe to
  delete a cache entry?* — has a clean answer: **no, as long as the
  deletion happens under `PackageCache::acquire_global_lock()` on the same
  cache root**, the exact same lock `Installer::install()` already takes.
  Verified two ways, not assumed: (a) grepped the fork for every reader of
  `PrefixRecord.extracted_package_dir`/`.link.source` (the fields that
  record which cache path a package was linked from) and found they are
  written once, at install time, and never read again anywhere in the
  fork — nothing reaches back into a cache entry for an
  already-materialized environment; (b) linking defaults to hardlinks or
  reflinks (`allow_hard_links`/`allow_ref_links`, both "default to `true`
  if unset") whenever the filesystem supports them, falling back to a
  plain copy otherwise — in every case, once `install()` returns, the
  installed environment's own copy is already independent of the cache
  entry's continued existence (a hardlink's inode persists as long as any
  link to it exists, including the one now in the prefix; a reflink or
  copy was never sharing lifetime with the cache to begin with). So a
  completed install has zero ongoing dependency on the cache entry it came
  from — the only reason to keep an entry around is to avoid re-downloading
  it for a *future* install, never to protect a past one. This applies
  uniformly to `pkgs/` and `wheel-pkgs/`, both before and after the pending
  wheel-cache fixes, since wheel population already happens entirely inside
  the span where `Installer::install()` holds the conda cache's global
  lock today.

  **Consequence: no size-tracking or eviction machinery needs to ship with
  the installer work in this doc.** The only thing GC ever needed from
  `ana` ahead of time was a safety argument, and that's now settled without
  writing any code — a future pruning tool can be as simple as "acquire
  the lock, delete whatever an mtime/atime-based (or any other) heuristic
  picks, release the lock," built whenever cache size actually becomes a
  measured problem rather than pre-emptively. The one thing worth
  remembering when that day comes: a GC pass held under this lock
  serializes against concurrent installs exactly like two installs
  serialize against each other (per "The global cache lock is a mechanical
  consequence of the shared-cache decision, not a separate choice") — it's
  a third kind of participant in the same, already-accepted trade, not a
  new risk to weigh. The wheel-cache layout-stability caveat elsewhere in
  this list (needing the extraction-format question settled before
  building anything that reads the wheel cache's on-disk layout directly)
  still applies to a *pruning tool's implementation* once one exists — it
  just doesn't block anything today, since nothing is being built yet.

## Suggested shape for a new `ana-installer` crate

Not a full design — just where the pieces above land, to make the
recommendations concrete enough to start from:

```rust
// Built once per `ana` invocation, shared across every environment that
// invocation reconciles (default env + any --group/--extra selections).
// `root` is `rattler_cache::default_cache_dir()` — the same location
// pixi/rattler-build/rattler-bin already use — computed and
// `ensure_cache_dir`-ed exactly once, in one place (see "Cache location:
// use rattler's generic location as-is"), not re-derived per subsystem.
struct Downloader {
    client: rattler_networking::LazyClient,   // retry middleware, see below
    package_cache: rattler_cache::package_cache::PackageCache,
    wheel_cache_dir: std::path::PathBuf,
    repodata_cache_dir: std::path::PathBuf,   // handed to `ana-solver::Solver::new`
}

impl Downloader {
    fn new(root: &Path) -> std::io::Result<Self> {
        rattler_cache::ensure_cache_dir(root)?;
        let client = LazyClient::new(|| {
            ClientWithMiddleware::builder(Client::builder().build().expect("..."))
                .with(RetryTransientMiddleware::new_with_policy(
                    rattler_networking::retry_policies::default_retry_policy(),
                ))
                .build()
        });
        let package_cache = PackageCache::new(root.join(rattler_cache::PACKAGE_CACHE_DIR));
        let wheel_cache_dir = root.join(rattler_cache::WHEEL_CACHE_DIR);
        let repodata_cache_dir = root.join(rattler_cache::REPODATA_CACHE_DIR);
        Ok(Self { client, package_cache, wheel_cache_dir, repodata_cache_dir })
    }
}
```

- Pass the *same* `client` into `ana-solver`'s `Gateway::builder().with_client(...)`
  (closing the retry-middleware gap on the solve side too) and into
  `Installer::new().with_download_client(...)` — one client, one retry
  policy, for both repodata and package-artifact fetches.
- Pass `repodata_cache_dir` into `ana-solver::Solver::new`'s existing
  `cache_dir` parameter — replacing today's `ana_pypi_conda_map`-borrowed
  root, per "Cache location" above; `Solver::new`'s signature is unchanged.
- `Installer::new().with_target_platform(..).with_download_client(..)
  .with_package_cache(..).with_wheel_cache_dir(..).with_io_concurrency_semaphore(Arc::new(Semaphore::new(N)))`
  — deliberately no `.with_max_concurrent_requests(...)` call, per
  recommendation 2, and both cache paths passed explicitly rather than left
  to `Installer`'s own (already-correct, per "Cache location" above)
  built-in defaults.
- Superseded (2026-08): the fingerprint-check-before-`Installer` bullet
  this line originally sketched is now `ana-lockfile`'s env lock,
  compared by the caller before it decides whether to call `reconcile` at
  all -- see `investigations/env_state_implementation_plan.md`.
- `wheel_cache_dir` is wired up here unconditionally, but **do not point
  `ana` at wheel-origin records in any real environment until the go/no-go
  gate above (double-extraction fix + hash verification) is confirmed
  landed** — this is the one place in this sketch where "wire it up" and
  "turn it on" are genuinely different steps.


## Open questions this doc surfaces, not yet answered

- Tracking, not deciding: when does the in-progress wheel-cache branch (the
  double-extraction fix, hash verification, shared concurrency semaphore,
  `ValidationMode` parity) land, and does its landing get checked against
  the go/no-go gate in "Decisions `ana` needs to make" before `ana` turns on
  wheel installs anywhere real users see them.
- Whether `ana` wants a configurable download-concurrency cap at all (the
  `rattler_config` crate's `ConcurrencyConfig` — `solves`/`downloads`,
  defaults CPU-count/50 — is already available transitively and is a cheap
  way to expose one later) versus staying uncapped indefinitely like pixi.
  Recommendation 2 above says "don't add it now"; this is "leave the door
  open," not "never." (Once the wheel-cache fixes land, this cap will
  finally apply uniformly to wheels and conda packages alike, per the
  now-corrected R2.1 in the revised "What's missing" section — it was
  silently conda-only before.)
- Of the items in "Decisions `ana` needs to make before building an
  installer": pip-interoperability is the one still genuinely open — an
  `ana` product call, not something further source-reading resolves.
  Concurrent-multi-install strategy turned out not to be a separate
  decision at all (struck through there). Cache GC's safety question is
  resolved (see that bullet); only the pruning *policy/heuristic* is
  deferred, and deferred deliberately, not left open by oversight.

## Amendment history

- **2026-08 — fingerprint+lock short-circuit (recommendation 5) superseded
  by `ana-lockfile`'s env lock.** `investigations/env_state_implementation_plan.md`
  replaces the `.ana-install-marker` fingerprint/interruption scheme this
  doc recommended and `package_download_and_install_implementation_plan.md`
  built with a plain `packages`-equality check against
  `ana-lockfile`'s env lock (`<env_path>/ana.lock`), decided by the caller
  (`ana::run_command`) rather than short-circuited inside
  `ana_installer::reconcile` itself; the env lock's `dirty` bit (set
  before a reconcile starts, cleared after it succeeds) replaces
  `Interrupted`/`Fresh`/`Installed`, and a `dirty` env lock now wipes
  `env_path` recursively instead of forcing every package through
  `Installer::with_reinstall_packages`. See that plan for the current
  algorithm; the "Pixi's fingerprint + lock short-circuit" section above
  is kept as the verified research record of pixi's actual mechanism, not
  as a description of what `ana` implements today.
- **2026-08 — wheel-cache section revised against an in-progress fork
  branch.** The original version of this doc characterized the wheel path
  as missing two mechanical protections (a download-concurrency-semaphore
  bypass and a cross-process lock) relative to the conda path, and
  recommended filing both as upstream fork issues. A direct review of an
  actual in-progress fork branch (not yet merged, going further than the
  pinned base rev this doc was otherwise checked against) found: (a) two
  real bugs this doc had missed entirely — wheels are downloaded and
  extracted **twice** per install, and have **zero** hash verification
  against repodata; (b) the "no cross-process lock" claim overstated a real
  gap — `Installer::install()`'s existing whole-call exclusive lock already
  incidentally covers wheel population, as long as all wheel fetches
  continue to happen only via that call; (c) `rattler_wheel_cache_parity_spec.md`'s
  R5 (a collision-avoidance refactor) targets a collision that structurally
  cannot occur given the current separate-directories-plus-`_whl`-suffix
  layout, and that spec's step 5 (relocating the global lock file up one
  directory) would have broken mutual exclusion between old- and
  new-rattler processes during any rollout — both are retracted. See
  `rattler_wheel_cache_parity_spec.md`'s own amendment note for the
  corresponding correction on the rattler-fork-spec side. Recommendation
  numbering in the summary above was renumbered and expanded (three new
  items: tracking the branch instead of filing fresh issues, a
  pip-interoperability decision, and a cache-GC-ownership decision) rather
  than kept stable across this revision, since the original numbering no
  longer matched the underlying facts closely enough to preserve.
- **2026-08 — cache-location decision made: rattler's generic location,
  as-is.** Recommendation 6 originally framed cache placement as an open
  choice between a machine-wide cache and per-environment caches, without
  saying *which* machine-wide location. It's now settled: `ana` uses
  `rattler_cache::default_cache_dir()` (the same location `pixi`,
  `rattler-build`, and `rattler-bin` already default to) rather than an
  `ana`-branded `ProjectDirs` root, for both the repodata cache and the
  future package/wheel cache. This surfaced a real, present-day gap:
  `ana`'s repodata cache (`crates/ana/src/main.rs`'s `repodata_cache_dir`)
  currently borrows `ana_pypi_conda_map`'s own `ProjectDirs`-based root
  instead, which is neither `rattler`'s generic location nor (despite the
  removed doc comment's reasoning) actually the same concern as that
  crate's own cache. See the new "Cache location: use rattler's generic
  location as-is" section for the verified mechanics (the `Gateway`-vs-
  `Installer` default-resolution asymmetry, confirmed against `rattler-bin`;
  the fine-grained-vs-global lock-granularity asymmetry this decision's
  cross-tool sharing exposes) and the concrete list of what changes in
  `ana`'s own code.
- **2026-08 — sharded-repodata requirement decided, section simplified.**
  What was "The wheel-visibility gap," framed as an open, blocking risk
  ("does the channel publish sharded repodata? if not, the fork needs a
  change"), is now "Wheel visibility requires sharded repodata": `ana`
  simply requires any wheel-serving channel to publish sharded (CEP-16)
  repodata, full stop, rather than asking the fork to make the non-sharded
  path see wheels too. The underlying technical facts (only the sharded
  subdir client reads `v3.whl`; the fork change to fix the other two paths
  would be small but is now unnecessary) are unchanged from the original
  finding — only the framing and the corresponding "open question"/
  recommendation-4 entries were simplified to match the decision.
- **2026-08 — "three options" framing for concurrent installs retracted;
  clarified as a mechanical consequence, not a decision.** Earlier
  revisions presented "accept serialization / isolate caches / push
  upstream for finer locking" as three options for `ana` to weigh, both in
  "The global cache lock..." and in "Decisions `ana` needs to make." That
  framing was confusing and, on reflection, wrong: the lock's scope is
  *derived* from the cache root (`<cache root>/.cache.lock`), with no
  independent knob, so once "Cache location: use rattler's generic location
  as-is" settled on one shared root, it settled the lock too — "isolate
  caches" isn't an option sitting alongside that decision, it's *undoing*
  it, and "push upstream for finer locking" isn't an `ana`-side choice at
  all, it's a rattler change with no bearing on what `ana` does today. The
  section was rewritten to state this as the direct, already-decided
  consequence it is, and the "Decisions" list's now-defunct bullet was
  struck through rather than deleted outright, to keep the "we did
  consider this, and here's why it's not actually a separate decision"
  trail visible.
- **2026-08 — cache-deletion safety resolved; eviction/pruning explicitly
  deferred.** The cache-GC-ownership bullet in "Decisions `ana` needs to
  make" originally treated eviction as a capability gap `ana` would
  eventually need to fill itself or push upstream, with no safety argument
  attached. Verified directly (grepping every reader of
  `PrefixRecord.extracted_package_dir`/`.link.source`, and checking the
  default link-type behavior) that deleting a cache entry is safe — for
  already-completed installs unconditionally, and for anything
  concurrently in flight as long as the deletion happens under the same
  `PackageCache::acquire_global_lock()` every install already takes.
  Recommendation 9 and the cache-GC bullet were rewritten to state this,
  and to draw the conclusion it actually licenses: no size-tracking or
  eviction machinery needs to be built now, or alongside the installer
  work in this doc — pruning is a "build it later, off an mtime/atime (or
  other) heuristic, whenever cache size is a measured problem" item, not a
  design constraint on anything currently being planned. Only the pruning
  *policy* remains open, and deliberately so.
