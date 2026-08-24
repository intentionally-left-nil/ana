# ana-pyproject

PEP 621 / PEP 735 `pyproject.toml` dependency resolution for `ana`.

## What's here today

Just `src/resolution.rs`: the algorithm that resolves
`[project.optional-dependencies]` and `[dependency-groups]` into flat
lists of requirements, handling three things:

- **`include-group` expansion** (PEP 735) -- `{ include-group = "base" }`
  pulls in another group's entries.
- **Self-referential extras** -- `myproj[test]` appearing inside
  `myproj`'s own `optional-dependencies`/`dependency-groups` expands into
  that extra's own entries.
- **Cycle detection** -- following either of the above into a cycle is a
  hard error, not infinite recursion.

Resolved results are memoized in the returned `ResolvedDependencies` map
as they're computed, so a group referenced by several other groups'
`include-group` entries is only walked once.

The `toml_edit`-based code that reads an actual `pyproject.toml` file and
produces this module's inputs does not exist yet. See
`investigations/pep508_to_matchspec_api.md` and
`investigations/pyproject_toml.md` in the repo root for that design.

## Provenance

`src/resolution.rs` is adapted from
[`resolution.rs`](https://github.com/PyO3/pyproject-toml-rs/blob/02d274155edf0faf08f8600f0048199067fec26d/src/resolution.rs)
in [`PyO3/pyproject-toml-rs`](https://github.com/PyO3/pyproject-toml-rs)
(published as the `pyproject-toml` crate on crates.io), version
`0.13.7`, commit
[`02d274155edf0faf08f8600f0048199067fec26d`](https://github.com/PyO3/pyproject-toml-rs/commit/02d274155edf0faf08f8600f0048199067fec26d).

That crate is MIT-licensed, Copyright (c) 2021-present PyO3 Project and
Contributors. The `LICENSE` file in this directory is that license,
verbatim, and covers `resolution.rs`'s derivation from it.

We didn't take a dependency on `pyproject-toml` itself (see
`investigations/pep508_to_matchspec_api.md`'s "`ana-pyproject`" section):
it returns `pep440_rs`/`pep508_rs::Requirement` values, and every
requirement string `ana` cares about needs to end up as a
`uv_pep508::Requirement` for the downstream matchspec conversion.
Depending on `pyproject-toml` directly would mean parsing every
requirement string twice, into two non-interoperable `Requirement`/marker
ASTs. The resolution *algorithm*, though, has no dependency on which
`Requirement` type it's pushing around -- it's pure graph traversal over
group/extra names -- so porting it with the types swapped gets us a
correct, spec-tested implementation without that double-parse cost.

### Why port instead of write from scratch

PEP 735's `include-group` semantics and PEP 685's self-referential-extra
semantics are simple to state but easy to get subtly wrong (in particular:
memoizing correctly so shared includes aren't re-walked, and reporting
*which* parent referenced a missing/cyclic name rather than just "not
found somewhere"). `pyproject-toml-rs` already solved this and has a test
suite exercising the tricky cases -- cross-group cycles, missing
includes, extra/group name collisions, groups and extras that don't
interact, and normalization edge cases. Porting the algorithm and test
*cases* (not the test code -- see below) verbatim, with only the types
changed, gets us that correctness for the cost of a type-level port
instead of a from-scratch reimplementation and from-scratch test design.

## Changes from upstream

All changes are type-level, not behavioral -- the recursion structure
(memoize-then-cycle-check-then-recurse), the field names, and the
function names are otherwise unchanged so this stays diffable against the
original:

- **Typed, pre-normalized keys.** Upstream's maps are keyed by raw
  `String`, so a reference like `foo[group-one]` has to be matched against
  a definition written as `group_one =` by re-normalizing both sides on
  every lookup (`normalize_name`, plus a linear `find` over the map instead
  of a direct lookup). Here, the maps are keyed by `uv_normalize::ExtraName`
  / `GroupName`, which normalize once at construction time (by whatever
  builds these maps -- the not-yet-written TOML layer). Two spellings that
  normalize to the same name are already the same key, so
  `resolve_optional_dependency` does a plain `IndexMap::get`, and the
  `normalize_name` helper doesn't exist here at all. The
  `optional_dependencies_with_underscores` test (ported from upstream's
  test of the same name) is the case that exercises this directly.
- **Typed self-reference/include comparisons.** `project_name` is
  `Option<&uv_normalize::PackageName>` compared directly against
  `Requirement::name: PackageName`, instead of `Option<&str>` compared
  against `requirement.name.to_string()`. `DependencyGroupSpecifier`'s
  include variant (`IncludeGroup(GroupName)` here, `Table { include_group:
  String }` upstream) is typed the same way.
- **`DependencyGroupSpecifier::Requirement` instead of `::String`.**
  Upstream's variant holding a `Requirement` is named `String` (matching
  the TOML shape it deserializes from via `serde(untagged)`); renamed here
  since this crate doesn't deserialize this enum directly -- it's built by
  the TOML-walking layer -- so there's no serde-shape reason to keep a
  variant holding a `Requirement` named `String`.
- **Two "must have a parent" panic messages instead of one copy-pasted
  one.** Upstream's `resolve_dependency_group` reuses the string `"missing
  optional dependency must have parent"` verbatim in the *group*-not-found
  branch (a copy-paste artifact from the extra-not-found branch it was
  adapted from). Split into two accurate messages here.
- **One empty-map allocation instead of one per top-level group.**
  Upstream's `resolve` calls `optional_dependencies.unwrap_or(&IndexMap::default())`
  inside the `for group in dependency_groups.keys()` loop, allocating a
  fresh empty map on every iteration whenever there are no optional
  dependencies at all. Hoisted out of the loop here; same fallback
  behavior.
- **No `pub(crate)`.** Upstream's `resolve` is `pub(crate)`, called only
  through `PyProjectToml::resolve()`. This crate's whole point is to be
  importable elsewhere, so `resolve` and the types it touches are `pub`.

Not changed, and deliberately not in scope for this pass: `resolve`
doesn't validate that a `DependencyGroupSpecifier` shape is recognized
(upstream gets that for free from `serde(untagged)` rejecting anything
that isn't a string or `{ include-group = ... }`; here, that validation
belongs to whatever TOML-walking code eventually constructs a
`DependencyGroupSpecifier` in the first place, not to this resolution
step) -- see `investigations/pep508_to_matchspec_api.md`'s
`PyprojectError::UnrecognizedGroupEntry`.

## Tests

`src/resolution.rs`'s test module ports upstream's test *cases* (inputs +
expected resolved output or error message), not its test *code*: upstream
builds each case from a full `pyproject.toml` source string parsed via
`PyProjectToml::new(...).resolve()`; these tests build the same
group/extra maps directly with `indexmap!` and call `resolve` directly,
since the TOML-parsing front end this crate will eventually have doesn't
exist yet.
