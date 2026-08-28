# Env lock file: replacing `pyproject_hash.json` + `.ana-install-marker`

Supersedes the stage-1/stage-2 cache design in `lock_generation_algorithm.md`
and the marker design in `package_download_and_install*.md`. Those docs'
other content (matchspec conversion, splice mechanics, rattler
`Transaction`/`Installer` mechanics) is still accurate.

This is unreleased code. No migration, no compat shims, no dual-read of old
`pyproject_hash.json`/`.ana-install-marker` files.

## Files

1. `pyproject.toml` — unchanged.
2. `ana.lock` (project root, or `.ana/<hash>/ana.lock` for `--group`) —
   same TOML format as today, minus the `requires_python` field (see
   "Necessary consequence" below).
3. `<env_path>/ana.lock` (e.g. `.env/ana.lock`) — same file format and same
   Rust types as file 2, reusing `LockFile`/`PlatformSection`/their
   TOML parse/serialize code as-is:
   - `platforms`: a map, exactly one entry (the platform `env_path` is
     for).
   - Each platform section: `requirements` + `packages`, same shape as
     `ana.lock`'s own sections. No `requires_python`.
   - One new top-level key: `dirty: bool`.
   - No `source_path`, no `source_sha256`, no `lock_sha256`, no hashing
     anywhere.

## Algorithm (`ana run`)

1. Read `<env_path>/ana.lock`. Missing/corrupt → `dirty = false`, empty
   `platforms`.
2. If `dirty == true`: delete `env_path` recursively (this also deletes
   the env lock file). Treat as step 1's "missing" case from here on.
3. Convert `pyproject.toml`'s current requirements to matchspecs for
   `platform`.
4. Read `ana.lock`'s section for `platform`.
   - Missing, or matchspecs != section's `requirements` (order-independent
     set comparison): **lock is stale.** Solve, biased by the env lock's
     `packages` (not `ana.lock`'s own, possibly-stale, packages). Splice
     the result into `ana.lock`.
   - Else: use the existing section as-is.
5. Compare that section's `packages` to the env lock's `packages` (plain
   equality, both in canonical sorted order).
   - Equal: nothing to install. Run the command.
   - Not equal: write env lock `dirty = true` (must propagate on
     failure). Reconcile/install the section's `packages`. On success,
     write env lock = `{ dirty: false, platforms: { platform: {
     requirements: <section's requirements>, packages: <section's
     packages> } } }` (best-effort).
6. Run the command.

## Code changes

**`ana-lockfile`**
- `lock_file.rs`: remove `requires_python` field, its TOML parse/emit
  branches, and `PlatformSection::hash` (no more hashing). Keep its
  canonical-sort behavior (`packages.sort()`, `requirements.sort_by(...)`),
  applied wherever a section is built or compared, not just hashed.
- `matchspec.rs`: stop excluding the `requires-python`-derived `python`
  matchspec from `locked`; insert it into the same dedup map as every
  other requirement, with a distinct `source` value (e.g.
  `"requires-python"`).
- `cache.rs`, `hash.rs`: delete.
- New module (e.g. `env_lock.rs`): the env lock file's read/write, reusing
  `lock_file.rs`'s section parse/serialize functions for the `platforms`
  part, plus read/write of the one extra `dirty` key on the same document.
- `algorithm.rs`: replace the stage-1/stage-2 flow
  (`EnsureOutcome::Fresh`/`CacheRefreshed`/`Resolved`, `cache::*` calls)
  with the algorithm above. Solve's `preferred` now comes from the env
  lock's `packages`, not `ana.lock`'s own previous section (default mode
  only — `lock_platform`/`check` are untouched, they never read `env_path`).
- `project.rs`: remove `Project::source_hash`.

**`ana-installer`**
- `marker.rs`, `fingerprint.rs`: delete.
- `lib.rs`: `reconcile` loses the fingerprint/marker short-circuit
  entirely (the caller now decides whether to call it at all, per step 5
  above). `ReconcileOutcome::Unchanged` becomes unreachable from inside
  this function; simplify the return type accordingly.
- `error.rs`: remove `Error::Marker`.

**`ana`**
- `run.rs`: call the new algorithm; only call `ana_installer::reconcile`
  (with the env-lock dirty-flag writes around it) when step 5 says
  packages differ.
- `main.rs`, `lib.rs`: update the outcome types consumed for reporting.

## Tests to update

- Delete: `cache.rs`'s tests, `marker.rs`'s tests, `fingerprint.rs`'s
  tests, `reconcile.rs`'s `second_call_with_the_same_desired_set_short_circuits`
  and `interrupted_marker_forces_a_real_reinstall`.
- Rewrite against the new types: `algorithm.rs`'s stage-1/stage-2 tests
  (`cosmetic_pyproject_edit_refreshes_cache_without_touching_lock`,
  `lock_that_moved_under_us_falls_to_stage2_then_refreshes_cache`,
  `corrupt_cache_is_a_stage1_miss_not_an_error`,
  `check_never_reads_or_writes_the_cache`, every `EnsureOutcome::*` assertion),
  `lock_file.rs`'s `requires_python`/`hash()` tests, `matchspec.rs`'s
  `requires_python_becomes_a_python_spec_without_touching_locked`, `run.rs`'s
  `EnsureOutcome`/`ReconcileOutcome` assertions.
- Add: requires-python-only edit is detected stale via `requirements`
  (no separate field); `dirty = true` on disk wipes and fully reinstalls;
  `ana.lock` packages moving with unchanged requirements (simulated
  `git checkout`) reconciles without a solve.
