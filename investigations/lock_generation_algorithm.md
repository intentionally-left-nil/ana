s# Lock generation: deciding whether to (re)solve, and writing the result

Scope: this is the concrete, implementation-ready algorithm for the one
question `env_storage.md`, `sync_algorithm.md`, and `lock_file.md` left as
"and then you solve" — given a project root and a resolved environment (root
`ana.lock`/`.env` or `.ana/<hash>/ana.lock`/`env`, per `env_storage.md`'s
discovery procedure), **should `ana.lock` be regenerated, and if so, how is
that done safely under concurrent invocations, across possibly more than
one platform, without dirtying a committed file for no-op checks.** Every
decision below was reached by walking through the tradeoffs explicitly
(hash-based checks, subset hashing, semantic `matches()`-based checks, a
plain structural set-diff, how a single `ana.lock` covers more than one
platform without resurrecting conda-lock's coordinated multi-arch solve,
and finally where the cheap staleness-check bookkeeping itself should
physically live) and settling on concrete answers for stated reasons. This
doc records the destination, not the walk, so a fresh implementer isn't
left re-deriving it. Where something is still genuinely open, it's called
out as such, not silently assumed.

This doc assumes `env_storage.md`'s environment-path discovery procedure has already
run and produced a `(lock_path, env_path)` pair — it starts from there, and
does not re-derive which environment a given `--group` selection maps to.
`env_path` is inherently single-platform per invocation (you can't
materialize a directory that's simultaneously a valid `linux-64` and
`osx-arm64` environment), so it only ever reflects `Platform::current()`
regardless of how many platforms `lock_path` itself covers.

**Two files, two different authority levels, on purpose:**

- **`ana.lock`** — committed to git, and changes *only* when a real
  resolve happens. Holds the actual per-platform requirement/package
  records that matter for reproducibility.
- **A small cache file inside `env_path`** — never committed (it lives
  inside a directory `env_storage.md` already gitignores), and changes on
  nearly every `pyproject.toml` edit, most of which don't require a
  re-resolve. Holds only the stage-1 staleness shortcut. Losing it,
  deleting it, or having it go stale relative to `ana.lock` is always
  safe — it can only ever cause extra work, never an incorrect answer
  (same "fails open" property this design has relied on since the
  original hash-check discussion).

The rest of this doc is organized around that split.

## The key enabling fact this whole design leans on

`ana`'s PEP 508 → matchspec conversion pipeline
(`ana-marker-matchspec`'s `known_values_assumption(subdir: Platform)` +
`restrict()`, per `pep508_to_matchspec_api.md`) is a **pure function of an
arbitrary target `Platform`, not of the machine actually running `ana`**.
The facts it bakes in — `platform_machine == "x86_64"` for `linux-64`, etc.
— are, in that doc's own words, "definitional constants... true by the
definition of what `linux-64` *means*, on any machine, not something probed
from whatever host `ana` happens to be running on right now." No syscalls,
no network, no host detection: `known_values_assumption(subdir)` for a
subdir you are *not currently running on* is exactly as cheap and exactly
as valid as for the one you are.

This is the fact that makes everything below possible: **you can compute
"what would `ana` convert this project's requirements to, on platform P"
for any P, from any machine, offline** — the same pure conversion used for
a real resolve, just parameterized by a `Platform` value that isn't
necessarily `Platform::current()`. Solving for P is a different matter: it
needs network access to P's package index/repodata — but nothing else.
Given the repodata, the solver runs the same way for a foreign subdir as
for the native one, from any machine. That split — conversion is free and
portable, solving is network-bound but equally portable — is the boundary
the three modes below are built around: *checking* any platform's
staleness is always offline; *resolving* any platform is always online;
and neither ever happens implicitly for more than one platform per
invocation.

## Decision: `ana.lock` partitions by `(environment, platform)`; each section holds only real, resolve-time data

Adopting `rattler_lock`'s per-`(environment, platform)` partitioning
(`lock_file.md`'s Property 3) literally, for the file's *shape* — but
populated **independently and opportunistically**, one platform at a time,
never via a coordinated multi-target solve (see "Why not take-1's
multi-subdir solve" below).

```toml
[platforms.linux-64]
requires_python = "..."

[[platforms.linux-64.requirements]]
matchspec = "numpy[version='>=1.20']"  # canonical MatchSpec::to_string(),
                                         # produced with
                                         # known_values_assumption(linux-64)
source = "runtime"

[[platforms.linux-64.packages]]         # full resolved PackageRecord,
name = "numpy"                          # lock_file.md Property 2, unchanged
version = "1.23.5"
# ...

[platforms.osx-arm64]                   # a second, fully independent
requires_python = "..."                  # section -- possibly reflecting
[[platforms.osx-arm64.requirements]]     # an older pyproject.toml state
matchspec = "numpy[version='>=1.20']"    # than linux-64's, if this
source = "runtime"                       # platform hasn't been re-checked
[[platforms.osx-arm64.packages]]         # as recently -- see lock_file.md's
name = "numpy"                           # "a package can legitimately be
version = "1.23.4"                       # pinned to different versions in
# ...                                    # different environments" note, which
                                          # applies across platforms within
                                          # one environment the same way.
```

Notably **no hash of any kind lives in `ana.lock`** — see the next section
for where that bookkeeping actually lives and why. `requirements`/
`packages`/`requires_python` are still duplicated per platform section
(not hoisted to a shared top level) for the same independence reason
discussed when this design was first proposed: a resolve for one platform
must never be able to make a *different*, unresolved platform's section
read as fresher than it actually is. Exact serialization format
(TOML/JSON/YAML) is still not decided by this doc — see "Open TODOs."

### Why not take-1's multi-subdir solve

The unpleasant part of conda-lock's actual multi-arch model is solving N
times — once per target platform, on every lock update, whether or not
anything changed for that platform. `ana` already opted out of that at
the matchspec-conversion layer (`pep508_to_matchspec_api.md`'s superseded
"take 1" — a `CondaTarget` loop producing one portable matchspec set —
was designed but never built, in favor of single-target `restrict()`).
This decision doesn't reopen that: **solving still only ever happens one
platform per invocation, and only when that platform's section is
actually stale — or when a platform was explicitly named.** What changes
is that the *lock file* can now accumulate more than one platform's
already-solved output over time, that *checking* any section's staleness
(not solving it) is possible from any machine, per the "key enabling
fact" above, and that the platform a solve targets may be explicitly
selected (cross-platform mode below) rather than always being
`Platform::current()`.

## Decision: the stage-1 hash lives in a separate, local, gitignored cache file — never inside `ana.lock`

**Why:** `ana.lock` is committed. A "still valid, just refresh the
staleness-check bookkeeping" event — which happens on *every*
`pyproject.toml` edit that stage 1 misses on, including edits totally
unrelated to dependencies — is not something anyone reviewing `git log`/
`git diff` on `ana.lock` should ever see. If the hash lives inside
`ana.lock`, every such no-op edit dirties a file that's supposed to only
change when the actual resolved package set changes. Moving the hash out
fixes this at the root: **`ana.lock` now changes if and only if a real
resolve happened.**

**Where it lives:** inside `env_path` — e.g. `.env/pyproject_hash.json`
for the default environment, `.ana/<hash>/env/pyproject_hash.json` for a
`--group` environment. Two things fall out of this placement for free:

- **Already gitignored, with no new rule needed.** `env_storage.md`
  already ignores `.env/`/`.ana/*/env/` in their entirety ("always derived
  from their sibling `ana.lock`... should never be committed"). A file
  placed inside one of those directories inherits that for free.
- **Already single-platform-scoped by directory, with no new machinery
  needed.** `env_path` itself is inherently tied to one platform — per
  `env_storage.md`'s own "Platform is deliberately not part of the path"
  section, a foreign-platform `env_path` "simply looks entirely foreign...
  and gets rebuilt from scratch." So this cache file never needs to hold
  more than one platform's worth of bookkeeping; there is no
  `platforms.<subdir>` nesting to reintroduce here the way there is inside
  `ana.lock`.

**Content — 2 keys:**

```json
{
  "pyproject_hash": "<sha256 hex of pyproject.toml>",
  "ana_lock_hash": "<sha256 hex of this platform's section of ana.lock>"
}
```

- **`pyproject_hash`**: same role it had before this revision — just
  relocated.
- **`ana_lock_hash`**: hash of this platform's own `platforms.<subdir>`
  section within `ana.lock` (not the whole file — hashing the whole file
  would reintroduce exactly the cross-platform-contamination problem
  already solved by not sharing `pyproject_hash` across sections, since
  editing *another* platform's section would change the whole file's bytes
  too). This is the half of stage 1 that catches "`pyproject.toml`
  unchanged, but the lock section moved" — a branch switch, a `git pull`,
  a teammate's re-resolve — and falls through to stage 2 instead of
  wrongly trusting the cache. Hash the canonical serialization of the
  *parsed* section, not raw file bytes, so serializer or formatting drift
  doesn't cause spurious misses.

**Failure mode, unchanged in spirit from every prior hash discussion in
this doc:** missing, corrupt, or platform-mismatched cache file → treat
stage 1 as a miss, fall through to stage 2 against `ana.lock`'s real
content. Never trust a doubtful cache into skipping the real check.

## The algorithm, end to end

Three modes. **Default mode** (`ana run`, `ana install`, `ana sync` with
no special flag) touches only `platforms[Platform::current()]`.
**Cross-platform mode** (an explicit `ana lock --platform <p>`-style
invocation) resolves and writes exactly one explicitly-named platform's
section, and never touches `env_path` or the cache file. **Check mode**
(see "CI check mode" below) inspects platforms regardless of which one is
currently running, and never reads or writes the cache file at all (see
why in that section).

```
// ---- default mode ----
1. lock_path, env_path = discover_paths(project_root, groups)    // env_storage.md, unchanged
2. acquire advisory lock on <root>/.ana/locks/<key>.lock   // held across steps 3-11; key is `default` or the environment hash
3. If lock_path does not exist, or fails to parse, or missing current platform, skip to lock file regeneration
4. Parse the lock file, extract the section with the current platform
5. Calculate the sha256 of the current platform's lock section, and of pyproject.toml. If both match the values in env_path/pyproject_hash.json (if it exists), succeed and do nothing
6. Delete pyproject_hash.json if it exists
7. Calculate the matchspec for pyproject.toml for the current environment
8. Do a set diff of the matchspecs (and requires_python, as its own field) against the current platform's section in ana.lock. If there are any changes, skip to lock file regeneration
9. Otherwise, the matchspec requirements remain the same. Update pyproject_hash.json with the new hash of pyproject.toml and ana_lock_hash, then exit
10. Lock file regeneration: Take the matchspec of the desired platform, and feed it into the rattler solver. Take the output and save the original requirements, and the outcome dependencies to the ana.lock file (re-read, splice only this platform's section, atomic write — see Concurrency)
11. Rewrite pyproject_hash.json with the new pyproject.toml and lock-section hashes, then release the environment lock


Cross-platform solving (ana lock --platform <p>; always solves when invoked — see below)
1. Hold the environment lock
2. Never generate environments, only update the ana.lock file
3. Never consider pyproject_hash.json, as that is only for the native environment
4. Lookup the correct values to use for the desired platform
5. Generate the matchspec
6. Feed it into the rattler solver. Take the output and the original matchspec, storing it in the ana.lock (same re-read/splice/atomic write as default-mode step 10)
7. Release the environment lock


CI mode (is ana.lock out of date)
1. Hold the environment lock
2. For each platform (every section present in ana.lock, plus any declared platforms), generate the matchspec using cross-platform steps 4-5 only — value lookup + conversion, no solver
3. If any platform has differences in the requirements (or is missing a section), return an error (or re-solve the stale platforms via cross-platform step 6 and update ana.lock, depending on the CI settings)
4. Release the environment lock
```

### Cross-platform mode, deliberately

An explicit, online, one-platform resolve — the supported way to add or
repair a section for a platform you are not running on, without a matrix
of machines. It exists because solving is portable given network access
to the target subdir's repodata (see the key enabling fact); what this
design rejects is solving *implicitly* for N platforms on every
invocation, not solving *explicitly* for one. Two deliberate properties:

- **It always solves when invoked** — no stage-1/stage-2 shortcut. Since
  default mode never re-resolves while requirements are unchanged, an
  explicit `ana lock` is the only path that picks up newly published
  upstream packages when the requirements haven't changed ("refresh the
  pins"). `ana lock` with no `--platform` is the same operation for
  `Platform::current()`.
- **It never touches `env_path` or the cache file** — both are scoped to
  the native platform's environment, which a foreign solve knows nothing
  about. (An explicit `ana lock` for the *current* platform refreshes the
  cache exactly like default mode's step 11.)

## Stage 1 / Stage 2

**Stage 1** (step 5) is a two-key check against the *cache file* — the
hashes live there, never inside `ana.lock` (see the decision above) — and
both must match for a hit:

- `pyproject_hash`: a whole-file `sha256` of `pyproject.toml`, computed
  once per invocation. Deliberately not scoped to just the
  dependency-relevant subset of the file, and deliberately not optimized
  further: editing any part of `pyproject.toml` causes a miss on the
  *next* check, and that's an accepted cost, not a bug — accepted
  specifically because a miss now only costs a stage-2 recheck and a tiny
  local cache write, never a committed-file change.
- `ana_lock_hash`: a `sha256` of the current platform's parsed lock
  section, so "`pyproject.toml` unchanged but the lock moved" (branch
  switch, `git pull`, a teammate's re-resolve) is a miss that falls
  through to stage 2, not a wrong "valid" verdict.

**Stage 2** is a plain equality check on two sets of matchspecs, computed
with `known_values_assumption(p)` for whichever platform `p` is being
checked:

- `current.requirements`: the current environment's selected requirements
  (`ana-pyproject::load()`'s `runtime` unioned with every requested group)
  run through the full conversion pipeline
  (`ana-pep508-to-matchspec`/`ana-marker-matchspec`) for platform `p`, then
  canonicalized.
- Compared against `stored.platforms[p].requirements` (read directly from
  `ana.lock`, always fresh off disk — never through the cache file).
- **Any** difference — name added, removed, or changed — is stale. No
  `matches()`-based semantic compatibility check against stored
  `PackageRecord`s (considered and rejected for v1, for the same reason as
  always: the case it would rescue is a minority of real edits, and an
  unnecessary resolve is safe, just wasted work).

**Why matchspecs, not the raw PEP 508 requirement text:** unchanged from
prior revisions — `ana` is a conda-based tool, the matchspec is what
actually feeds the solver, and comparing at the PEP 508 layer would let a
change in `ana`'s *own* conversion logic go undetected even when the
source text hasn't changed.

**Canonicalization rule:** each `MatchSpec`'s own canonical string form
(`to_string()`/`Display`), sorted by package name, so cosmetic
`pyproject.toml` edits never register as a difference, while genuine
differences in what `ana` converts the same source text to correctly do.

`requires_python` is checked as its own field, not folded into
`requirements` — it isn't itself an entry in
`[project.dependencies]`/`dependency-groups`, so it needs its own
comparison or a `requires-python` edit would silently not invalidate
anything.

### Why the cache refresh (steps 9 and 11) is mandatory

If stage 1 misses for platform `p` but stage 2 says "still valid," the
cache file **must** be rewritten with the new hashes — even though
`ana.lock` doesn't change at all. The same applies after a real resolve
(step 11): the lock section's hash has changed and step 6 deleted the old
cache, so skipping the rewrite would guarantee a pointless stage-1 miss
next time. Skipping either write permanently disables the fast path for
`p` (every future invocation re-misses stage 1 and re-pays stage 2 for no
reason), but — critically, and this is the whole point of this revision —
it has **zero effect on `ana.lock`, on git history, or on any other
platform's section**, unlike before this revision where the equivalent
write touched the committed file.

### Failure mode if the cache or lock is missing, stale, platform-mismatched, or corrupt

Any doubt about the cache file (missing, corrupt, wrong platform) pushes
straight to stage 2 against `ana.lock`'s real, freshly-read content —
never to an incorrect "valid" verdict. A missing `ana.lock`, or one with
no section for this platform, pushes to a full resolve for *this platform
only*, never affecting any other platform's section. A syntactically
*corrupt* `ana.lock` is instead a hard error in every mode (including CI
check, where it must never read as "fresh"): the file is committed and
shared, so silently regenerating it would destroy every other platform's
section — the user repairs or deletes it explicitly. Neither file's
absence or staleness can ever produce a wrong "skip everything" answer,
only extra work.

## CI check mode

**Determine whether any section of a committed `ana.lock` is out of sync
with the current `pyproject.toml`, without needing to be running on every
platform the lock covers, and without depending on any machine-local
state.** CI-mode step 2 runs stage 2 for *every* platform under
consideration — including ones that don't match `Platform::current()` —
per the "key enabling fact" above. The platform set under consideration
is the sections present in `ana.lock` unioned with whatever platforms the
project declares it cares about — a declared platform with no section
reports STALE, same as a diff. Where that declaration lives (a
`tool.ana` key, repeated CLI flags) is an open TODO; note the set
*cannot* come from `ana.lock` alone, or a missing section would be
undetectable.

**Check mode deliberately never reads or writes the per-`env_path` cache
file, for any platform, including the current one.** Two reasons, both
load-bearing: first, the cache file only exists for platforms that have an
`env_path` on *this* machine, which for every platform but
`Platform::current()` is never true — there is nothing to read for a
foreign platform regardless. Second, and more importantly, check mode's
entire value proposition is a *complete, from-scratch* verification
against the source of truth (`ana.lock` + `pyproject.toml`), suitable for
a CI job that has never seen this checkout before — trusting a possibly
stale local cache, even for the current platform, would undermine exactly
the guarantee check mode exists to provide.

**What check mode can do:** report, per platform, VALID or STALE (a
declared platform with no section is STALE) — entirely offline, no
network, no index access, safe to run on every PR/push regardless of
which OS the CI job itself is on.

**Fixing stale sections (`--fix`):** an optional auto-solve mode resolves
each stale platform via the cross-platform flow (matchspec → solver →
splice the section back, per cross-platform step 6). Checking is offline,
but fixing is not: it needs network access to each stale platform's
repodata. Because solving is portable given the repodata, a single CI
runner *can* fix every stale platform in one job; the alternative shape
is a matrix job (one runner per platform) where each runner fixes only
`Platform::current()`'s section, keeping each runner's index access
scoped to its own subdir and each solve on native hardware. Either way,
`ana.lock` changes only for sections that were actually stale, and a bare
check (no `--fix`) remains the cheap, complete, all-platforms staleness
report suitable for gating PRs.

Exact CLI surface (flag names, whether this is `ana lock --check`, a
dedicated `ana check`, etc.) is left to the implementer — see "Open
TODOs."

## Concurrency and atomicity

Two independent lock+write flows now, not one:

**`ana.lock`'s environment-level lock** — one advisory file lock per environment
(`.ana/locks/<key>.lock`, so the project root stays clean and a
single `.ana/locks/` gitignore rule covers every environment),
held across default-mode steps 2 through 11, across cross-platform steps
1 through 7, and across CI mode's whole run — but `ana.lock` is **only
actually written in the resolve step** (default-mode step 10,
cross-platform step 6, CI `--fix`). The lock is held across the solve
itself, network I/O included: solves are rare and per-environment, and the
alternative (re-acquiring around the write, re-validating everything in
between) buys nothing worth the complexity. Cache-only refreshes (steps 9
and 11) still run under the same held lock for simplicity (no benefit to
a finer-grained scheme for something this infrequent), but never touch
`ana.lock` itself while holding it.

**Read-modify-write for `ana.lock`, required in the resolve step**
(default-mode step 10, cross-platform step 6). A writer for
platform A must not blindly serialize a full document built from a stale
in-memory snapshot — doing so would silently discard whatever platform
B's writer wrote to *its* section if B's write landed while A was
resolving. Correct sequence, under the held lock: re-read `lock_path`
immediately before writing, splice the newly-resolved section for `p` into
that freshly-read document, leave every other key untouched, write the
whole thing back atomically.

**The cache file's own lock+write is separate and simpler.** It's local
to one machine, one environment, one platform, and one scalar record — no
read-modify-write needed (there's nothing else in the file to preserve;
steps 9 and 11 always overwrite the whole thing), just the same
temp-file-then-rename atomicity, and a lock scoped to `env_path` rather
than the environment root (though reusing the same environment lock, as the
pseudocode does, is simpler and the contention cost is negligible for how
infrequently this write happens).

**Mechanism, unchanged from prior revisions:** `fd-lock` (`Cargo.toml:46`)
for advisory locking, `tempfile` (`Cargo.toml:38`) + same-directory temp
file + `rename()` for atomic writes — both already workspace dependencies,
both already used for exactly this shape of read-check-write sequence in
`ana-pypi-conda-map`'s cache refresh. Pixi's "still waiting on another
process" periodic warning (`sync_algorithm.md:250-274`) is still worth
porting for lock contention.

**Crash recovery:** unchanged — partial writes to either file are
structurally impossible given temp-file-then-rename, and `env_path`'s
broader crash recovery (partial installs) is still deferred until the real
installer replaces the `mkdir -p` placeholder.

## Explicitly out of scope / deferred

- **No semantic `matches()`-based per-package compatibility check** for
  stage 2, for any platform.
- **No pixi-style full transitive re-walk.** `lock_file.md:227-231`'s
  Property 1 decision applies per-platform-section; revisit only once
  local/path/source packages enter scope.
- **No automatic multi-platform solving.** One platform per invocation:
  the current one in default mode, an explicitly named one in
  cross-platform mode. Nothing ever fans out to re-solve every platform
  on every lock update the way conda-lock does; CI `--fix` across
  platforms is a caller's explicit choice, not a default.
- **No `--extra` support yet.** Extras need `env_storage.md`'s
  namespaced-hash addition before they can be added to environment discovery,
  and the same union logic in requirement selection, once that lands.
- **No real channel configuration.** Hardcoded to `["defaults"]`.
- **No real environment materialization.** Still a `mkdir -p env_path`
  placeholder; wiring in `rattler::install::Installer`
  (`sync_algorithm.md:121-221`) is separate, larger work.
- **No fingerprint-based install short-circuit.** Pixi's optimization of
  hashing the *resolved target set* to skip install I/O is a different
  mechanism at a different layer, orthogonal to this doc.

## Open TODOs for the implementer

- **Solver crate.** `rattler_conda_types` is pinned (`Cargo.toml:24`); no
  solver crate (`rattler_solve` or equivalent) is in the workspace yet.
- **Lock file serialization format** (for `ana.lock`) **and cache file
  format** (for the `env_path` JSON sketched above) — neither is decided.
  They don't need to be the same format; TOML for the former (readable
  next to `pyproject.toml`, `toml_edit` already a workspace dependency)
  and JSON for the latter (never hand-read, no reason to prefer TOML)
  are sketched above as suggestions, not decisions.
- **Where this logic lives.** Suggest a new `ana-lockfile` crate,
  depending on `ana-pyproject` and `ana-pep508-to-matchspec`/
  `ana-marker-matchspec` — naming/organization suggestion, not a settled
  decision.
- **CLI surface.** Subcommand/flag naming for all three modes is
  unspecified: check mode (`ana lock --check` vs. a dedicated
  `ana check`), `--fix`, and cross-platform mode's platform selection
  (`ana lock --platform <p>`, repeatable).
- **Where the expected platform set is declared.** CI check mode needs to
  know which platforms *should* have sections (a missing section is
  STALE), and that set cannot come from `ana.lock` itself. Candidates: a
  `tool.ana.platforms` key in `pyproject.toml`, repeated `--platform`
  flags, or both.
- **Section garbage collection.** Nothing in any mode ever *removes* a
  platform section, so a dropped platform lingers in `ana.lock` (and in
  CI's checked set) forever. Options: prune undeclared sections on
  explicit `ana lock`, or leave removal manual.
- **Section validation.** Default-mode step 3 treats "missing" as a regen
  trigger and a syntactically corrupt file as a hard error; decide whether
  a section that parses but is semantically incomplete (requirements
  present, packages empty) also forces regen.
