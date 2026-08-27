# Lock files: uv, rattler/pixi, and decisions for ana

Scope: record how `uv` actually decides (a) whether a resolve is needed and
(b) what to change in the environment, precisely enough to use as the
reference model when we design `ana`'s equivalent over conda v3 repodata +
rattler. The first half of this file is uv-only. The second half
(`rattler_lock and pixi`) does the same primary-source dive for the
conda-ecosystem side and records the design decisions we've actually made
for `ana` as a result of comparing the two.

## Method

Claims below are checked against uv's own docs (`docs.astral.sh/uv/concepts/
projects/{layout,sync}/`, retrieved 2026-08-26), not inferred from general
familiarity with the tool. Quotes are paraphrased where the doc prose is
long; anything in a blockquote-style line is close to verbatim.

## The two-phase split: resolve, then install

uv keeps dependency **resolution** (`uv lock`) and environment
**installation** (`uv sync`) as two separate phases with a fully-pinned file
— `uv.lock` — as the boundary between them. This split is the single most
important structural fact: the installer contains no solver. Everything it
does is a local diff against a graph that was already fully solved in the
previous phase. Both phases run automatically (`uv run` does lock-then-sync
before invoking the command), but they remain conceptually and
code-path separate, and each has its own escape hatches (`--locked`/
`--frozen` for lock, `--no-sync`/`--exact`/`--inexact` for sync).

`uv.lock` itself is described as a **universal / cross-platform** lockfile:
one file captures the resolution for *all* possible markers (OS, arch,
Python version), not one lockfile per platform. Concretely this means a
single `uv.lock` records enough conditional structure (per-package marker
expressions) that a given sync on a given platform can filter it down to a
platform-specific subset without re-resolving.

## Phase 1: `uv lock` (or the implicit lock inside `uv run`/`uv sync`)

### Step 1 — is the existing lock still valid?

> "When considering if the lockfile is up-to-date, uv will check if it
> matches the project metadata."

This is a **structural comparison of resolution inputs**, not a content
hash of `pyproject.toml`. uv re-derives the same input set it would use to
produce a lock from scratch — `[project.dependencies]`,
`[project.optional-dependencies]`, `[dependency-groups]`, `requires-python`,
workspace member set, configured sources/indexes — and checks whether that
input set is still consistent with what's already pinned in `uv.lock`.

Two concrete rules from the docs:
- Add or remove a dependency → lock is stale (input set changed).
- Tighten a version constraint such that the *currently locked* version
  falls outside the new constraint → lock is stale.
- Tighten a constraint but the currently locked version still satisfies it
  → **lock stays valid, no re-resolve.** (This is the case naive "hash of
  pyproject.toml" reasoning gets wrong — a hash would falsely invalidate
  here even though the lock is still a correct answer to the new, narrower
  question.)

Explicitly **not** a staleness trigger: new upstream releases. uv states
this directly — "uv will not consider lockfiles outdated when new versions
of packages are released." Freshness is relative to the project's own
declared constraints, never relative to the state of the package index.
Upgrading past what's pinned is an opt-in action (`uv lock --upgrade[-package]`),
not something the validity check ever does on your behalf.

Enforcement knobs:
- `uv lock --check` (equivalently `--locked` on `run`/`sync`) — treat a
  stale lock as a hard error instead of silently re-locking.
- `--frozen` — skip the validity check entirely, trust `uv.lock` as-is.

### Step 2 — recompute, biased toward the existing lock

If the lock is stale, uv re-resolves — but not from a blank slate. The
existing `uv.lock` entries are fed back into the resolver as **preferences**:

> "uv will prefer the previously locked versions of packages when running
> `uv sync` and `uv lock`. Package versions will only change if the
> project's dependency constraints exclude the previous, locked version."

So a full PubGrub-style solve still happens, but it's biased to reproduce
the previous answer wherever the previous answer remains legal. This is
why adding one unrelated dependency to a large project typically produces a
small lockfile diff rather than a full re-shuffle of every pin. The same
bias extends to Git dependencies pinned to a branch: uv prefers the
previously-locked commit SHA over the branch's current HEAD unless
`--upgrade`/`--upgrade-package` is passed.

`--upgrade` / `--upgrade-package <name>[==<version>]` are the explicit
opt-outs from this bias — full or single-package forced re-resolution
against the latest index state, still bounded by the project's declared
constraints (an upper-bounded dependency won't be upgraded past its cap
even under `--upgrade`).

Output of this phase: a rewritten `uv.lock` with fully pinned
versions/hashes/sources for every package, for every platform marker
combination the project's constraints admit — self-sufficient input for
phase 2, no further index queries needed at sync time (modulo download).

## Phase 2: `uv sync` (or the implicit sync inside `uv run`)

### Step 3 — read the current environment

uv inspects the target virtual environment's already-installed packages by
reading installed distribution metadata (`RECORD`/`.dist-info`) — a cheap
local metadata read, not a re-resolution and not a reinstall-to-check.

### Step 4 — diff against the lock's target set, act on the delta

The "target set" for a given sync invocation is **the lock filtered down**
to:
- the current platform/interpreter's markers (universal lock → per-platform
  subset),
- whichever extras were requested (`--extra <name>`, `--all-extras`; extras
  are **not** synced by default),
- whichever dependency groups were requested (the `dev` group is
  special-cased and included by default; others need `--group <name>` /
  `--all-groups`; group exclusions always win over inclusions when both are
  passed for the same name).

uv computes the difference between "installed now" and "this target set"
and acts only on the delta — installs what's missing or at the wrong
version, leaves already-correct packages untouched. Whether it also
*removes* packages present in the environment but absent from the target
set depends on sync mode:

| Command | Default mode | Behavior on extraneous packages |
|---|---|---|
| `uv sync` | exact | removed |
| `uv sync --inexact` | inexact | left alone |
| `uv run` | inexact | left alone |
| `uv run --exact` | exact | removed |

So "only adds or removes what's necessary to bring into compliance" is
right for `uv sync`'s default, but `uv run`'s default is additive-only —
"compliance" there means "everything required is present," not "the
environment exactly equals the target set." This distinction matters for a
port: two different notions of environment compliance (exact vs. inexact)
coexist, selected per invocation, not a single fixed policy.

The project itself (and other workspace members) is installed **editable**
by default during sync, so that source edits don't require a re-sync to
take effect (`--no-editable` opts out; a project with no build system isn't
installed at all).

## Bottom line

uv's four-part shape holds up, with the corrections folded in:

1. **Validity check** = structural comparison of current resolution inputs
   against what the lock was solved from, *not* a hash and *not* a
   freshness-vs-upstream check. Narrowing a constraint without evicting the
   current pin keeps the lock valid.
2. **Recompute** = a real resolve, but seeded with the existing pins as
   preferences, so the result tends to be a minimal diff rather than a
   fresh answer. Explicit `--upgrade[-package]` is required to intentionally
   move off a still-valid pin.
3. **Read the environment** = cheap local metadata read of what's already
   installed, no network, no solver.
4. **Reconcile** = diff installed-vs-target-set and act only on the delta —
   but "target set" itself is parameterized (platform markers, extras,
   groups) and "act on the delta" has two modes (exact: add+remove,
   inexact: add-only), chosen per command/flag rather than fixed.

The structural takeaway to carry into rattler-based design: resolution and
installation are separable *only* because `uv.lock` is fully self-sufficient
per platform (every version pinned, hashes recorded, no ambiguity left) —
step 4 is pure set-diffing precisely because step 2 already did all the
constraint-satisfaction work. Any conda-v3-repodata equivalent needs that
same completeness property, or "sync" quietly turns back into "solve."

# rattler_lock and pixi: comparison to uv, and decisions for ana

## Method

Claims below are checked directly against primary source, not docs alone:
`conda/rattler`'s `crates/rattler_lock/src/{lib,conda,pypi}.rs` and
`crates/rattler_virtual_packages/src/lib.rs`; `prefix-dev/pixi`'s
`crates/pixi_core/src/lock_file/{satisfiability/,update.rs}`,
`crates/pixi_manifest/src/system_requirements.rs`, and
`crates/pixi_core/src/workspace/virtual_packages.rs`; plus pixi's own docs
at `pixi.sh/latest/reference/pixi_manifest`. All retrieved 2026-08-26.

## Design goal: the same as uv's, reached by a different mechanism

`rattler_lock`'s module doc states the goal explicitly, and explicitly
rejects the mechanism `conda-lock` (its predecessor) used:

> "Conda-lock stores a `content-hash` which is a hash of all the input data
> of the lock-file. This crate approaches this differently by storing
> enough information in the lock-file to be able to verify if the lock-file
> still satisfies an input/source without requiring additional input (e.g.
> network requests) or expensive solves. We call this **static
> satisfiability verification**."

This is the same goal as uv's lockfile-validity check (cheap, local,
no-network verification), independently arriving at the same conclusion
uv's docs make about hashing: a content hash over-invalidates (any edit to
the input, even a no-op one relative to what's pinned, forces a re-solve),
so both ecosystems store structured data and do structural verification
instead of hashing.

## Property 1 — cheap staleness check: same goal, heavier implementation

pixi's actual check (`crates/pixi_core/src/lock_file/satisfiability/`) is
in two layers: `verify_environment_satisfiability` (cheap metadata checks —
channels, subdir/virtual-package identity, indexes, solve strategy) then
`verify_platform_satisfiability`, which is a **full transitive re-walk**:
it pushes the manifest's direct requirements onto a work stack, and for
every locked package it matches, pulls that package's own recorded
`depends`/`requires_dist` back out of the lock and pushes those too —
walking the whole graph using only data already in the lockfile. It then
asserts **bidirectional exhaustiveness**: every requirement must resolve to
something reachable, *and* every locked package must have been visited, or
the lock is stale (`PlatformUnsat::TooManyCondaPackages`/
`TooManyPypiPackages`).

uv's check is shallower — a structural diff of requirement text against
what's recorded, trusting that the previously-verified transitive closure
is still valid. It can take this shortcut because a specific PyPI
version+hash is immutable; pixi can't, because it supports mutable local/
path/source packages (a `recipe.yaml` can change without a version bump)
and `when=` conditions evaluated against virtual packages that can differ
by host even with identical manifest text.

**Decision for ana:** stay with uv's cheap shallow check rather than
pixi's full transitive walk, for as long as `ana` excludes local/path/
source packages from scope (which is the current stance per
`investigations/pyproject_toml.md`). Revisit this if source/path packages
ever enter scope — that's specifically what forces pixi's heavier check.

## Property 2 — self-sufficient entries: converged, adopt directly

`rattler_lock` stores the **full** `rattler_conda_types::PackageRecord`
per locked conda package (not a partial snapshot like `conda-lock`'s
`RepoDataRecord`-lite), and `requires_dist: Vec<Requirement>` per locked
PyPI package — explicitly so the record can be fed back into a solver as a
"preferred" package without re-fetching metadata. pixi actually does this:
`crates/pixi_core/src/lock_file/update.rs` forwards the previous lock's
full records into the solver as `installed: Vec<PixiRecord>` hints, with
the effect (per its own comment) that "the re-solve is a no-op for
anything that was already pinned" — mirroring uv's "prefer previously
locked versions" bias exactly.

**Decision for ana:** store the complete resolved record per lock entry,
not a digest or partial snapshot. Design the future solver crate's
interface with an explicit `installed`/`preferred` parameter from the
start, since both reference implementations converge on this as required
plumbing for incremental re-solve, not an optional nicety.

## Property 3 — platform selection: real structural difference, resolved by ana's mixed scope

uv has one universal graph with PEP 508 markers on the edges; "packages for
this platform" is computed by evaluating markers at read time.
`rattler_lock` instead pre-partitions at lock time: every environment
stores an explicit, enumerated package list per `(environment, platform)`
pair (a flat table of `SelectorId`s into one global deduplicated package
table). Reading "packages for this platform" is a pure lookup
(`lock_file.platform(name)` → `environment.packages(platform)`), no marker
evaluation for that top-level partition. This isn't arbitrary — a conda
package for `linux-64` and one for `osx-arm64` are genuinely different
binary artifacts, not "the same universal thing, conditionally
applicable." (Within a platform's package set, rattler still evaluates
PEP 508 markers on pypi `requires_dist` edges and `MatchSpecCondition`/
`when=` on conda `depends` edges — coarse partition precomputed, fine-grained
edges still conditionally evaluated.)

**Decision for ana:** ana's environments mix conda packages *and* wheels
(v3 wheel repodata) in the same environment — a hybrid neither uv nor pixi
does today, so neither format can be adopted outright. Adopt rattler's
per-target enumeration structure for the coarse partition (subdir-boundedness
is real for the conda side and can't be wished away), but see below for why
ana's edges don't need rattler's remaining conditional-evaluation machinery.

## Property 4 — identity/addressing: rattler is finer-grained than uv

uv's identity is essentially normalized-name(+version). `rattler_lock`'s
`SelectorId` is location-based: the binary's URL/path for conda binaries,
`name[hash]@location` for conda source packages, verbatim URL for pypi —
because conda has a build-matrix axis (build string/build number/variant)
that can produce multiple distinct artifacts at the same name+version,
which wheels don't really have (a wheel's tag already folds its variant
identity into the filename).

**Decision for ana:** if any conda build-string/variant axis survives into
ana's model (it will, for the conda-package side), lock entries need
location- or build-string-aware identity, not plain name+version — name+
version alone would silently collide two different builds of the same
release.

## PEP 508 marker scope: why the lock can end up marker-free

Recap of the scoping decision already made: `platform_release` and
`platform_version` are banned outright (error if referenced anywhere in a
direct or transitive requirement) because they're genuinely open-ended,
host-specific, and unbounded — no fixed set to enumerate ahead of time.
Every *other* PEP 508 marker variable collapses to information the solver
already has by the time it finishes locking:

| Marker variable | Determined by |
|---|---|
| `os_name`, `sys_platform`, `platform_machine`, `platform_system` | subdir |
| `platform_release`, `platform_version` | **banned** — no baseline possible |
| `python_version`, `python_full_version`, `implementation_name`, `implementation_version` | the concrete `python` package the solve pinned for that target |
| `extra` | which extras the environment selection includes — stays a live selection axis, unaffected by any of this |

Consequence: once `platform_release`/`platform_version` are excluded, there
is nothing left in the marker space that depends on the machine doing the
*installing* rather than the machine (or metadata) that did the *locking*.
Every marker can be evaluated once, at lock time, and baked into a plain
boolean (in the lockfile: present in this target's list, or not present at
all). **Ana's lock format needs no marker evaluator downstream of
locking** — no `when=`-style conditional edges anywhere in the file. This
is a stronger simplification than either uv (defers all marker evaluation
to sync time) or pixi (defers most, but still evaluates `requires_dist`
markers and `MatchSpecCondition`/`when=` during satisfiability
verification) — a direct payoff of scoping out the two open-ended markers.

## Conda-native virtual-package `when=` conditions: same fault line, different vocabulary

Conda's virtual packages (`__win`, `__osx`, `__unix`, `__linux`, `__glibc`,
`__archspec`, `__cuda`, `__cuda_arch`, …) split along the identical
fault line as the PEP 508 markers above, confirmed against
`rattler_virtual_packages`' actual fallback behavior:

- **Collapse to subdir, same treatment as `sys_platform`/`os_name`:**
  `__win`, `__osx`, `__unix`, `__linux` (OS family + baseline OS version),
  `__glibc` (baseline minimum glibc, the conda-native analog of manylinux's
  `manylinux_2_17`/`manylinux_2_28` tags), `__archspec` (baseline minimum
  microarchitecture, the analog of the x86-64 psABI levels `x86_64_v2`/
  `v3`/`v4`). Evidence: `VirtualPackages::baseline_for_platform` exists
  specifically to supply these when cross-compiling — e.g.
  `libc: platform.is_linux().then(|| LibC { family: "glibc", version:
  defaults::default_glibc_version() })` — a fixed policy minimum per
  subdir, not real host detection.
- **No baseline possible, presence itself is host-variable:** `__cuda`,
  `__cuda_arch`. Evidence, verbatim from rattler: "`__cuda` and
  `__cuda_arch` are never part of a baseline — no platform is assumed to
  have a GPU — even though both are valid on any platform once something
  detects or declares them." `SystemRequirements::cuda` defaults to `None`
  with no `baseline_for_platform` entry at all.

pixi's actual precedent for the second bucket is **not** a parse-time ban —
it's an explicit, opt-in, manifest-declared floor. `pixi.toml`'s
`platforms = [{ platform = "linux-64", cuda = "12.0" }]` (or the legacy
`[system-requirements]` table) becomes a `SystemRequirements.cuda:
Option<Version>`, turned directly into `VirtualPackage::Cuda(Cuda
{ version })` and fed to the solver as that declared value —
`Cuda::detect_from_host()` is never used for what ends up in the lock.
Undeclared, `__cuda` simply doesn't exist in the solve, so any package
that transitively depends on it becomes unsatisfiable (a normal solve
failure, not a special-cased rejection). It's additionally hard-gated at
the subdir level regardless of declared version:
`virtual_package_applies_to_subdir("__cuda", subdir) = !subdir.is_osx()`
(macOS dropped CUDA support in 2019).

pixi also runs a **separate, later** check — `pixi run`/`shell`/`install`
call `verify_current_platform_can_run_environment`, which probes the real
host's actually-detected virtual packages and compares them against both
the declared floor and what the already-locked packages actually require,
erroring or warning depending on severity. This never rewrites `pixi.lock`
— locking (declared-value-only, deterministic, portable) and "can this
machine run it" (real-host-probed, a pure execution gate) are fully
decoupled passes.

## Decisions made for ana

1. **Mixed conda+wheel environments are ana's own thing.** Neither uv's
   universal-marker-graph lock nor pixi's per-subdir `rattler_lock` covers
   this combination as-is; ana's format has to be assembled from pieces of
   both rather than adopted wholesale.
2. **Per-target enumeration (rattler-style), not a universal marker graph
   (uv-style)**, for the coarse (environment, platform) partition — forced
   by conda binaries being genuinely subdir-bound artifacts.
3. **No conditional edges survive into the lock itself.** Because of the
   marker-collapse argument above, every dependency edge in an ana lock
   entry is unconditional by the time it's written — narrower than
   `rattler_lock`, which still carries live `requires_dist` markers and
   `MatchSpecCondition`/`when=` clauses at read time.
4. **`platform_version`/`platform_release`: banned outright.** Error if
   referenced anywhere in a direct or transitive requirement.
5. **`__cuda`/`__cuda_arch`: not banned, opt-in via a new `[ana.matchspecs]`
   manifest table.** This table lets users write arbitrary
   conda-ecosystem matchspec-style constraints directly — pin a Python
   version, pin CUDA, pin any other virtual package. This mirrors pixi's
   `platforms = [{ cuda = "12.0" }]`/`[system-requirements]` precedent
   exactly: a declared floor fed to the solver, never detected from the
   authoring host. If `cuda` is present in `[ana.matchspecs]`, the lock
   solves with `__cuda` available at that version; if absent, `__cuda`
   doesn't exist in the solve and anything transitively needing it is
   unsatisfiable — the practical equivalent of a ban, but escapable
   per-project rather than a hard parse-time rejection.
6. **`__glibc`/`__archspec`/`__win`/`__osx`/`__linux`/`__unix`: allowed,
   collapse to subdir** via a fixed baseline table modeled on rattler's
   `baseline_for_platform` (manylinux-style minimums). The actual baseline
   values still need to be chosen and written down — see open questions.
7. **Store the full resolved record per lock entry** (adopted from both
   ecosystems), to support incremental re-solve via a solver-side
   `installed`/`preferred` hint parameter.

## Open questions / deferred

- **Staleness-check depth**: shallow uv-style input diff vs. pixi-style
  full transitive re-walk. Deferred pending whether ana ever supports
  local/path/source packages — that's specifically what would force the
  heavier check (see Property 1).
- **Composite target key**: is the lock's partition key subdir alone, or
  `(subdir, resolved python build)`? Leaning toward the latter, since
  wheel selection needs the interpreter tag, not just OS/arch, and a
  single subdir can host environments pinned to different Python minor
  versions.
- **The actual `__glibc`/`__archspec`/`__win`/`__osx`/`__linux` baseline
  table per subdir** — decision #6 above says these collapse to subdir via
  a fixed baseline, but the concrete version numbers haven't been chosen
  or documented anywhere yet.
