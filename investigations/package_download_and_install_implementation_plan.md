# Implementation plan: real environment creation (conda + v3 wheels)

**2026-08 supersession notice**: this plan's Phase 4 "Fingerprint + lock
short-circuit" and part of "Lock-scope change" describe the
`.ana-install-marker` scheme this codebase originally implemented (and
this doc's phases below walk through building). That scheme has since
been superseded end to end by `investigations/env_state_implementation_plan.md`:
`ana-lockfile`'s env lock (`<env_path>/ana.lock`, a `dirty` bit plus the
last-reconciled section) replaces the marker file, `ana_installer::reconcile`
no longer short-circuits or tracks interruption at all (the caller decides
whether to call it, by comparing packages against the env lock), and a
`dirty` env lock now wipes `env_path` recursively rather than forcing a
`with_reinstall_packages` call. This doc otherwise remains an accurate
historical record of how Phases 0-3, 5, and 6 (and most of Phase 4) were
actually implemented; only the fingerprint/marker mechanics are stale
relative to current code.

Turns `investigations/package_download_and_install.md` into code. Written
after reading that doc plus `sync_algorithm.md`, `env_storage.md`,
`lock_file.md`, `lock_generation_algorithm.md`, the current state of every
crate under `crates/`, and the actual `intentionally-left-nil/rattler` fork
history (cloned to `/tmp/rattler-check`, HEAD `3b93372` as of this plan).

## Decisions already made (via the pre-planning Q&A)

1. **Enable wheel installs for real**, not just wired-but-inert. Rationale
   confirmed against the fork: `3b93372` (current `main`) is three commits
   past the pinned `66ffb3a` --
   `f06d239`/`66ffb3a` (wheel install support, already pinned) →
   `930b8ca` "route wheel install through PackageCache and fix cache/link
   bugs" (fixes the double-download/double-extraction bug, adds real hash
   verification, generalizes the shared concurrency semaphore and
   `ValidationMode` parity) →
   `3b93372` "rewrite wheel RECORD and write INSTALLER for pip
   interoperability" (implements decision 8 unconditionally, no opt-out).
   This is exactly the doc's "go/no-go gate," and it's satisfied by the rev
   bump this task already requires.
2. **ana-pypi-conda-map's HTTP client becomes async reqwest**, sharing the
   same retry-middleware client (`rattler_networking::LazyClient`) the
   installer/gateway use -- one client, one retry policy, process-wide.
3. **No live wheel-serving channel exists yet.** `ana`'s only wired channel
   (`repo.anaconda.com/pkgs/{main,r}`) is a classic Anaconda channel; I
   found no evidence it publishes sharded (CEP-16) repodata with `v3.whl`
   entries, and no other investigation doc names one. Wheel-install code
   is validated via fixtures/unit tests only -- see "Testing strategy."
4. **Build `ana-installer` and wire it into `ana run`** so environments are
   actually materialized on disk, not just plumbed as a library.
5. **`ana run` actually execs the command** now, replacing the current
   `println!("would run: ...")`.
6. **Build the per-`env_path` advisory lock + fingerprint short-circuit
   now**, not deferred.

## New finding surfaced during this planning pass -- needs your sign-off

None of the six decisions above are blocked by this, but it changes what
"wire up steps 3-4" actually requires, so flagging it before writing code
rather than discovering it mid-implementation.

**`ana.lock` cannot currently produce the `RepoDataRecord`s
`Transaction::from_current_and_desired`/`Installer::install` need.**
`sync_algorithm.md`'s own pseudocode assumes
`rattler_lock::Environment::conda_repodata_records(platform) ->
Vec<RepoDataRecord>` is available "for free" -- but `lock_file.md` decided
`ana.lock` is a **custom TOML format**, not `rattler_lock`'s, and per
`crates/ana-lockfile/src/lock_file.rs` and `crates/ana-lockfile/src/solver.rs`
(verified directly), what it actually stores end-to-end is a bare
`rattler_conda_types::PackageRecord` per package -- no `url`, no `channel`,
no archive filename/`identifier`. `ana_lockfile::Solver::solve()`'s trait
signature returns `Vec<PackageRecord>`, and `ana-solver`'s free `solve()`
function explicitly *discards* the `RepoDataRecord` wrapper
(`.map(|record| record.package_record)`) before handing results back across
that seam. `RepoDataRecord` (`rattler_conda_types/src/repo_data_record.rs`)
is `{ package_record: PackageRecord, identifier: DistArchiveIdentifier
(#[serde(rename="fn")]), url: Url, channel: Option<String> }` -- exactly the
three fields missing. Without them there is no way to re-fetch or verify a
locked package at install time, and no way to install a wheel-origin record
at all (its URL is not derivable from name/version/build the way a
conda-native archive's filename is).

This was very likely an ordering artifact, not a considered trade-off:
`ana-lockfile`'s `Solver` seam predates `rattler_solve`/`rattler_repodata_gateway`
being wired in at all ("no solver crate in the workspace yet" per
`solver.rs`'s own module doc), so it's unsurprising it settled on the
simplest type that existed at the time.

**Fix, and why it's low-risk:** `RepoDataRecord` already derives
`Serialize`/`Deserialize` (it's literally shaped like one `repodata.json`
package entry), and `lock_file.rs`'s TOML (de)serialization already goes
through a fully generic `serde_json::to_value`/`from_value` round-trip
(`package_to_table`/`table_to_json`), not hand-picked fields -- so widening
the stored type is a rename, not a rewrite:

- `ana_lockfile::Solver::solve()` returns `Vec<RepoDataRecord>`.
- `SolveRequest::preferred` becomes `&'a [RepoDataRecord]`.
- `PlatformSection::packages` becomes `Vec<RepoDataRecord>`.
- `ana-solver`'s `identity_key`/`available_by_identity`/`favored` logic
  keys off `record.package_record` instead of `record` directly, but is
  otherwise unchanged; it now returns real `RepoDataRecord`s instead of
  rebuilding them by re-matching against fetched candidates (I'm keeping
  the re-match-against-`available` step anyway, not just trusting the
  stored record blindly -- see "ana-solver changes" below for why).
- **No `LOCK_FILE_VERSION` bump** (per your steer: this is a redefinition
  of what one package entry holds, not a versioned migration). `version`
  stays `1`. Old on-disk `ana.lock` files simply fail `PackageRecord`'s
  (now `RepoDataRecord`'s) `serde_json::from_value` -- surfacing as the
  existing `Error::CorruptLock` path, not a new error variant. Acceptable
  pre-1.0/no external users; existing local `ana.lock`/`.ana/` state just
  needs deleting once after this change lands. `check_version`'s own logic
  (rejecting a lock *newer* than this binary understands) is untouched and
  irrelevant here -- this change isn't about the file being newer, it's
  about this binary now requiring fields older files never had.

This is in scope for this task -- without it, "install a resolved lock"
has no `url` to download from.

## Phase 0 -- rattler rev bump + new workspace deps

- Bump the `rev` on all four already-pinned crates
  (`rattler_conda_types`, `rattler_solve`, `rattler_repodata_gateway`,
  `rattler_virtual_packages`) from `66ffb3a7e4083629a7df8e6e41d6d54da037b742`
  to `3b933725de1aae88695e4ecd1e26de5242bbb8d2` (current fork `main` tip,
  confirmed reachable) in the root `Cargo.toml`. Update the doc comment
  above the pin to describe what the three new commits fix (the go/no-go
  gate, see above) instead of the stale "just wheel support" framing.
- Add two new workspace git deps at the same rev: `rattler` (for
  `Installer`, `Transaction`, `default_cache_dir`) and `rattler_cache` (for
  `default_cache_dir`, `ensure_cache_dir`, `PackageCache`, and the
  `PACKAGE_CACHE_DIR`/`WHEEL_CACHE_DIR`/`REPODATA_CACHE_DIR` consts).
  Verified all of this API surface exists at `3b93372` exactly as
  `package_download_and_install.md` describes (checked
  `crates/rattler/src/install/installer/mod.rs`'s builder methods and
  `crates/rattler_cache/src/lib.rs`/`consts.rs` directly).
- Add `rattler_networking` (for `LazyClient`,
  `retry_policies::default_retry_policy`) as a workspace dep, also pinned
  at the same rev.
- Add `reqwest`, `reqwest-middleware`, `reqwest-retry` as workspace deps
  **matching the fork's own pinned versions and package aliases exactly**,
  confirmed from the fork's root `Cargo.toml`:
  ```toml
  reqwest = { version = "0.13", default-features = false, features = ["json", "stream"] }
  reqwest-middleware = { package = "astral-reqwest-middleware", version = "0.5" }
  reqwest-retry = { package = "astral-reqwest-retry", version = "0.9" }
  ```
  Getting the package alias wrong (e.g. depending on plain `reqwest-middleware`
  from crates.io instead of the `astral-` fork) would give us a
  `ClientWithMiddleware` type that doesn't unify with `rattler_networking::LazyClient`'s
  -- this is the one part of this phase most likely to silently produce a
  confusing type error if skipped.
- Add `async-trait` as a workspace dep (needed for Phase 2's `dyn
  HttpClient` trait, which can't use native async-fn-in-trait and stay
  object-safe).
- `cargo update -p rattler_conda_types --precise ...` (or just `cargo build`
  once the `rev`s change) to regenerate `Cargo.lock`; commit it.
- Sanity check: `cargo build --workspace` after just this phase (no other
  code changes yet) should still succeed unmodified, since nothing consumes
  the new crates yet -- confirms the rev bump alone doesn't break
  `rattler_conda_types`'s API surface that `ana-marker-matchspec`/
  `ana-pep508-to-matchspec`/`ana-lockfile`/`ana-solver` already depend on.
  (Spot-checked: `PackageRecord`, `RepoDataRecord`, `Gateway`, `SolverTask`
  signatures are unchanged between `66ffb3a` and `3b93372` -- the three new
  commits only touch `rattler`/`rattler_conda_types::package::wheel`/
  `rattler_conda_types::prefix_record`, not the types those four crates
  already use.)

## Phase 1 -- lock schema + `Solver` seam widening

(The "New finding" above, executed.) Order matters: do this before Phase 4
so the installer crate never has to consume the narrower `PackageRecord`
shape at all.

- `crates/ana-lockfile/src/solver.rs`: `Solver::solve` returns
  `Vec<RepoDataRecord>`; `SolveRequest::preferred` becomes `&'a
  [RepoDataRecord]`. Update the module doc's rationale paragraph (currently
  explains why `preferred` borrows `PackageRecord`s -- same reasoning,
  new type).
- `crates/ana-lockfile/src/lock_file.rs`: `PlatformSection::packages: Vec<RepoDataRecord>`;
  `package_to_table`/`table_to_json`/`parse_section`'s
  `serde_json::from_value::<PackageRecord>` become `::<RepoDataRecord>`.
  `LOCK_FILE_VERSION` stays `1` -- this is a redefinition of what one
  package entry contains, not a new version to branch on; a pre-change
  `ana.lock` just fails today's existing `Error::CorruptLock` path on
  read, same as any other malformed file. Update the module doc's TOML
  example to show `fn`/`url`/`channel` alongside the existing package
  fields.
- `crates/ana-lockfile/src/algorithm.rs`, `cache.rs`: check every place that
  currently takes `&[PackageRecord]`/`Vec<PackageRecord>` for the previous
  section's packages or hash input -- `PlatformSection::hash()` already
  hashes via each record's own deterministic `Serialize`, so widening the
  type needs no algorithm change there, just the type update propagating
  through.
- `crates/ana-solver/src/lib.rs`: `identity_key` takes `&PackageRecord`
  still (unchanged signature) but callers pass `&record.package_record`
  where `record` is now a `RepoDataRecord`; `available_by_identity` stays
  keyed the same way; `favored` is built by matching `request.preferred`
  (now `&[RepoDataRecord]`) against `available_by_identity` **by identity
  key, not by returning the stored record directly** -- deliberately
  keeping the re-match-against-freshly-fetched-`available` step even though
  the stored record is now "complete enough" to use as-is, because a
  previously-locked record's URL can go stale (channel repodata patched,
  package pulled) in a way name/version/build alone wouldn't catch; always
  prefer the freshly-fetched `RepoDataRecord` for the same identity over
  the one carried in from the lock. Update the module's final step ("5.
  Unwrap each winning `RepoDataRecord` back down to its `PackageRecord`")
  to describe returning `RepoDataRecord`s directly instead.
- Run every existing test in `ana-lockfile`/`ana-solver`/`ana`'s
  `run.rs` (`FakeSolver`/`CountingSolver`/etc. in tests already construct
  bare `PackageRecord`s) -- update those fixtures to build a minimal
  `RepoDataRecord` (e.g. `identifier` from the record's own
  name-version-build, a `file://`-scheme placeholder `url`, `channel:
  None`) instead. This is mechanical but touches most of `run.rs`'s and
  `algorithm.rs`'s existing test modules.

## Phase 2 -- ureq → reqwest for `ana-pypi-conda-map`

Verified first: `ana_pypi_conda_map::load` is not actually called from any
production code path yet (only `ana_pypi_conda_map::cache_dir` is, from
`main.rs`; `load`'s only callers today are its own tests). This lowers the
blast radius of this change considerably -- there is no existing sync call
chain elsewhere in the binary to preserve compatibility with.

- `crates/ana-pypi-conda-map/src/http.rs`: `HttpClient` trait becomes
  `#[async_trait::async_trait] pub(crate) trait HttpClient: Send + Sync`
  with `async fn head(...)`/`async fn get(...)` (object-safety for the
  existing `&dyn HttpClient`/`Arc<dyn HttpClient>` usage requires the
  macro, not native async-fn-in-trait). `UreqHttpClient` is replaced by a
  `ReqwestHttpClient` wrapping a `reqwest_middleware::ClientWithMiddleware`
  (concretely, a `rattler_networking::LazyClient` handed in from the
  caller -- see below), translating conditional `If-None-Match`/
  `If-Modified-Since` headers and status codes the same way the current
  `ureq` impl does. Same short timeouts
  (`CONNECT_TIMEOUT`/`OVERALL_TIMEOUT`) via `reqwest::Client::builder()`'s
  `connect_timeout`/`timeout`.
- `crates/ana-pypi-conda-map/src/refresh.rs`: `perform_refresh` and its
  internal `HttpClient` calls become `async`. `FakeHttpClient` in its test
  module gets the same `#[async_trait]` treatment (mechanical).
- `crates/ana-pypi-conda-map/src/load.rs`: `load()`'s public signature
  changes from a bare sync fn to one that takes a
  `tokio::runtime::Handle` and a `rattler_networking::LazyClient` (both
  supplied by the caller, per "share one client/runtime across the
  process" -- see Phase 5). Internally:
  - `Action::UseCached` -- unchanged, no I/O.
  - `Action::BlockingRefresh` -- `handle.block_on(refresh::perform_refresh(...))`
    instead of a direct sync call.
  - `Action::UseCachedAndRefreshInBackground` -- the spawned
    `std::thread` now does `handle.block_on(refresh::perform_refresh(...))`
    inside the thread body (the `Handle` is `Clone + Send + 'static`,
    so this is a direct substitution, not a redesign); the thread itself
    stays a real OS thread, not a tokio task, so `MappingHandle::finish`'s
    "join a `JoinHandle`" contract is unchanged.
  - This keeps `load()`'s *return type* and blocking/background-thread
    behavior exactly as documented in `pypi_conda_map.md` -- only the
    transport underneath changes, plus two new required parameters.
- No change to `ana_pypi_conda_map::cache_dir()` (unaffected; still its own
  `ProjectDirs` root, per the investigation doc's explicit "should NOT
  change" call).
- `ana-pypi-conda-map/Cargo.toml`: drop `ureq`, add `reqwest`,
  `reqwest-middleware` (workspace versions from Phase 0),
  `rattler_networking` (for `LazyClient`), `tokio` (for `Handle`),
  `async-trait`.
- Root `Cargo.toml`: remove the `ureq` dependency declaration and its
  explanatory comment (the whole point of that comment -- avoiding
  pulling in tokio -- is moot now that `rattler_repodata_gateway`/`rattler`
  already pull in the full tokio/reqwest stack unconditionally).
- Since nothing calls `ana_pypi_conda_map::load` from production code yet,
  this phase is self-contained and independently testable/mergeable before
  touching `ana-installer` at all.

## Phase 3 -- cache location

- `crates/ana/src/main.rs`: replace `repodata_cache_dir()`'s current body
  (which derives from `ana_pypi_conda_map::cache_dir()`) with the
  `rattler-bin` pattern, verified directly against
  `rattler-bin/src/commands/create.rs`:
  ```rust
  let root = rattler_cache::default_cache_dir()?;
  rattler_cache::ensure_cache_dir(&root)?;
  let repodata_cache_dir = root.join(rattler_cache::REPODATA_CACHE_DIR);
  ```
  `RattlerSolver::new`'s signature (`cache_dir: PathBuf, root_dir: PathBuf`)
  does not change -- only what `main.rs` computes and passes in.
- This root (`rattler_cache::default_cache_dir()`, i.e.
  `$RATTLER_CACHE_DIR` or `~/Library/Caches/rattler/cache` /
  `~/.cache/rattler/cache`) is computed **once** in `main.rs` and reused
  for `PACKAGE_CACHE_DIR`/`WHEEL_CACHE_DIR` too, via the new
  `ana-installer::Downloader` (Phase 4) -- not re-derived per subsystem.
- No change to `ana_pypi_conda_map::cache_dir()` (confirmed out of scope by
  the doc; it's a genuinely `ana`-specific API-response cache, not
  something other rattler-based tools would ever share).

## Phase 4 -- `ana-installer` crate

New crate, `crates/ana-installer`, depending on: `ana-fs-util` (reuse
`AdvisoryLock`/`write_atomic`, not a new locking primitive), `ana-paths`
(for `EnvironmentPaths`), `rattler`, `rattler_cache`, `rattler_conda_types`,
`rattler_networking`, `tokio`, `thiserror`, `sha2` (fingerprint hashing --
actually `xxh3`, see below), `xxhash-rust` (new workspace dep, `xxh3`
feature -- pixi's own choice per the doc, cheap non-cryptographic hash for
a cache key, not a security digest).

### `Downloader`

```rust
pub struct Downloader {
    client: rattler_networking::LazyClient,
    package_cache: rattler_cache::package_cache::PackageCache,
    wheel_cache_dir: PathBuf,
    io_concurrency_semaphore: Arc<tokio::sync::Semaphore>,
}
```

- `Downloader::new(root: &Path) -> io::Result<Self>`: `ensure_cache_dir(root)`,
  build the client as `LazyClient::new(|| ClientBuilder::new(Client::builder()
  .user_agent(concat!("ana/", env!("CARGO_PKG_VERSION"))).build()?)
  .with(RetryTransientMiddleware::new_with_policy(default_retry_policy()))
  .build())` (matches the doc's "Suggested shape" and recommendation 1;
  deliberately narrower than `rattler-bin`'s own reference client, which
  has no retry middleware at all -- confirmed by reading
  `rattler-bin/src/commands/client.rs` directly -- and narrower than
  pixi's, which also adds offline/mirror/OCI/S3/GCS/auth-challenge
  middleware `ana` has no channel-configuration story for yet per
  `ana-solver`'s own "No real channel configuration" TODO).
  `PackageCache::new(root.join(PACKAGE_CACHE_DIR))`, `wheel_cache_dir =
  root.join(WHEEL_CACHE_DIR)`. `io_concurrency_semaphore` explicit at
  rattler's own default (100), per recommendation 3 -- not left to
  `Installer`'s built-in default, for the same "one place, testable"
  reason the doc gives for the cache paths.
- Exposes `.client()` (handed to `ana-solver::RattlerSolver` and
  `ana_pypi_conda_map::load`, Phase 5) and a method that builds a
  pre-configured `Installer` for one call:
  ```rust
  fn installer(&self, platform: Platform) -> Installer {
      Installer::new()
          .with_download_client(self.client.clone())
          .with_package_cache(self.package_cache.clone())
          .with_wheel_cache_dir(&self.wheel_cache_dir)
          .with_io_concurrency_semaphore(self.io_concurrency_semaphore.clone())
          .with_target_platform(platform)
          .with_execute_link_scripts(true)
          // deliberately no .with_max_concurrent_requests/
          // .with_concurrent_requests_semaphore -- recommendation 2.
  }
  ```

### Fingerprint + lock short-circuit (superseded 2026-08 -- see the notice at the top of this doc)

Pixi's exact algorithm (doc's "Pixi's fingerprint + lock short-circuit,
exact mechanics"), adapted to reuse `ana`'s existing per-`env_path`
advisory lock rather than a second lock:

- `fingerprint(records: &[RepoDataRecord]) -> String`: `xxh3` over
  `(name.as_normalized(), sha256)` per record, sorted by name, formatted as
  16 lowercase hex chars -- exactly pixi's rule, ported directly.
- Marker file: `<env_path>/.ana-install-marker` (sibling to the
  `conda-meta` directory `rattler` itself manages -- **not** inside
  `conda-meta`, so a future `rattler`-internal cleanup of that directory
  can never delete `ana`'s own bookkeeping). Three states, but stored as
  small JSON via `ana_fs_util::write_atomic` rather than pixi's fixed
  16-byte in-place write: `write_atomic` is already crash-safe
  (tempfile-in-same-dir + rename -- a crash mid-write leaves the *previous*
  complete file, never a torn one), so `ana` doesn't need pixi's
  specific fixed-width trick, which exists there to support a *lock-free*
  peek reader `ana` has no equivalent of (every reader here already holds
  the same advisory lock).
  - No file / unreadable → `Fresh`.
  - `{"state": "installing"}` → `Interrupted` (written *before* the real
    install starts; a crash between that write and the matching `Installed`
    write leaves this in place. Since the write itself is atomic-rename,
    the only way to observe `Interrupted` is a crash *after* the marker
    swap and *before or during* the install that followed it -- exactly
    the case pixi's `was_interrupted()` exists to catch).
  - `{"state": "installed", "fingerprint": "<16 hex>"}` → `Installed(fp)`.
- `reconcile(lock: &EnvironmentLockGuard, downloader: &Downloader, paths: &EnvironmentPaths, platform: Platform, desired: Vec<RepoDataRecord>, mode: ReconcileMode) -> Result<ReconcileOutcome, Error>`
  (`ReconcileMode` is `Exact | Inexact`, per `sync_algorithm.md`'s existing
  decision -- `Inexact` computes `ignored = names(current) − names(desired)`
  and passes it to `Installer::with_ignored_packages`; this needs
  `current`, i.e. a `PrefixRecord::collect_minimal_from_prefix` read, which
  happens regardless since it's also needed for the fingerprint's
  `Interrupted`-forces-reinstall path):
  1. Compute `fingerprint(&desired)`.
  2. Read the marker. If `Installed(fp)` and `fp == fingerprint(&desired)`
     and mode allows skipping (no forced reinstall), return
     `ReconcileOutcome::Unchanged` without touching `conda-meta`, building a
     `Transaction`, or calling `Installer` at all.
  3. If `Interrupted`, force every name in `desired` into
     `Installer::with_reinstall_packages(...)` (don't trust a prefix a
     crashed process may have left half-written).
  4. Write the `installing` sentinel (atomic-rename).
  5. Build and run the `Installer` (`downloader.installer(platform)`, plus
     `.with_ignored_packages(...)` for `Inexact` mode, plus
     `.with_reinstall_packages(...)` if `Interrupted`), `.install(&paths.env_path, desired).await?`.
  6. On success, write `{"state": "installed", "fingerprint": ...}`
     (best-effort per the doc -- a failed write here just costs the next
     invocation one extra reinstall, not correctness. `write_atomic`
     already returns `io::Result`; log-and-ignore rather than propagating).
  7. Return `ReconcileOutcome::Applied(InstallationResult)` (the transaction
     summary, for future `--diff`-style reporting per `sync_algorithm.md`'s
     one open, explicitly-deferred question).
- `reconcile` is `async fn` (it calls `Installer::install`, itself async);
  the caller (`ana::run_command`, Phase 5) already has a shared
  `tokio::runtime::Handle` to drive it from a sync context, same pattern as
  `ana-solver`.

### Lock-scope change in `ana-lockfile` (required for "layered inside the
existing lock, not a second one")

`ensure_current_platform` today opens and releases `paths.advisory_lock_path()`
entirely internally (`algorithm.rs`), returning before the caller could
extend the critical section. To hold one continuous lock across steps 1-4
(`ana_reconcile`'s pseudocode is written as a single function precisely
because of this), split lock acquisition out of `ana-lockfile`'s API:

- `ana-lockfile` gains `pub struct EnvironmentLockGuard<'a>` (thin wrapper
  around the `AdvisoryLock`/`RwLockWriteGuard` machinery already in
  `ana-fs-util`, re-exported as proof-of-possession) and `pub fn
  acquire_environment_lock(paths: &EnvironmentPaths) -> Result<EnvironmentLockGuard<'_>, Error>`.
- The existing `ensure_current_platform(project, paths, groups, platform, solver)`
  becomes a thin wrapper: acquire, delegate to a new `pub fn
  ensure_current_platform_locked(_guard: &EnvironmentLockGuard<'_>, project, paths, groups, platform, solver) -> Result<EnsureOutcome, Error>`
  (the actual, unchanged logic), release. `lock_platform`/`check` get the
  same treatment for consistency, though only `ensure_current_platform`'s
  locked variant is actually needed by `run_command`.
- `ana::run_command` (Phase 5) calls `acquire_environment_lock` once,
  passes the guard into `ensure_current_platform_locked`, then into
  `ana_installer::reconcile`, then drops it -- one lock, held from before
  the stage-1 cache check through the post-install fingerprint write,
  exactly matching `sync_algorithm.md`'s single-function `ana_reconcile`
  sketch and the doc's recommendation 5.
- This is a small, mechanical refactor (extract lock acquisition from
  three existing functions into a shared entry point) -- not a rewrite of
  `ana-lockfile`'s actual algorithm, which is untouched.

## Phase 5 -- wiring into the `ana` binary

- `crates/ana/src/main.rs`: build one process-wide `tokio::runtime::Runtime`
  here (not inside `ana-solver` anymore -- `RattlerSolver` currently owns
  its own; now that `ana-pypi-conda-map` and `ana-installer` both need
  async too, one shared runtime avoids three separate thread pools in one
  process). Build one `ana_installer::Downloader` from the shared cache
  root (Phase 3). Construct `RattlerSolver::new` with the runtime's
  `Handle` and `downloader.client()` instead of building its own client
  and runtime internally (`ana-solver`'s `Gateway::builder()` call gains
  `.with_client(downloader.client().clone())`, closing the "Gap:
  `ana-solver` currently has no retry middleware at all" finding).
- `crates/ana-solver/src/lib.rs`: `RattlerSolver::new` signature changes
  from `(cache_dir: PathBuf, root_dir: PathBuf)` to additionally take a
  `tokio::runtime::Handle` and a `rattler_networking::LazyClient`; drop its
  internal `tokio::runtime::Builder::new_multi_thread()...build()` call;
  `solve()` becomes `self.runtime_handle.block_on(...)` instead of
  `self.runtime.block_on(...)`.
- `crates/ana/src/run.rs`: `run_command` gains a `runtime: &tokio::runtime::Handle`
  and `downloader: &ana_installer::Downloader` parameter (both owned by
  `main.rs`, passed down -- `run_command` itself stays synchronous at its
  outer boundary, matching every other seam in this codebase). New body,
  replacing today's "compute paths, ensure lock, print command":
  1. `let guard = ana_lockfile::acquire_environment_lock(&paths)?;`
  2. `let ensure = ana_lockfile::ensure_current_platform_locked(&guard, &project, &paths, groups, platform, solver)?;`
  3. Read `desired: Vec<RepoDataRecord>` for `platform` out of the
     now-current `paths.lock_path` (a new small helper --
     `read_lock_section(&paths.lock_path, platform)?.packages`, reusing
     `lock_file.rs`'s existing parser, now returning `RepoDataRecord`s per
     Phase 1).
  4. `let outcome = runtime.block_on(ana_installer::reconcile(&guard, downloader, &paths, platform, desired, ReconcileMode::Inexact))?;`
     (`Inexact` is `ana run`'s documented default per `sync_algorithm.md`;
     nothing here changes that decision, just finally implements it).
  5. Drop `guard` (end of scope) -- lock released before exec.
  6. Exec: prepend `paths.env_path.join("bin")` (or, on Windows,
     `paths.env_path` itself plus `paths.env_path.join("Scripts")`) to
     `PATH`; on Unix, `std::os::unix::process::CommandExt::exec` (replaces
     the current process image, preserving signal/exit-code behavior the
     way `uv run`/`pixi run` do); on Windows, spawn + wait +
     `std::process::exit(status.code)` (no `exec` syscall equivalent).
     **Deliberately not** running any activation script (`conda activate`'s
     full environment-variable/hook machinery) -- out of scope for this
     doc, which only covers steps 3-4 (materializing the prefix), not
     shell activation; a `PATH`-prepend is the minimum needed to make
     `ana run python ...` actually find the installed interpreter, and is
     the same minimal approach `uv run` uses for its own venvs.
- `RunOutcome` changes from `{ ensure, command }` (a command to print) to
  something that reflects "the command already ran" -- likely just
  removing `RunOutcome`/`shell_join` from the non-exec path entirely and
  letting `main.rs`'s `match` on `EnsureOutcome` become a `eprintln!`
  logged *before* the exec (since exec never returns on success, anything
  after it in `main` only runs on exec failure/Windows's post-wait path).
- Every existing test in `run.rs` that asserts on `RunOutcome::command`/
  `shell_join` needs rework once exec replaces printing -- tests move to
  asserting `PATH`/prefix contents and exit codes via a real (but tiny,
  fixture-backed) install rather than string-matching a printed command.
  `shell_join`/`shell_quote` likely still earn their keep for error
  messages (e.g. "the command that failed was: ...") even once they're no
  longer the primary output.

## Phase 6 -- concurrency/retry configuration

Already folded into `Downloader::new` (Phase 4) and `main.rs`
(Phase 5), listed separately only because the doc calls these out as
their own numbered recommendations:

- No `Installer::with_max_concurrent_requests`/`with_concurrent_requests_semaphore`
  call anywhere (recommendation 2).
- `Gateway::builder().with_client(...)` and
  `Installer::new().with_download_client(...)` both receive the *same*
  `Downloader::client()` (recommendation 1 + the "Suggested shape"'s "one
  client, one retry policy, for both repodata and package-artifact
  fetches").
- `io_concurrency_semaphore` set explicitly to `Arc::new(Semaphore::new(100))`
  (rattler's own default value, made explicit per recommendation 3 --
  not a different number, just no longer implicit).
- No `ana`-specific cache-location env var/flag added (recommendation 6's
  explicit "don't invent one" -- `$RATTLER_CACHE_DIR` is the only knob).

## Testing strategy

No live wheel-serving (or even conda-serving) channel is assumed reachable
in tests or CI. Everything below is `file://`-backed:

- **Fixture channel(s)**: a small `tests/fixtures/` tree per crate that
  needs one, each holding a minimal `noarch/repodata.json` (or, for the
  wheel-path tests, a sharded/CEP-16-shaped index -- checked exactly what
  `sharded_subdir` expects on disk in `/tmp/rattler-check`'s own test data
  before building this) plus 1-2 genuinely tiny archives (`.conda` for a
  no-op conda package with an empty payload; a hand-built trivial `.whl`
  with one `.py` file for the wheel path) so `Installer::install` has
  something real to extract, hash-verify, and link. `rattler`'s own repo
  has test fixtures shaped exactly like this (used by its `sharded_subdir`/
  `PackageCache` unit tests) -- worth checking whether any are small and
  license-clean enough to copy in rather than hand-rolling from scratch,
  before committing to fully bespoke fixtures.
- **`ana-installer` unit tests**: `reconcile` against a temp `env_path` and
  the fixture channel -- first call installs (asserts `conda-meta`
  populated, marker file written); second call with the same `desired`
  short-circuits (asserts no `Installer` work happens -- e.g. by pointing
  the fixture's HTTP-serving mock at a counter that must stay at zero);
  simulate `Interrupted` by hand-writing the `installing` sentinel before
  a call and asserting a forced full reinstall; simulate a changed
  `desired` set and assert the diff (`Transaction`) does the minimal
  add/remove.
- **`ana-lockfile`/`ana-solver` tests** (Phase 1's fallout): update in
  place to build `RepoDataRecord` fixtures instead of `PackageRecord`;
  no new test *behavior* needed, this is schema fallout.
- **`ana-pypi-conda-map` tests** (Phase 2's fallout): `FakeHttpClient`
  becomes async; existing test *cases* (etag/last-modified handling,
  concurrent-refresh dedup via `std::thread::spawn`) are unchanged in
  intent.
- **`ana` integration test** (new): a full `run_command` call against a
  `pyproject.toml` fixture whose one dependency resolves against the local
  fixture channel, asserting the process actually executed (e.g. `ana run
  python -c "import sys; sys.exit(0 if 'installed-pkg' in sys.path... "`-style
  check, or simpler: assert the installed package's file exists under
  `env_path` and the exec'd command's exit code matches).
- Explicitly **not** attempting an end-to-end wheel-install test against
  any real PyPI-mirrored conda channel, per the "no live channel yet"
  finding -- the wheel-path fixture test above is the actual coverage for
  "wheels are enabled," and real-channel testing becomes a follow-up once
  such a channel exists.

## Suggested sequencing / PR breakdown

Each phase above is close to independently mergeable, in this order (each
depends only on earlier ones):

1. Phase 0 (rev bump + deps) -- almost no risk, `cargo build --workspace`
   is the whole acceptance check.
2. Phase 2 (ureq → reqwest) -- fully isolated, zero production callers
   today, easiest to review in isolation.
3. Phase 1 (lock schema widening) -- mechanical but touches the most
   existing test code; worth its own review pass.
4. Phase 3 (cache location) -- small, one function body in `main.rs`.
5. Phase 4 (`ana-installer` crate, new) -- the actual new logic; biggest
   single review, but net-new code with no existing callers to break.
6. Phase 5 (wiring + exec) -- ties everything together; this is the PR
   that changes `ana run`'s user-visible behavior (prints → actually
   installs and execs), so it's the one most worth a deliberate,
   isolated review even though the plumbing was staged earlier.
7. Phase 6 is config-only and rides along inside Phase 4/5's diffs rather
   than being its own PR.
