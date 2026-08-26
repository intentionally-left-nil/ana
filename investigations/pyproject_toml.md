# pyproject.toml → conda matchspecs: what we will and won't parse

Scope: how `ana` reads a project's dependency data out of `pyproject.toml`
before translating it into conda v3 matchspecs. Decision made here: **we
only support "modern" projects** — static [PEP 621][pep621] `[project]`
metadata plus optional [PEP 735][pep735] `[dependency-groups]`. No
`setup.py`, no legacy `[tool.poetry.dependencies]`, no
`dynamic = ["dependencies"]`. Anything outside that shape is a hard error,
not a best-effort fallback.

## Method

Findings below are checked directly against the canonical, currently
maintained specs on the PyPA packaging guide — [pyproject.toml
specification][spec-pyproject] and [Dependency Groups][spec-depgroups] —
rather than the (now-historical) PEP text alone, since the PyPA spec page
is the one that gets amended as the ecosystem evolves. Where PEP text is
cited it's because it explains the *rationale* better than the terser
living spec.

## The three tables in `pyproject.toml`

Only three top-level tables are currently standardized; everything else is
either `[tool.*]` (namespaced per-tool config) or reserved for future PEPs.

| Table | PEP | Purpose | Read by |
|---|---|---|---|
| `[build-system]` | 518 | Declares `build-backend` + `requires` (what's needed to *run* the backend) | Any PEP 517 frontend (pip, build, uv) — **not relevant to ana's dependency extraction**, only to actually building a wheel |
| `[project]` | 621 | Static, backend-agnostic package metadata: `name`, `version`, `dependencies`, `optional-dependencies`, `requires-python`, etc. | Every "modern" backend is required to treat this as canonical and copy it verbatim into `Requires-Dist`/`Provides-Extra` in the built wheel's `METADATA` |
| `[dependency-groups]` | 735 | Named groups of dev/test/etc. dependencies, never shipped in built artifacts | Local tooling only (uv, pip `--group`, pdm) — build backends are explicitly forbidden from including this in `PKG-INFO`/`METADATA` |

The absence of `[project]` entirely is itself meaningful: per spec, "the
lack of a `[project]` table implicitly means the build backend will
dynamically provide all keys" — i.e. this is the signature of a
`setup.py`/`setup.cfg`-driven legacy project. That's an immediate reject
for us.

## Why `dynamic` is the line we draw

PEP 621's `dependencies`/`optional-dependencies` are *supposed* to be
backend-agnostic — any conformant backend just echoes them into wheel
metadata unchanged. The escape hatch is the `dynamic` array: if a key is
listed there, the backend is allowed to compute (or append to) it at
build time, via arbitrary Python (a `setup.py`-style callable, a
`[tool.setuptools.dynamic]` file reference, a Hatch metadata hook, etc.).

Historically `dynamic` and a static value were mutually exclusive for a
given key — specify statically *or* dynamically, never both, or the
backend must error. **That changed**: the spec now says list/table-valued
keys (which includes both `dependencies` and `optional-dependencies`) MAY
be specified statically *and* listed in `dynamic` simultaneously, with
the backend restricted to only *appending* further entries — it "MUST
NOT remove, reorder, or modify any statically-specified entries."

That change doesn't help us. It means a `pyproject.toml` can look
fully populated (`dependencies = ["requests", "click"]`) and still be
missing entries that only exist after invoking
`prepare_metadata_for_build_wheel` — you cannot tell the difference
between "this is the complete list" and "this is a prefix, the backend
appends more" without checking `dynamic`. So our rule is unconditional:

> If `dynamic` contains `"dependencies"` or `"optional-dependencies"`,
> reject the project. No partial-static-plus-backend-append handling.

Getting the real list in that case requires running the backend's PEP
517 `prepare_metadata_for_build_wheel` hook in an isolated build
environment — real interpreter invocation, not TOML parsing. That's a
fundamentally different (and much heavier) code path than the one we're
building, so it's out of scope rather than a "phase 2" item.

## What counts as "modern" for ana

A project is in scope if, and only if, all of the following hold:

1. `[project]` table exists.
2. `dependencies` is present and **not** listed in `[project.dynamic]`.
3. If `optional-dependencies` is present, it is **not** listed in
   `[project.dynamic]` either (same reasoning — extras feed matchspecs
   too, once `ana` supports extras selection).
4. No `[tool.poetry.dependencies]` table is present without a
   corresponding `[project.dependencies]` — this is the tell for a
   pre-2.0 Poetry project that never adopted PEP 621. (Poetry 2.0+ *can*
   emit standard `[project]` tables; when it does, rule 1–3 already
   cover it and we don't care that the backend happens to be
   `poetry-core`.)
5. `[dependency-groups]`, if present, parses cleanly per PEP 735: no
   include cycles, no duplicate group names after [name
   normalization][name-norm] (lowercase, `-`/`_`/`.` runs collapsed to a
   single `-`).

Everything else — `setup.py`/`setup.cfg` legacy builds, dynamic
dependency computation of any kind, pre-PEP-621 Poetry — is a hard parse
error with a message telling the user why (e.g. "`dependencies` is
listed in `[project.dynamic]`; ana requires static PEP 621 dependency
metadata").

Explicitly *not* a rejection reason:

- **Self-referential extras** (`all = ["myproj[gui,cli]"]`) — the spec
  documents this as a supported pattern across pip/uv/poetry/hatch/pdm,
  and it's still static data, just self-referencing. `dependency-groups`
  cross-group `{include-group = "..."}` includes are the analogous,
  equally-static mechanism on the dependency-groups side.
- **PEP 508 environment markers** inside a dependency string (`django>2;
  os_name != 'nt'`). These are static — the string itself never changes —
  they just need evaluating against the target platform/interpreter at
  matchspec-generation time. This is a downstream solver concern, not a
  parsing-stage rejection.
- **Missing `[build-system]`** by itself. A `[project]` table with fully
  static dependencies is parseable regardless of which backend (or none
  declared) would build it; we never invoke the backend, so which one it
  is doesn't matter for this stage.

## Backend landscape (context, not something we branch on)

We don't special-case backends — the whole point of PEP 621 compliance is
that we shouldn't have to. This table exists to justify *why* that's a
safe bet for "modern" projects specifically:

| Backend | PEP 621 `[project]` support | Legacy non-PEP-621 table | Notes |
|---|---|---|---|
| `setuptools` (≥61) | Yes | `setup.py`/`setup.cfg` | Legacy path still extremely common; `dynamic` is also how `setuptools` handles version-via-`setuptools_scm` |
| `hatchling` | Yes, PEP-621-native | — | Dynamic fields go through `[tool.hatch.metadata.hooks.*]`, which can run arbitrary Python |
| `poetry-core` | Yes, as of Poetry 2.0 | `[tool.poetry.dependencies]` (pre-2.0, and still supported for back-compat) | The one backend where "which version of the tool wrote this file" changes which table holds ground truth |
| `pdm-backend` | Yes, PEP-621-native | `[tool.pdm.dev-dependencies]` (pre-PEP-735 groups) | PDM added native PEP 735 support; legacy dev-dependency groups still appear in older repos |
| `flit-core` | Yes, PEP-621-native | — | Deliberately minimal; no dynamic-dependency machinery to speak of |
| `maturin` / `scikit-build-core` / `meson-python` | Yes, PEP-621-native for Python metadata | — | Native-extension backends; the compiled part is irrelevant to dependency parsing |

The practical upshot: every backend a project would plausibly use *today*
speaks PEP 621 natively. The only real hazard is **old files that predate
a project's own backend adopting PEP 621** — overwhelmingly a Poetry-1.x
problem, occasionally a bare `setup.py` with no `pyproject.toml` metadata
at all. Both are covered by our reject rules above.

## `[dependency-groups]` specifics worth encoding in the parser

- Groups are plain TOML lists containing either PEP 508 strings or
  `{include-group = "name"}` tables — no other object-specifier shape is
  legal yet (future PEPs may add more; per spec we should error on
  unrecognized table shapes rather than silently skip them, since PEP 735
  explicitly says "Tools SHOULD error when evaluating or processing
  unrecognized data in Dependency Groups").
- Includes expand positionally and **do not deduplicate** — if two
  included groups both pin `foo` differently, both entries pass through
  unchanged and it's the solver's problem, not the parser's.
- Group names normalize like PyPI project names (case-insensitive,
  `-`/`_`/`.` treated as equivalent). Duplicate normalized names in the
  same file is a parse error, not "last one wins."
- Nothing in `[dependency-groups]` implies installing the project itself
  or its `[project.dependencies]` — a group is just a bag of extra specs.
  For `ana run`, that means: default env = `[project.dependencies]` (+ the
  project itself, once we support editable/local installs), and any
  `--group <name>` flag is purely additive on top, exactly mirroring how
  `pip install --dependency-groups=<name>` and `uv`'s group support work.

## Bottom line

PEP 621 + PEP 735 together cover every dependency-declaration shape a
project can present *without* invoking a build backend: static runtime
deps, static extras, and static dev-only groups, all as plain PEP 508
strings with a fully standardized location. The only thing that falls
outside that envelope is metadata a backend computes at build time
(`dynamic`) or metadata predating PEP 621 (legacy Poetry,
`setup.py`/`setup.cfg`). Restricting `ana` to the standardized-and-static
case means dependency extraction is pure TOML parsing — no subprocess,
no isolated build env, no arbitrary Python execution — at the cost of
erroring out on a shrinking-but-nonzero slice of older repos. Given the
project's own framing ("we're just going to support modern-style Python
projects"), that's the correct trade.

[pep621]: https://peps.python.org/pep-0621/
[pep735]: https://peps.python.org/pep-0735/
[spec-pyproject]: https://packaging.python.org/en/latest/specifications/pyproject-toml/
[spec-depgroups]: https://packaging.python.org/en/latest/specifications/dependency-groups/
[name-norm]: https://packaging.python.org/en/latest/specifications/name-normalization/
