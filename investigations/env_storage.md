# Where the environment lives: decision

Scope: this closes the question `sync_algorithm.md` and `lock_file.md`
deliberately left open — given a project root, a target platform, and a
selected set of groups, *where on disk* does the resulting lock and
environment actually live, and how does `ana` find them again on the next
invocation. An earlier draft of this doc surveyed four options (single
project-local env, signature-named project-local envs, a central store
keyed by project path, a central content-addressed store) without picking
one. This version records the actual decision and works out its mechanics
precisely enough to implement against.

## The decision

```
<project_root>/
├── pyproject.toml
├── ana.lock                  # default env's lock (no --group/--extra flags)
├── .env/                     # default env's materialized prefix
└── .ana/
    ├── locks/                 # per-bucket advisory lock files (gitignored)
    │   ├── default.lock       # the default bucket's lock
    │   └── ef260e9a.lock      # the --group dev bucket's lock
    ├── ef260e9a/              # --group dev
    │   ├── ana.lock
    │   ├── selection.toml
    │   └── env/
    └── e62119cb/              # --group dev --group doc
        ├── ana.lock
        ├── selection.toml
        └── env/
```

- **No `--group`/`--extra` flags** (`ana run foo`, `ana install`, `ana
  sync`): lock and env live at the fixed, unhashed, top-level paths
  `<project_root>/ana.lock` and `<project_root>/.env/`, peers of
  `pyproject.toml`. This is the common case and stays maximally
  discoverable — no hash to compute, no directory to look up, a human or an
  editor can find it by name alone.
- **Any `--group`/`--extra` selection** (`ana run --group dev foo`, `ana
  install --group dev --group doc`): lock and env live at
  `<project_root>/.ana/<hash>/ana.lock` and
  `<project_root>/.ana/<hash>/env/`, where `hash` is derived from the
  requested selection — not from a name, so the number of distinct
  selections anyone has ever asked for doesn't have to be enumerated or
  declared anywhere in advance the way pixi's `[environments]` table
  requires.
- **Each bucket is a fully independent, self-contained lock+env pair**,
  produced by running the *entire* algorithm in `sync_algorithm.md`
  (steps 1–4) against that bucket's own requirement set (`[project.
  dependencies]` plus whichever groups/extras that bucket represents) and
  that bucket's own previous lock as the resolver's preference seed. There
  is no shared "universal lock" that every bucket filters a subset out of
  — the default bucket's `ana.lock` and `.ana/e62119cb/ana.lock` are two
  separate resolver outputs that happen to overlap heavily in practice, not
  two views of one file. This is a real simplification relative to the
  originally-assumed uv-style "one universal lock, group selection is a
  filter at sync time" model: every bucket uses the exact same
  create-or-reconcile logic, uniformly, regardless of which groups it
  represents — group selection only ever affects *which requirement set*
  gets fed into step 2 and *which path* the result is written to, never the
  algorithm itself.

## Why this shape, not one of the other three

- **Not a single mutable env** (the "Option A" previously considered):
  that model reconciles the same fixed path against whichever flags the
  *current* invocation happened to pass, which means `ana run --group dev`
  and `ana run --group doc` on the same checkout fight over one directory.
  Verified against both uv's and pixi's actual source (see the "Verified
  against uv/pixi" section below): both handle this by taking a
  cross-process lock on the shared prefix rather than by giving different
  requests different paths — a real, working alternative, but it means an
  agent's `--group dev` run and a concurrent `--group doc` run *serialize*
  through the same directory rather than proceeding independently, and
  every switch between different flag sets is a full re-reconcile (extra
  installs and possibly removals) rather than a cache hit. Splitting by
  selection avoids both costs.
- **Not a central, path-hash-keyed store** (Option C, Poetry's model):
  that model would key each project by the absolute path to its checkout,
  which means every fresh agent worktree of the *same* project — the
  primary use case per constraint 3 — is a guaranteed cache miss even when
  its lock is byte-identical to a sibling checkout's. Keeping storage
  project-local sidesteps needing that key at all; the cost (no reuse
  *across* checkouts) is accepted explicitly below rather than solved.
- **Not a fully content-addressed central store** (Option D, Nix/pnpm's
  model): the strongest constraint-3 answer, but the most implementation
  weight, and it forces an immediate, hard decision about whether hand
  modifying a live environment (`sync_algorithm.md` scenario 3) is even
  supported, since a shared store path can't be both. Deferred rather than
  rejected — nothing about this decision forecloses layering a
  content-addressed cache *underneath* `.ana/<hash>/env` later (e.g., as
  the thing individual buckets hardlink from), it just isn't part of v1.

The shape landed on is closest to the previously-considered Option B
(signature-named, project-local), with two refinements: the common
no-selection case gets an unhashed, top-level path instead of living under
`.ana/default/`, and the signature is a short hash rather than a literal
joined string, both purely for filesystem/UX cleanliness.

## Mechanics

### Hashing rule

1. Take the requested `--group` names (and, once extras ship, `--extra`
   names — see "Open questions" below for why extras need one small
   addition before that lands).
2. Normalize each one the same way group names are already normalized
   elsewhere (`pyproject_toml.md`'s PEP 735 rule: lowercase, `-`/`_`/`.`
   runs collapsed to a single `-`), then dedupe and sort.
3. Join the normalized, sorted list with `,`.
4. Hash the resulting string with SHA-256 (already in the dependency graph
   transitively via `rattler_digest`/`sha2` — no new crate needed) and take
   the first 8 hex characters.

No flags at all is the one case that does *not* go through this rule —
it's the fixed, unhashed root path, not `hash("")`.

Worked examples (computed, not illustrative placeholders):

| Invocation | Normalized, sorted signature | `hash[:8]` | Path |
|---|---|---|---|
| `ana run foo` | *(none)* | *(n/a)* | `ana.lock`, `.env/` |
| `ana run --group dev foo` | `dev` | `ef260e9a` | `.ana/ef260e9a/` |
| `ana run --group dev --group doc foo` | `dev,doc` | `e62119cb` | `.ana/e62119cb/` |
| `ana run --group doc --group other foo` | `doc,other` | `4a091557` | `.ana/4a091557/` |

Flag order never matters (`--group doc --group dev` hashes identically to
`--group dev --group doc`), and repeating a flag is a no-op (dedupe happens
before hashing).

### `selection.toml`: defending against the hash being lossy

A hash can't be reversed back into the group list it came from, and 8 hex
characters (32 bits) is not zero-collision-risk over a project's entire
lifetime of ad hoc flag combinations, even though it's comfortably enough
for any single project's realistic usage. Every `.ana/<hash>/` bucket
therefore also gets a small sidecar, `selection.toml`, recording the
literal normalized group/extra list that produced it:

```toml
groups = ["dev", "doc"]
extras = []
```

On every load, `ana` recomputes the hash from `selection.toml`'s contents
and asserts it matches the directory name. A mismatch means either manual
tampering or a genuine hash collision between two different selections —
both vanishingly unlikely, but the check is nearly free and turns a
silent, hard-to-diagnose wrong-environment bug into an explicit error
pointing at the exact bucket. `selection.toml` is also what makes `ana env
list` (or a human doing `ls .ana/`) able to say what a bucket actually
*is* without un-hashing anything.

### Discovery procedure

On every invocation:

1. Walk up from `cwd` to the nearest `pyproject.toml` — the existing
   project-root convention, unchanged.
2. Normalize, dedupe, and sort the requested `--group`/`--extra` flags.
3. If the resulting list is empty: `lock_path = <root>/ana.lock`,
   `env_path = <root>/.env`.
4. Otherwise: `hash = sha256(join(",", list))[:8]`;
   `lock_path = <root>/.ana/<hash>/ana.lock`,
   `env_path = <root>/.ana/<hash>/env`. Write or verify
   `<root>/.ana/<hash>/selection.toml` as described above.
5. Hand `lock_path`/`env_path` to `sync_algorithm.md`'s four-step
   reconcile, unchanged — this doc only decides *which* `lock_path`/
   `env_path` a given invocation resolves to, not what happens once it has
   them.

`ana install`/`ana sync`/`ana run`/read-only commands (`ana list`, future
`ana diff`) all go through the same steps 1–4; only step 5's behavior
(exact vs. inexact, or "stop after computing the diff") differs between
them, per the decisions already made in `sync_algorithm.md`.

## Relation to the lock file(s)

Nothing about `lock_file.md`'s decisions on lock *format* changes — every
`ana.lock`, root or bucketed, is still the same per-platform-partitioned,
fully-self-sufficient-per-entry structure decided there. What changes is
*cardinality and location*: a project can now have an unbounded number of
`ana.lock` files, one per distinct selection anyone has ever materialized,
each independently valid or stale, each independently re-resolved with its
*own* previous entries as the bias/preference seed for the next resolve —
never another bucket's.

One consequence worth stating plainly so it doesn't read as a bug later:
**a package can legitimately be pinned to different versions in different
buckets of the same project at the same time.** `ana.lock`'s `numpy` pin
and `.ana/ef260e9a/ana.lock`'s (the `dev` bucket) `numpy` pin are the
outputs of two separate biased re-resolves with two separate histories;
if the `dev` group happened to pull in a package that forced a different
transitive choice at some point in the past, or if one bucket was
re-resolved (`--upgrade`) more recently than the other, they can diverge
and stay diverged until something forces them back into alignment. This
is the direct cost of "no shared universal lock," paid for the benefit of
"every bucket uses identical, uniform machinery."

## What gets checked into git

- **Tracked:** `ana.lock` (root) and every `.ana/<hash>/ana.lock` plus its
  `selection.toml` that a project actually depends on for reproducible
  builds — e.g., if CI runs `ana run --group test`, that bucket's lock and
  selection file need to be committed, exactly like the root lock. A
  project that exercises several group combinations in CI ends up with
  several committed lock files under `.ana/`; that's an expected
  consequence of this design, not a problem to route around.
- **Ignored:** `.env/`, `.ana/*/env/`, and `.ana/locks/` — the envs are
  always derived from their sibling `ana.lock`, exactly as disposable as
  `.venv` is for uv, and the advisory lock files are pure local
  synchronization state; none of these should ever be committed. Keeping
  every bucket's lock under `.ana/locks/` (rather than a `.lock` in the
  project root or beside each bucketed `ana.lock`) means this one ignore
  rule covers all buckets, present and future. `ana init`/`ana lock`
  should write the ignore rules into `.gitignore` the way uv writes its
  own internal `.gitignore` into a fresh `.venv`.

## Concurrency

Verified directly against both ecosystems' actual source before relying on
this: **uv** documents "uv applies a file-based lock to the target virtual
environment when installing, to avoid concurrent modifications across
processes" (`docs.astral.sh/uv/concepts/cache/`), and **pixi**'s real
install path (`crates/pixi_command_dispatcher/src/install_pixi/ext.rs`)
takes an explicit `EnvironmentLock` on the prefix across the whole
fingerprint-check + install + fingerprint-write sequence, with periodic
"still waiting on another process" warnings and crash recovery
(`was_interrupted()` forces a full reinstall rather than trusting a
partial prefix). `ana` should take the same kind of lock on whichever
`env_path` a given invocation resolves to (root `.env` or a specific
`.ana/<hash>/env`), for the same reason. Splitting storage by selection
already eliminates *most* of the concurrency exposure — `--group dev` and
`--group doc` invocations no longer touch the same directory at all — so
the lock only has to cover the remaining case of two invocations hitting
the *identical* selection (including the no-flags default) at the same
time, which is a real, expected case (two agents both running `ana run`
with no flags against the same checkout) rather than an edge case to
dismiss.

## Cleanup and constraint 3, assessed honestly

This decision is **project-local, not centralized** — it does not deliver
constraint 3's nice-to-have of one central entity that can enumerate and
clean up every `ana`-managed environment on a machine across every
project. What it does deliver, cheaply, is a per-project version of the
same idea: every ad hoc selection a project has ever materialized lives
under that project's own `.ana/`, so a future `ana clean` (scoped to one
project, e.g. run as part of `ana run` itself, opportunistically, the way
uv's own cache pruning is invoked) can enumerate `.ana/*/` and prune
buckets that haven't been touched in some retention window. It can't do
better than age/LRU-based pruning, though — unlike a lock's own staleness
check, there's no way to derive "which selections are still wanted" from
the manifest alone, since group combinations are requested ad hoc and
never declared. If a genuinely central, cross-project entity becomes a
real requirement later, that's a separate, additive piece of work layered
on top (closer to the previously-drafted Option C or D), not something
this decision's shape needs to anticipate or leave room for beyond "don't
do anything that would make it impossible."

## Accepted trade-off

A fresh agent checkout/worktree of a project always starts cold — for the
default bucket and for every `.ana/<hash>/` bucket — even if an identical
sibling checkout of the same project, with the same lock, already resolved
and materialized every bucket it needed. Nothing here shares work across
checkouts, only within one. This is a deliberate simplicity/locality
trade against the content-addressed store's cross-checkout reuse (Option
D); revisit if checkout-churn-driven redundant solving/installing turns
out to dominate in practice.

## Open questions / deferred

- **Extras need a namespaced token before they ship.** The hashing rule
  above only has groups to work with right now. Once `--extra` exists,
  the signature must disambiguate an extra named `dev` from a group named
  `dev` — e.g. hash over `group:dev,extra:dev` rather than bare
  `dev,dev`-that-dedupes-away-wrongly. Not needed for the group-only
  scheme described here, but should land in the same change that adds
  extras support, not be retrofitted after buckets already exist under
  the un-namespaced scheme.
- **Platform is deliberately not part of the path.** An environment is
  inherently single-platform; if the same checkout is later invoked on a
  different platform, the existing bucket's `env/` simply looks entirely
  foreign to that platform's package manager and gets rebuilt from
  scratch — the same all-missing degenerate case as `sync_algorithm.md`
  scenario 1, not a new problem requiring a platform component in the
  directory name.
- **Pruning policy specifics** (retention window, "touched" bookkeeping
  mechanism, whether it runs opportunistically inside `ana run` or only
  via an explicit `ana clean`) are deferred to whenever that command gets
  designed — this doc only establishes that per-project pruning is
  possible and roughly how, not the exact policy.
- **Whether a content-addressed cache eventually sits underneath
  `.ana/<hash>/env`** (e.g., individual buckets hardlinking shared package
  content from a lower-level store, à la pnpm) to recover some
  cross-checkout reuse without changing this doc's path/naming decision —
  worth revisiting if the accepted trade-off above turns out to matter in
  practice, but explicitly out of scope now.
