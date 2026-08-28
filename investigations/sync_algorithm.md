# The `ana` sync algorithm: one algorithm, three initial conditions

Scope: the actual algorithm decision `investigations/lock_file.md` was
building toward — what `ana` does, concretely, when it's asked to make an
environment match a project's declared dependencies. This file resolves the
three scenarios raised for that decision (no environment yet; `pyproject.toml`
changed; environment was manually modified) against the four-part shape
`lock_file.md` already extracted from uv and rattler/pixi. No new
primary-source claims are made here — this is synthesis and decision-making
on top of that prior research, not another doc-diving pass.

## The central claim: there are not three algorithms

The temptation in the prompt that produced this doc was to treat the three
scenarios as three code paths — one of which (manual environment
modification) gets explicitly written off as out of scope. That's the wrong
frame. Re-reading `lock_file.md`'s four-step shape with an eye specifically
on *what each step's input actually is* shows all three scenarios are the
same four steps fed different initial conditions, not different steps:

| Step | Input it reads | Scenario 1 (no env) | Scenario 2 (`pyproject.toml` changed) | Scenario 3 (manual env edit) |
|---|---|---|---|---|
| 1. Validity check | lock file vs. current resolution inputs | no lock file exists → trivially stale | inputs changed → re-derive and compare | **unaffected** — inputs didn't change |
| 2. Recompute | stale lock + previous pins as preferences | full resolve, no preferences to seed with | biased resolve, seeded by old lock | **skipped** — lock was already valid |
| 3. Read environment | live package metadata (conda-meta / dist-info) | empty environment | whatever was there before | **whatever the user just changed it to** |
| 4. Reconcile | target set (from lock) vs. installed set (from step 3) | everything is "missing" → install-only | normal add/remove delta | delta now includes the user's manual changes |

Step 3 is the load-bearing design decision: **it must read the live state of
the environment on every invocation**, not a cached record of "what `ana`
last put there." Given that one commitment, scenario 3 isn't a gap that
needs a special case or a decision to ignore — it's automatically absorbed
by the same diff that already has to exist for scenario 2. A user who `pip
install`s something extra, or `conda remove`s something the lock requires,
or force-upgrades a pinned package by hand, is — from step 3's point of
view — indistinguishable from "the environment happens to currently look
like this," which is exactly the input step 4 is already built to reconcile
against a target set. There is no detection problem to solve, because
nothing needs to be detected; the live read *is* the ground truth every
time.

## What this means concretely for each scenario

### Scenario 1 — no environment

Step 1 is vacuously stale (no lock to validate), so step 2 always runs a
full, unseeded resolve. Step 3's "installed set" is the empty set. Step 4 in
exact mode degenerates to "install everything in the target set" — there is
nothing to remove, because there is nothing installed. This isn't a
separate creation code path; it's the all-missing degenerate case of the
same diff. **No special-cased "environment creation" routine is needed** —
create-the-venv-or-prefix-if-absent is the only genuinely new bit of logic,
and it belongs *before* step 3 (you need something to read metadata from),
not as an alternate branch of step 4.

### Scenario 2 — `pyproject.toml` changed

This is the steady-state path exactly as documented in `lock_file.md`:
structural staleness check → biased re-resolve if stale → live read → diff
→ act on the delta. The one decision this scenario forces that the research
didn't yet pin down: which `ana` commands run in **exact** vs. **inexact**
mode by default, mirroring uv's split.

**Decision:** `ana install` / `ana sync` (explicit environment-mutation
commands) default to **exact** (add missing, remove extraneous) — matching
`uv sync`. `ana run` defaults to **inexact** (add-only, leave extras alone)
— matching `uv run`'s bias toward "don't surprise me with removals when I
just wanted to execute something," with an `--exact` flag to opt into the
stricter behavior per-invocation. Read-only commands (`ana list`, `ana
show`, a future `ana diff`) run steps 1–3 for reporting purposes but never
step 4 — they surface the delta without acting on it.

### Scenario 3 — manual environment modification

**Recommendation: do not carve this scenario out as unhandled.** Doing
so would actually be *more* work than the default behavior, not less — it
would require explicitly detecting and trusting drift (e.g., maintaining a
separate "what did `ana` last install" ledger and diffing against *that*
instead of live state) in order to deliberately leave manual changes alone
regardless of mode. The uv/pixi-derived algorithm gives you the opposite,
better default for free: every environment-mutating command re-establishes
truth from the live environment, so manual changes are corrected (exact
mode) or left alone but not trusted to satisfy the lock (inexact mode) on
the very next `ana` invocation, with zero scenario-specific code.

Concretely, under this algorithm:

- Manually `pip install`ing an extraneous package: absent from the target
  set → exact mode removes it on next `ana install`; inexact mode
  (`ana run`) leaves it, but also doesn't let its presence satisfy anything.
- Manually removing or downgrading a package the lock pins: live read shows
  it missing or at the wrong version → flagged as "needs install," exactly
  like the scenario-1 all-missing case, and corrected transparently.
- Manually installing a version that would satisfy `pyproject.toml`'s
  looser constraint but not the lock's exact pin (e.g. lock pins
  `numpy==1.23.5`, `pyproject.toml` only says `numpy>=1.20`, user installs
  `numpy==1.24.0` by hand): still flagged and reverted in exact mode. This
  is intentional, not a bug to design around — steps 3/4 diff against exact
  pins, not against the looser requirement text step 1 checks. "The lock
  says what's installed" is the entire point of having a lockfile; scenario
  3 doesn't get to relax that any more than scenario 2 does.

**What is legitimately out of scope** — a narrower carve-out than "manual
environment changes" as a whole:

- **Metadata-invisible corruption.** Step 3 is a metadata read
  (`conda-meta` / `.dist-info`), not a file-integrity scan — this is a
  direct carry-over from uv's own step 3 ("a cheap local metadata read, not
  a reinstall-to-check"). If a user hand-deletes files inside a package's
  install directory without going through the package manager, the
  recorded metadata still claims the package is present and correct, and
  `ana` will not see the discrepancy. This is genuinely "the user's fault"
  in the sense that it's a fault mode `conda`, `pip`, and `uv` all share —
  not something particular to `ana`'s design that needs solving.
- **Environments not managed by any metadata-recording package manager**
  (e.g. files dropped into a bare `site-packages` by hand with no
  `.dist-info` at all). Same failure mode as above, same reasoning.
- **Hand-edited lock files claiming a false state** — analogous to
  bypassing `uv.lock`'s own integrity expectations; out of scope in the
  same way tampering with `uv.lock` directly is out of scope for uv.

## Steps 3–4, including wheels: `rattler::install`, one pass

Checked directly against the pinned source for the crates this repo already
depends on transitively (`rattler` `0.48.0`, `rattler_lock` `0.31.5`, both
present in the local cargo registry cache) plus the project's own rattler
fork (`intentionally-left-nil/rattler`, PR #1 "Allow installing from a
wheel", `wheel-support` branch — a fork of `conda/rattler`, not upstream),
not inferred from docs or general familiarity with rattler. This matters
enough to verify precisely, because it changes what code `ana` actually has
to write for steps 3 and 4: **none** — it's two library calls, and they
cover conda packages and wheels together, not as two coordinated systems.

In `ana`'s model, wheels aren't a different package type from conda's point
of view — they're conda packages whose archive happens to be a `.whl`
instead of a `.conda`, with their own v3 wheel repodata for solving. That
shows up concretely in the fork as one added field, not a parallel type
hierarchy: `rattler_conda_types::package::DistArchiveType` gained a
`Wheel(_)` variant, carried on `record.identifier.archive_type`. A
wheel-origin and a conda-origin package are **the same record type**
(`RepoDataRecord`/`PrefixRecord`), and the resolved, per-platform target
list the solver hands to the installer is homogeneous — one list, not
one-list-per-package-type.

`rattler::install::Transaction::from_current_and_desired()`
(`rattler/src/install/transaction.rs:186`) *is* the diff engine for step 4,
and it needs no wheel-awareness at all — it's generic over
`PrefixRecord`/`RepoDataRecord` and never inspects `archive_type`. Its
signature is exactly the "installed set vs. target set" shape steps 3–4
already needed:

```rust
Transaction::from_current_and_desired(
    current: impl IntoIterator<Item = PrefixRecord>,   // step 3's output
    desired: impl IntoIterator<Item = RepoDataRecord>, // step 4's target_set
    reinstall: Option<&HashSet<PackageName>>,
    ignored: Option<&HashSet<PackageName>>,
    platform: Platform,
) -> Result<Transaction, TransactionError>
```

It matches by package **name**, then per name: present only in `desired` →
`Install`; present only in `current` (and not in `ignored`) → `Remove`;
present in both with different content (sha256/md5/size, falling back to
name+version+build) → `Change`; present in both with identical content but
`python`'s own pinned version changed and the package is `noarch: python` →
`Reinstall` (relink for bytecode recompilation — this is why the
transaction tracks the `python` record specially, `transaction.rs:200-205`);
present in both, identical, no python relink needed → `unchanged`, zero I/O.
Removals apply LIFO; installs apply in the desired list's order. None of
this branches on where a record's archive came from — a wheel-origin
package that's already installed and unchanged is `unchanged` for the exact
same reason a conda-origin one is.

`rattler::install::Installer` (`rattler/src/install/installer/mod.rs:490`)
is the executor, and it subsumes step 3 as well: call
`Installer::new().install(prefix, desired_records)` without supplying
`.with_installed_packages(...)` and it reads the prefix's own `conda-meta`
itself (`PrefixRecord::collect_minimal_from_prefix`, `mod.rs:514`) before
building the `Transaction` — and short-circuits before touching disk at all
if the resulting transaction is empty (`mod.rs:602-610`). Getting the
`desired` list straight out of the (possibly freshly-written) lock is one
more call: `rattler_lock::Environment::conda_repodata_records(platform) ->
Vec<RepoDataRecord>` (`rattler_lock/src/lib.rs:878`).

Only the fetch/link stage *inside* that single `install()` call needs to
know about wheels, and the fork adds exactly that, nothing more: a
`wheel_cache_dir` (wheels and `.conda`/`.tar.bz2` need different extraction,
so they get separate caches), and, per record, a branch on
`archive_type` — `Wheel(_)` → `wheel::populate_wheel_cache` +
`wheel::install_wheel`; otherwise → the existing `populate_cache` +
`link_package`. Both branches run from inside the same `install()` call,
over the same unified `desired` list, against the same prefix. Removal
needs no equivalent branch at all — `unlink.rs` is untouched by the fork,
because `install_wheel` writes a normal `conda-meta`/`PrefixRecord` entry
just like a conda package would. Once installed, a wheel-origin package is
indistinguishable to every downstream reader: the next invocation's
`PrefixRecord::collect_minimal_from_prefix`, and this invocation's
`Remove`/`Change` handling, need no wheel-specific logic at all.

So the end-to-end "requirements changed → new lock → then what" answer,
concretely, covering both conda packages and wheels in one shot, is:

```rust
let desired = lock.environment(env_name)
    .conda_repodata_records(platform)?   // one unified list — conda + wheel records
    .unwrap_or_default();

Installer::new()
    .install(prefix_path, desired)   // reads current state, diffs, applies
    .await?;
```

**Exact vs. inexact maps onto the `ignored` parameter, not a different code
path.** Rattler's default behavior *is* exact mode (extraneous names get
removed). To get `ana run`'s inexact behavior, compute
`ignored_packages = names(current) − names(desired)` and pass that set in —
names present in both `current` and `desired` are untouched by this (still
get the normal content-compare/update treatment), only the truly-extraneous
ones get frozen in place. The entire exact/inexact switch is that one
name-set subtraction; there's no second diff algorithm to write, and it
applies uniformly to conda and wheel-origin packages alike.

**Correction/clarification: this is an `ana`-only policy, not a borrowed
rattler or pixi behavior.** Rattler itself has no concept of "exact" vs.
"inexact," and no solving happens at this layer at all — that already
finished at lock time. `ignored` is a generic "leave these names alone"
primitive; *inexact mode* is nothing more than one particular way `ana`
chooses to populate it (`names(current) − names(desired)`). Checked
directly against pixi's own current install path — the rattler ecosystem's
actual uv-analog — to see whether this split already exists there
(`prefix-dev/pixi`, `crates/pixi_command_dispatcher/src/install_pixi/ext.rs`,
`main` branch, retrieved 2026-08-26): **it doesn't.** Pixi calls
`Installer::with_ignored_packages(spec.ignore_packages.take()...)`, but
`ignore_packages` there is populated from an explicit, caller-supplied
skip-list used only to drop named *source* packages (built from a local
recipe) out of the source-build fanout before installing — never from
`names(current) − names(desired)`. Every `pixi install`/`pixi run` call in
that codepath drives the prefix to exactly match the fully-resolved
`binary_records` set; there is no pixi equivalent of `uv run`'s
additive-only default. **Pixi's actual behavior is uniformly closer to `uv
sync` than to `uv run`** — so "mirroring uv's split" is `ana` porting a
uv-specific design decision onto a rattler primitive that happens to be
capable of expressing it, not adopting a pattern that already exists
anywhere in the rattler/pixi ecosystem. That's a fine thing to do — the
primitive is real, tested (`test_ignored_packages`,
`rattler/src/install/transaction.rs`), and sufficient — but it should be
described as a deliberate `ana` design choice, not as "how conda tooling
already handles this."

**A related pixi mechanism worth adopting, found while checking the above,
that neither this doc nor `env_storage.md` currently accounts for:**
fingerprint short-circuit plus a cross-process prefix lock. **(2026-08:
adopted, but not in this exact shape** — see
`investigations/env_state_implementation_plan.md`; `ana` ended up with a
plain `packages`-equality check against `ana-lockfile`'s env lock rather
than a dedicated fingerprint hash, and "was a previous install
interrupted" is a `dirty` bit that triggers wiping `env_path` recursively
rather than pixi's `with_reinstall_packages` force-reinstall. The
underlying idea below — skip steps 3-4 on a match, guard interruption
detection with the same lock — is what was adopted; the mechanics
differ.)** The same file
shows pixi computing `EnvironmentFingerprint::compute(binary_records)` — a
hash of the fully-resolved target set — and, under a held
`EnvironmentLock` on the prefix, skipping the read-`conda-meta`/build-
`Transaction`/call-`Installer` sequence entirely if a marker written by a
previous install already matches that fingerprint (returning a synthetic
empty/"unchanged" `Transaction` instead of touching disk or even reading
current state). The same lock is held across the fingerprint-check,
install, and fingerprint-write, with periodic "still waiting on another
pixi process" warnings, and a crashed prior install is detected
(`was_interrupted()`) and forces a full reinstall rather than trusting a
possibly-partial prefix. Two implications for `ana`, independent of where
the environment physically lives: **(a)** hashing the resolved target set
to skip straight past steps 3–4 on a match is a real, already-proven
technique, and can be adopted on a fixed-path prefix without going as far
as making that hash the storage path itself (see `env_storage.md`'s Option
D, which does the latter — pixi only does the former); **(b)** concurrent
access to one shared mutable prefix does not have to be a correctness
hazard if it's guarded by a real lock — serializing via a lock is a working
alternative to isolating via separate paths, at the cost of one invocation
blocking on another's install rather than racing it, which nuances (without
eliminating — see `env_storage.md`'s Option A discussion) the concurrency
argument made there.

## Unified algorithm (pseudocode)

```
fn ana_reconcile(project, env, mode: Exact | Inexact, trigger: Mutating | ReadOnly):
    // Step 1: validity check — structural diff of resolution inputs
    // (lock_file.md Property 1 decision: shallow, uv-style; no transitive
    // re-walk, no content hash)
    if lock_missing(project) or not lock_matches_inputs(project.lock, project):
        // Step 2: recompute, biased toward the existing lock's pins
        preferred = project.lock.entries if project.lock else []
        new_lock = resolve(project.requirements, preferred_versions = preferred)
        write_lock(new_lock)
        lock = new_lock
    else:
        lock = project.lock

    // Step 3+4: not hand-rolled — see previous section.
    // desired = lock.environment(env).conda_repodata_records(platform)
    // Installer::new()
    //     .with_ignored_packages(mode == Inexact ? extraneous_names(env, desired) : {})
    //     .install(env.prefix, desired)
    // reads current state itself, diffs (Transaction::from_current_and_desired),
    // and applies (or, for trigger == ReadOnly, stop after building the
    // Transaction and report transaction.operations instead of calling
    // Installer at all).
    //
    // `desired` is one unified list — conda-archive and wheel-archive
    // records side by side (distinguished only by `archive_type`), per the
    // project's rattler fork ("Allow installing from a wheel"). One
    // Transaction, one Installer call, for the whole environment.
```

## Remaining open decision this doc surfaces

Whether to layer optional diagnostic UX on top of this — e.g. `ana list
--diff` explicitly calling out "these N packages are present but not in the
lock" so a user *notices* drift even when running in inexact mode and
nothing gets auto-removed. This is pure reporting on data steps 3/4 already
compute; it does not change the reconciliation algorithm above and can be
deferred independently of it.
