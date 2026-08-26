# uv's lock/sync algorithm

Scope: record how `uv` actually decides (a) whether a resolve is needed and
(b) what to change in the environment, precisely enough to use as the
reference model when we design `ana`'s equivalent over conda v3 repodata +
rattler. This file is uv-only — no rattler/conda-lock semantics yet, see
`investigations/rattler_lock.md` (once it exists) for the mapping.

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
