# API design: `pyproject.toml` → matchspecs

Scope: the Rust crate(s) that take PEP 508 requirement strings (sourced from
`ana-pyproject`'s reading of `[project.dependencies]` /
`[project.optional-dependencies]` / `[dependency-groups]`, per
`investigations/pyproject_toml.md`) and produce
`rattler_conda_types::MatchSpec` values, fast enough to run on every
dependency of every group on every `ana run`. This is the Rust port of
reroll's `to_matchspec()`, scoped down per `investigations/reroll_deps.md`:
a static PyPI→conda name table instead of the mapper chain (deferred — see
below), no wheel/filename handling, no repodata emission.

**Name mapping is out of scope for this pass.** `investigations/reroll_deps.md`
already settled on a static PyPI→conda lookup table as reroll's mapper chain's
eventual replacement, but that table itself — sourcing it, validating it,
deciding its build-time representation — is independent, deferred work. Every
place below that would call into a `map_name`-shaped function instead uses the
identity: `conda_name := pypi_name`, already PEP 503/CEP-26 normalized. This
keeps the crate boundary the later static-table work will slot into
(described under "Deferred: name mapping" below) visible in the API today
without blocking on it.

## Method

Every claim about a Rust crate's API below was checked against that crate's
actual `docs.rs` page for the version we'd pin (`uv-pep508`/`uv-pep440`
`0.0.72`, `rattler_conda_types` `0.51.0`), not inferred from the Python
original or from documentation prose. Where the Rust API is *richer* than
what reroll's Python code had to work with, that's called out explicitly,
because it changes the port from "transliterate" to "redesign" in a few
specific places.

## The headline finding: this isn't a 1:1 port

reroll's `pep508_to_matchspec.py` produces a matchspec by **string
concatenation**, then hands the whole assembled string back to py-rattler's
parser (`MatchSpec(value)`) just to validate it. That round-trip — format a
condition to a `when="..."` string, then re-parse the entire matchspec
grammar to check your own work — is necessary in Python because `rattler`'s
Python binding only exposes `MatchSpec` as a string-in/string-out type.

The Rust `rattler_conda_types` crate does not have that limitation:

```rust
pub struct MatchSpec {
    pub name: PackageNameMatcher,
    pub version: Option<VersionSpec>,
    pub extras: Option<Vec<String>>,
    pub condition: Option<MatchSpecCondition>,
    // ...13 more public fields, struct implements Default
}

pub enum MatchSpecCondition {
    MatchSpec(Box<MatchSpec>),
    And(Box<MatchSpecCondition>, Box<MatchSpecCondition>),
    Or(Box<MatchSpecCondition>, Box<MatchSpecCondition>),
}
```

`condition` — the CEP-29 `when=` clause — is a **typed AST**, not a string,
and `extras` is a **typed `Vec<String>`**, not a bracket-syntax fragment we'd
have to format ourselves. Every field is `pub` and `MatchSpec: Default`, so
the idiomatic construction pattern is a struct literal / `Default` +
field-assignment — there's no builder API, but there's also no parser to
round-trip through. **Our conversion pipeline should never format a
matchspec string and reparse it.** It builds a `MatchSpec` value directly,
field by field, and the type system is the validation — an
unrepresentable condition simply can't be constructed. This is the single
biggest structural difference from reroll's Python implementation, and it's
a straightforward win: one fewer allocation, one fewer parse (of the more
complex whole-matchspec grammar, not just a version string), per dependency.

The one place a string round-trip is *not* avoidable: `rattler_conda_types::Version`
has no general typed constructor (only `Version::major(u64)` for a
single-segment version, or a feature-gated `From<semver::Version>` bridge
that doesn't cover PEP 440's segment count / pre/post/dev/local shape). Any
version beyond a bare integer still goes through `Version::from_str(&formatted)`.
That's fine — formatting `"3.9.0a0"` and parsing it back is cheap (a small,
regular grammar with no backtracking), unlike reparsing an entire matchspec
with brackets, quoting, and a nested boolean-condition sub-grammar. The
optimization target is the *condition* and *extras* construction, not the
version leaf.

## Crate layout

```
crates/
  ana-marker-matchspec/   # single-target restrict()-based marker ->
                           # Applicability/MatchSpecCondition logic --
                           # see "Slow path, take 2" above for what this
                           # crate actually implements (not the
                           # CondaTarget/multi-subdir sketch below, which
                           # was never built)
  ana-pep508-to-matchspec/# per-Requirement orchestration: name + version +
                           # extras + marker -> MatchSpec
  ana-pyproject/          # TOML structure: PEP 621 + PEP 735, per
                           # investigations/pyproject_toml.md
```

`ana-pypi-conda-map` (see `investigations/pypi_conda_map.md`) now exists to
back the name-mapping call site — see "Deferred: name mapping" below.
`ana-pep508-to-matchspec` itself still depends on nothing but `uv-normalize`
for the name step today; swapping the identity mapping for a real lookup is
a one-function change at a single call site, not a re-plumbing.

No separate `ana-conda-targets` crate, and (now that `ana-marker-matchspec`
is implemented) no `CondaTarget`/`conda_targets()` at all — the single-
target design has no per-subdir list to hold. If a future portable-
matchspec invocation mode ever needs "Slow path, take 1"'s multi-subdir
loop, the same reasoning that ruled out a separate crate for it originally
still applies: it'd be ~100 lines with exactly one consumer, not a generic
"which platforms does ana support" fact (that's just
`rattler_conda_types::Platform`, which already exists as a crate
elsewhere).

Each crate is independently testable and independently useful — in
particular, `ana-pep508-to-matchspec` has no pyproject.toml or TOML concept
at all; it converts one `uv_pep508::Requirement` at a time, same contract
as reroll's `to_matchspec()`, so it can be fuzzed/property-tested against
reroll's own 3,438-line test suite as an oracle without needing any TOML
fixtures.

### Dependency pins

```toml
[workspace.dependencies]
uv-pep508   = { git = "https://github.com/astral-sh/uv", tag = "0.12.6" }
uv-pep440   = { git = "https://github.com/astral-sh/uv", tag = "0.12.6" }
uv-normalize = { git = "https://github.com/astral-sh/uv", tag = "0.12.6" }
rattler_conda_types = "0.51"
```

Same policy pixi uses (confirmed directly in `pixi`'s root `Cargo.toml`):
pin the internal `0.0.x` uv crates as **git dependencies at a fixed tag**,
not crates.io (they're published as unstable `0.0.x` internal-component
crates with no semver guarantee). Unlike reroll's Rust dependency table,
we only need three of the uv umbrella crates — `uv-pep508`, `uv-pep440`,
`uv-normalize` — not `uv-distribution-filename`, `uv-metadata`, or
`uv-pypi-types`, since we're not parsing wheel filenames or METADATA here.
The three we do need must be bumped together (they're path-interdependent
within one uv commit); treat the tag as one workspace-wide version knob,
bumped deliberately with a changelog read, not on autopilot.

#### Bump history

**`0.9.7` → `0.12.6`** (135 commits touching these three crates between the
tags, per `git log <old>..<new> -- crates/uv-pep440 crates/uv-pep508
crates/uv-normalize` against `astral-sh/uv`). Read in full, not sampled;
every substantive (non-"Bump version to ...") commit in that range was
checked against this workspace's own conversion logic and test suite,
each by reproducing the specific input against both tags directly (a
throwaway `cargo run` against each pin, not just reading the uv changelog)
rather than assumed from the PR description alone. Everything below either
changed observable behavior this workspace depends on, or was confirmed
*not* to despite looking like it might:

- **Three real panics, fixed.** `ana-pyproject`'s own module docs commit to
  never panicking on untrusted `pyproject.toml` content, and `0.9.7`
  violated that in three distinct ways any user-supplied requirement
  string could trigger: an extra name ending in a bare separator
  (`foo[bar-]`/`foo[bar_]`/`foo[bar.]`, uv#19779), a reversed-operand
  compatible-release marker against a string field (`"posix" ~= os_name`,
  uv#19782), and (in `uv-pep440` directly, not reached by any of this
  workspace's own inputs but latent regardless) a `u64::MAX` version
  segment overflow (uv#17985). All three are now graceful errors (the
  first) or silently-ignored non-constraints (the second), never a
  process abort. Regression tests: `ana-pyproject`'s
  `invalid_requirement_extra_with_trailing_separator_no_longer_panics`
  and `requirement_with_reversed_compatible_release_string_marker_no_longer_panics`.
- **A real silent-correctness bug, fixed.** `uv-normalize` `0.9.7`
  accepted an empty string as a valid, already-normalized package/extra/
  group name (uv#19435) — reachable in this workspace through
  `ana-pypi-conda-map`'s upstream PyPI→conda mapping table, where an
  empty name on either side of an entry normalized to `""` instead of
  being skipped as malformed, and could then be inserted into the
  filtered mapping as a bogus entry. Regression test:
  `ana-pypi-conda-map`'s `skips_entries_with_an_empty_name_on_either_side`
  (fails against `0.9.7`, passes against `0.12.6` — checked against both
  tags, not just the new one).
- **A genuine matchspec-construction gap, closed.** uv#20268 ("Fix
  exclusive post-release ordering") corrected `uv-pep440`'s own
  `Operator::LessThan`/`GreaterThan` *range* construction (used by uv
  itself for resolution) to match `packaging`'s semantics at the
  `<V.postN`/`>V.postN` boundary. This workspace's own
  `convert_exclusive_less_than`/`convert_exclusive_greater_than`
  (`ana-pep508-to-matchspec/src/version.rs`) were *already* correct here
  — they don't delegate to `uv_pep440::contains` — but the equivalence
  oracle test that would have caught either side drifting only exercised
  the `<`-boundary shape narrowly (`exclusive_comparator_carve_out`), not
  across the full `VERSION_CANDIDATES` sweep used for other operators,
  because that broader sweep genuinely disagreed with `<`'s pre-fix
  `uv_pep440::contains()` even though this workspace's own construction
  was right the whole time. Closed by adding `<` to
  `post_release_literal_agrees_with_pip_across_every_candidate`'s
  comparator list (confirmed: fails against `0.9.7`, passes against
  `0.12.6`) — real coverage this workspace was missing, not new
  production code.
- **A separate, still-open gap, reconfirmed unchanged.** `uv-pep440`'s
  `VersionSpecifier::contains` (the oracle these equivalence tests call
  into — a different implementation than the `Ranges` construction uv#20268
  touched) still disagrees with `packaging` for `>V` when `V` is a
  pre-release and the candidate is a post-release of the same base.
  Re-checked directly against `0.12.6`, not assumed carried over from the
  `0.9.7`-era doc comment: still excluded from
  `rc_literal_agrees_with_pip_across_every_candidate` and
  `dev_release_literal_agrees_with_pip_across_every_candidate`'s
  comparator sweeps, same as before the bump.
- **Two parser relaxations, not correctness fixes, that change what a
  `pyproject.toml` can say.** Trailing commas in a version specifier list
  (`foo>=1,<2,`, uv#19806) went from a parse error to accepted. A
  reversed `in`/`not in` marker (`"3.9" in python_version`) was already
  silently treated as `MarkerTree::TRUE` on `0.9.7` — reconfirmed
  unchanged on `0.12.6`, not a regression introduced by the bump.
  Regression test for the first:
  `ana-pyproject`'s `trailing_comma_in_version_specifiers_is_now_accepted`.
- **Checked and ruled out.** `c22efa11b` ("Reduce scope of public
  interfaces") and `116ca06b3` ("Returns new markers from marker
  operations") both looked, from their diffs alone, like they could break
  this workspace's compilation — the former removes `pub` from
  >1,000 LOC across the whole uv workspace, the latter changes
  `MarkerTree`'s `and`/`or`/`implies` signatures. Neither touches anything
  this workspace's three crates actually call
  (`Requirement`/`VersionOrUrl`/`ExtraName`/`PackageName`/`GroupName`/
  `Operator`/`Version`/`VersionSpecifier`/`VersionSpecifiers`, and
  `MarkerTree::is_true`/`contents`, never `and`/`or`/`implies`): `cargo
  build --workspace` at `0.12.6` is clean. `c462cc0b6` ("Update string
  marker ordering semantics") changes how `>`/`>=`/`<`/`<=` evaluate
  against pure string marker fields, but every marker this workspace
  processes is rejected outright via `MarkerTree::is_true()` regardless
  of shape (see `ana-pep508-to-matchspec`'s module docs), and that commit
  doesn't change which markers simplify to the `TRUE` tautology — checked
  directly by probing several string-comparison marker shapes against
  both tags, all agreeing on `is_true()`.



Compared to reroll, several whole subsystems collapse to nothing:

| reroll subsystem | Why it's gone |
|---|---|
| `NameMapper`/`open()`/`close()` chain, `AggregatorMapper`, votes, `Candidate`/`Winner` provenance | Not ported at all in this pass — see "Deferred: name mapping" below. When it lands, it'll be one static table, not a chain: no sqlite, no network, no `open()`/`close()` lifecycle to manage. |
| `python_latest_release.py` (endoflife.date network fetch + cache), `abi3_upper_bound` | This existed to know where to *stop* enumerating minors for `python_version in "<literal>"`. `uv_pep508` parses that marker shape into `MarkerExpression::VersionIn { key, versions: Vec<Version>, operator }` **already split into concrete `Version`s** at parse time — there is no open-ended literal string to bound. No network call, ever, for this feature. |
| `conda_package_name.py`'s CEP-26 name-length/charset regex | `rattler_conda_types::PackageName::try_from` already validates CEP-26 grammar and length on construction. Validation is a side effect of using the typed constructor, not a separate hand-rolled check. |
| Wheel filename/tag parsing, `WheelMetadata`, `wheel_record.py`, subdir-specific repodata records | Not our problem — ana converts a project's declared requirements, it doesn't generate repodata for arbitrary wheels. |

## Deferred: name mapping

For this pass, `ana-pep508-to-matchspec` does **no** PyPI→conda name
translation. The conda `PackageName` used in every produced `MatchSpec` is
just `requirement.name` (already PEP 503-normalized by `uv_normalize`)
run through `rattler_conda_types::PackageName::try_from` for CEP-26
validation — i.e. the identity mapping, same normalized string on both
sides. This is deliberately wrong for any PyPI package whose conda name
differs from its normalized PyPI name (the entire reason reroll's mapper
chain exists), but it's the right thing to build against *first*: every
other piece of this design (version-specifier conversion, the two-pronged
marker conversion, the pyproject.toml structural layer) is fully
exercisable and testable without it, and bolting on real name mapping
later is a single-function swap, not a redesign.

The call site itself still holds exactly as sketched in the original
version of this doc: a
`fn map_name(&uv_normalize::PackageName) -> Result<rattler_conda_types::PackageName, InvalidCondaNameError>`,
falling back to the identity mapping (what we're using unconditionally
today) whenever a name isn't in the table. What backs that function has
changed, though: **not** a compile-time `phf::Map` as originally sketched
here. The PyPI→conda name diffs aren't static in the way a fixed conda
subdir list is — packages get renamed/added on the conda side on an
ongoing basis independent of `ana` releases, and baking the table in at
build time would mean every affected package resolves wrong until the next
`ana` release ships. See `investigations/pypi_conda_map.md` for the
crate that replaces it: `ana-pypi-conda-map`, which fetches the table from
an internal API, caches it to disk as MessagePack, and loads it into
memory at process start — synchronously, network-free on the hot path in
the common case (see that doc's "Hot path stays synchronous and
network-free"), so the call site here still sees a plain, already-in-memory
`HashMap` lookup with no `open()`/`close()` lifecycle to manage, same as
the `phf` version would have provided, just backed by data that can
actually stay current.

## The two-pronged marker conversion

This is the part of the design directly answering the "fast path vs.
markerpry partial-solve" question.

### Fast path: direct structural rewrite, no `Environment` needed

reroll's `marker_conversion.py` already proves that most marker shapes
convert via a pure, context-free, node-type-driven rewrite: no knowledge of
the target platform is needed to turn `sys_platform == "linux"` into
`__linux`, or `python_version >= "3.9"` into `python>=3.9.0a0`. These
conversions are universal facts about the *literal values already in the
marker*, not facts that depend on which environment we're solving for.

```rust
/// Attempt a direct, context-free conversion. No Environment, no partial
/// solve, no target-platform knowledge. This is the path ~99% of markers
/// take (mirroring reroll's own <0.1% marker-related failure rate).
fn try_fast(marker: MarkerExpression) -> Result<MatchSpecCondition, Unconvertible>;
```

Exhaustive input coverage, enumerated directly from `uv_pep508`'s own
`MarkerExpression` (5 variants) crossed with its typed marker-key enums
(3 version keys, 14 string keys incl. legacy aliases, `extra`) — see
`docs/marker_expression_table.md` (to be written once implementation
starts) for the full case table. The load-bearing cases:

- `MarkerExpression::Version { key: PythonVersion | PythonFullVersion |
  ImplementationVersion, specifier }` → `python<op><version>` per the
  Operator conversion table reroll already validated (10
  `uv_pep440::Operator` variants to cover, one more than reroll's Python
  had to handle since `Operator` also spells out `EqualStar`/`NotEqualStar`
  as distinct variants rather than a `.*`-suffix string check).
- `MarkerExpression::VersionIn { key: PythonVersion, versions, operator }`
  → an `Or`/`And` chain of `python==<v>` / `python!=<v>` terms, **one per
  already-parsed `Version`** — no minor-enumeration, no upper bound, no
  reachability into markerpry's rewrite machinery at all. This whole case
  is *simpler* in Rust than in reroll, not just ported.
- `MarkerExpression::String { key: SysPlatform | OsName | PlatformSystem,
  operator: Equal, value }` → the `__linux`/`__osx`/`__win`/`__unix`
  virtual-package leaf table, `Equal` only (same restriction reroll
  enforces — `!=` against these has no matchspec equivalent, becomes
  `Unconvertible`, eligible for the slow path below).
- `MarkerExpression::String` with any other key (`platform_machine`,
  `platform_release`, `platform_version`, `implementation_name`,
  `platform_python_implementation`, and their deprecated aliases) →
  always `Unconvertible` here — no matchspec equivalent *in isolation*,
  but see the slow path.
- `MarkerExpression::String`/`VersionIn`/`List` with `In`/`NotIn` (generic
  substring or list-membership test, not the `python_version in
  "<versions>"` special case above) → always `Unconvertible`, same as
  reroll (`ContainsNode` → `UnconvertableMarkerError`). No matchspec
  equivalent to a substring test exists.
- `MarkerExpression::Extra { .. }` → always `Unconvertible`. Same reasoning
  reroll's `pep508_to_matchspec` uses: `extra` is the *current package's*
  own extras mechanism, not an environment condition, and it should never
  reach this function in the first place — the caller (see below) checks
  for and strips `extra` clauses before ever calling into marker
  conversion, exactly where reroll's `pep508_to_matchspec.py` checks
  `"extra" in marker_node` up front.

Combinators (`and`/`or`) recurse structurally: convert both sides, and
build the corresponding `MatchSpecCondition::And`/`Or` directly — this is
where "never format-then-reparse" matters most, since a naive port would
recursively build strings and nest parens, then reparse the whole thing.
We just nest the enum.

### Slow path, take 1 (superseded): a per-subdir `CondaTarget` loop

**This subsection describes a design that was never built and is now
superseded for the invocation mode `ana` actually has — see "Slow path,
take 2" just below for what's implemented.** It's left in place because
it's the right shape for a *different*, not-yet-needed invocation mode
(producing one portable matchspec/lockfile that has to remain valid
across every subdir `ana` supports, the way a `uv.lock` universal
resolution or a noarch conda package's own metadata does), and that
distinction — one target we already know vs. many targets we don't —
turned out to matter enough to change which upstream API the two modes
each need.

The fast path fails on exactly one *class* of input: a marker referencing
a key with no matchspec equivalent in isolation (`platform_machine`,
`platform_release`, `implementation_name`, `platform_python_implementation`,
...) — or a comparator unsupported for a key that otherwise has one
(`!=` against `sys_platform`). For a *portable* matchspec, none of these
keys' values are fixed — the whole point is that the matchspec has to
stay valid on every subdir `ana` supports, so this mode would need to loop
over every subdir it might ever run on (`conda_targets()`, one
`CondaTarget` per subdir) and emit an `Or` of per-subdir arms, each
prefixed by that subdir's own virtual-package leaf (`__linux`/`__osx`/
`__win`). That loop, and the `CondaTarget` struct backing it, is real
and well-motivated design work for that mode — it's just not the mode
`ana` runs in for a live, single-machine install, so it isn't implemented
today (no `CondaTarget`, no `conda_targets()`, no per-subdir loop exist in
`ana-marker-matchspec`).

### Slow path, take 2 (implemented): single-target `restrict()`

`ana` doesn't produce a portable matchspec at all: it installs a
dependency onto *the machine it's currently running on*. That changes the
question from "what's true on every subdir I might ever target" to "what's
true on the one subdir I'm targeting right now" — and unlike the portable
case, that subdir's `sys_platform`/`os_name`/`platform_system`/
`platform_machine` (plus, by policy, `implementation_name`/
`platform_python_implementation` — CPython is the only supported
interpreter) are not just *known*, they're *fixed for the lifetime of the
process*. Only `python_version`/`python_full_version`/
`implementation_version` stay free — that's the solver's job, not ours.

That's exactly the shape `uv_pep508::MarkerTree::restrict` is for — and
unlike the first draft of this doc, this method is no longer a claim
checked against the wrong crate version. It's confirmed, by reading the
actual pinned source at `uv-pep508` `0.12.6` (this workspace's current
pin, per the "Bump history" above), not assumed from a docs.rs page for a
version never pinned here:

```rust
/// Restrict this marker by assuming that `assumption` is true.
///
/// The returned marker is equivalent to this marker wherever `assumption` is true, but may
/// have a different value outside of that context. Before evaluating the simplified marker,
/// callers should conjoin `assumption` to restore its standalone meaning.
///
/// For example, restricting
/// `sys_platform == 'linux' and python_version < '3.11'` under the assumption
/// `sys_platform == 'linux'` produces `python_version < '3.11'`.
#[must_use]
pub fn restrict(self, assumption: Self) -> Self;
```

It was **not** public at the `0.9.7` tag this workspace started on — see
the "Bump history" note above and `ana-marker-matchspec`'s own module
docs for the full trace (checked directly against the crate's git
history back to the commit that introduced the whole ADD implementation:
`restrict` was `pub(crate)`-only there, and stayed that way through
`0.11.0`; the public, assumption-taking `restrict(self, assumption: Self)`
first appears at `0.12.0`, unchanged through `0.12.6`). This is exactly
why the workspace `Cargo.toml`'s bump to `0.12.6` (see "Bump history")
matters for markers specifically, not just for the panic/correctness
fixes it also picked up.

`MarkerTree::restrict`'s own unit test (`uv-pep508`'s `tree.rs`) *is* the
single-target scenario almost verbatim — a disjunction of
`platform_machine`/`sys_platform` pairs (one per subdir) restricted down
to a bare `python_version < '3.11'` residual — so the crate ships test
coverage for the exact shape this design leans on, not just the API
signature.

Because the single target is fixed for the whole process, there's no
per-subdir loop and no `virtual_leaf` re-conjoining step at all —
`restrict()` does the entire job in one call, and its result either
needs no further conversion (`is_true`/`is_false`), or only ever
references the free `python_version` family, which the fast-path leaf
table (below) already handles:

```rust
/// A dependency's applicability to the one machine `ana` is installing
/// onto -- distinct from `Unconvertible`, which means "we don't know how
/// to represent this," not "we know, and the answer is no."
pub enum Applicability {
    /// The marker holds unconditionally on this machine; no `when=` clause
    /// is needed.
    Always,
    /// The marker holds only when the given condition (over
    /// `python_version`/`python_full_version`/`implementation_version`,
    /// the only keys left free) also holds.
    Conditionally(MatchSpecCondition),
    /// The marker can never hold on this machine (e.g. `sys_platform ==
    /// "win32"` while installing on Linux) -- the caller drops the
    /// dependency entirely rather than emitting an always-false matchspec.
}

/// One conda subdir's fixed marker facts, as a `MarkerTree` assumption --
/// built once per process (it's a pure function of the subdir), reused
/// via `MarkerTree`'s `Copy` handle for every dependency. Built from typed
/// `MarkerExpression::String { key, operator: MarkerOperator::Equal, value }`
/// leaves folded with `.and()`, never a formatted-then-reparsed string --
/// see the headline finding above, now extended to assumption-building,
/// not just leaf conversion.
pub fn known_values_assumption(subdir: Platform) -> MarkerTree;

pub fn to_matchspec_condition(
    marker: MarkerTree,
    assumption: MarkerTree,
) -> Result<Applicability, Unconvertible> {
    if marker.is_true() {
        return Ok(Applicability::Always);
    }
    let residual = marker.restrict(assumption);
    if residual.is_true() {
        return Ok(Applicability::Always);
    }
    if residual.is_false() {
        return Ok(Applicability::Never);
    }
    // residual only ever references python_version/python_full_version/
    // implementation_version (the free variable), or a key deliberately
    // left out of `assumption` (platform_release/platform_version) --
    // both fall through to the same fast-path leaf table `try_fast_tree`
    // already describes, via `to_dnf()`.
    try_fast_tree(residual).map(Applicability::Conditionally)
}
```

**What's deliberately *not* in `assumption`**: `platform_release`/
`platform_version` (the OS kernel release/build strings) — real,
per-machine facts, but ones with no matchspec equivalent even once known,
and rare enough in practice (reroll's own fast-path table already treats
them as always-unconvertible) that probing them (a raw `uname()` call,
same shape `rattler_virtual_packages` already makes for a different
purpose) isn't worth it for now. Leaving them out of `assumption` rather
than treating them as an error at assumption-build time means `restrict()`
still simplifies every *other* clause in a marker that happens to also
mention one of these keys — the marker just surfaces in the residual
un-eliminated, and the existing leaf table's "no matchspec equivalent"
`Unconvertible` case catches it there, same as it always would have.

**On `restrict()`'s "may have a different value outside of [the]
context" caveat**: this matters for uv's own resolver-forking use case
(the same restricted marker can get reused across forks with different
assumptions), but not here — the residual only ever becomes a matchspec
`when=` clause that rattler evaluates while solving for this exact same
machine, so `assumption` is permanently true in every context the
residual is ever evaluated in again. This is a load-bearing claim, not an
incidental one, so it's backed by its own test category (not just
asserted in this doc) — see `ana-marker-matchspec`'s test suite,
specifically the `restrict_semantics` module, which checks the
`simplified.and(assumption) == marker.and(assumption)` identity `restrict`'s
own upstream test uses (i.e., re-conjoining the assumption always
reconstructs something equivalent to the original marker-under-that-
assumption) across a deliberately wide sweep of marker shapes — known-key
equalities/inequalities/orderings, disjunctions and conjunctions mixing
known and free keys, `extra` clauses coexisting with environment clauses,
and the two "deliberately excluded" keys (`platform_release`/
`platform_version`) appearing alongside otherwise-resolvable clauses —
rather than trusting the single example in `restrict()`'s own doc comment
to generalize.

### Where the take-1 target list would come from (still superseded)

The rest of this subsection continues "Slow path, take 1" above — kept
for the same reason: real design work for the portable-matchspec mode,
not something the single-target implementation uses. Worth being
explicit about, since "the fixed, small set of subdirs ana solves for"
hand-waves over a real question: how is that list decided, and does
deciding it cost anything?

`CondaTarget.assumption`/`virtual_leaf` themselves are **definitional
constants** — `platform_machine == "x86_64"` for `linux-64` is true by
the definition of what "linux-64" *means*, on any machine, not something
probed from whatever host `ana` happens to be running on right now. This
is the same table reroll's `_SUBDIR_PLATFORM` dict already is: hardcoded,
zero I/O, no syscalls, no file reads. There is a genuinely slow kind of
"query subdir info" in the conda ecosystem — `rattler_virtual_packages`
detecting the *actual* glibc/CUDA/macOS-SDK version present on the running
host does real syscalls and file probing, and that can take measurable
(if still small) wall time. But it's irrelevant here: PEP 508 has no
marker key for glibc or CUDA at all (no `__glibc`/`__cuda` equivalent
exists in the marker grammar), so this crate never needs that data,
regardless of how slow it is elsewhere in `ana`'s eventual solve step.

What's still open is *which subset* of the fixed platform list `targets`
should actually be — always every platform ana knows about, or just the
ones the project declares it cares about (a `platforms = [...]` list in
`pyproject.toml`/`[tool.ana]`, pixi-style). Both of those inputs would be
already fully resolved, synchronously, before a single dependency gets
converted — no future/promise/background-fetch step for `targets` to
ever be waiting on, same reasoning as `known_values_assumption`'s own
zero-I/O construction in the implemented (take 2) design above.

```rust
fn try_slow(marker: MarkerTree, targets: &[CondaTarget]) -> Result<MatchSpecCondition, Unconvertible> {
    let mut arms = Vec::new();
    for target in targets {
        let restricted = marker.restrict(target.assumption);
        if restricted.is_false() { continue; }             // dependency doesn't apply on this subdir
        if restricted.is_true() {
            arms.push(target.virtual_leaf.clone());          // applies unconditionally on this subdir
            continue;
        }
        // Still has real content (almost always just python_version clauses
        // at this point) -- convert it with the SAME fast-path table, then
        // re-conjoin the subdir's own virtual-package predicate, since
        // `restrict` strips the assumption out of the result.
        let remainder = try_fast_tree(restricted)?;          // may itself fail -> Unconvertible
        arms.push(and(target.virtual_leaf.clone(), remainder));
    }
    match arms.len() {
        0 => Ok(MatchSpecCondition::MatchSpec(Box::new(MatchSpec::NEVER))), // false on every target we support
        _ => Ok(or_all(arms)),
    }
}
```

`try_fast_tree` here is `try_fast` lifted from one `MarkerExpression` to a
whole `MarkerTree` via `to_dnf()` — a `Vec<Vec<MarkerExpression>>` that
maps directly onto `Or(And(...))`, which is *why* `MatchSpecCondition`
having exactly `And`/`Or`/leaf (no `Not`) isn't a limitation: DNF form has
already pushed every negation down to individual `MarkerExpression`
leaves (uv-pep508's `MarkerOperator::negate()` handles that during
`to_dnf()`/`restrict()` internally), so by the time we see a DNF clause,
there's nothing left to negate — every leaf is already in its
already-negated-if-needed form (`!=` instead of `not(==)`, etc.). This
`to_dnf()`-then-per-leaf pattern is exactly what the implemented
`try_fast_tree` (take 2, referenced above) does too — it's the one piece
of machinery both modes share.

Both branches would be pure CPU-bound functions over `Copy` values
(`MarkerTree` is `Clone + Copy` — it's an interned handle, not an owned
tree) with no I/O and no shared mutable state, same as the implemented
orchestration above — nothing about either mode is async or needs a
lock.

## Reusable state: what gets built once

This is the concrete answer to "what parser state could be set up once,
then reused":

| State | Built | Lifetime | Why it's safe to share |
|---|---|---|---|
| `conda_targets()` (assumption `MarkerTree`s + virtual-package leaves, one per subdir) | first call (`std::sync::LazyLock`) | process | `MarkerTree` values are `Copy`; building ~6-8 small `and`-chains once and cloning the handle afterward is free. |
| `MatchspecConverter` (below) | once per `ana run` invocation | invocation | Holds only `Arc<[CondaTarget]>` + an `allow_pre: bool` flag — a few bytes plus one refcounted pointer, trivially `Clone`/`Sync`. |
| `uv_pep508`'s own `MarkerTree` interner | internal to the crate (backed by `boxcar`, a lock-free append-only vec, per its `Cargo.toml`) | process | Not ours to manage — but it means identical marker clauses repeated across many dependencies in one `pyproject.toml` (e.g. `python_version >= "3.9"` showing up 50 times) automatically dedupe at zero cost to us, and it's already safe to hit from multiple threads concurrently, since uv's own resolver parses requirements in parallel this way. |
| Extras name validator (CEP-29 `[a-z0-9_.+-]{1,64}`) | *never* — no state | n/a | Recommend a hand-rolled byte-class scan over the `regex` crate here: this check is a simple ASCII char-class + length test called on every extra of every requirement, and skipping regex entirely (no compiled pattern to even amortize) is strictly cheaper than "compile once, run many," which is already what reroll does at the Python module level. |

| Name mapping (`ana_pypi_conda_map::load`) | once per `ana run` invocation | invocation, refreshed on disk between invocations | Not a `phf`/`static` table — see "Deferred: name mapping" above and `investigations/pypi_conda_map.md`. Loaded from an on-disk cache at process start, synchronous and (in the common case) network-free; the returned `HashMap` is plain, owned, and safe to share the same way `conda_targets()`'s `MarkerTree` handles are. |

Explicitly **not** carried over: reroll's `NameMapper.open()`/`close()`
lifecycle. That abstraction exists in reroll to manage per-process state
for mappers backed by a sqlite cache or a network client. Name mapping in
this design — even once built — has neither, so there's no lifecycle to
manage at all, whether it's the identity mapping we use today or the real
static table later.

## API sketch per crate

### `ana-marker-matchspec`

**Implemented**, per "Slow path, take 2" above — the single-target
`restrict()` design, not the take-1 `CondaTarget`/multi-subdir sketch
this section used to show (kept as historical prior art above, not
reproduced here since it was never built).

```rust
/// This machine's known marker facts, as a `MarkerTree` assumption --
/// built once per process from `subdir`, a pure function with no I/O.
pub fn known_values_assumption(subdir: rattler_conda_types::Platform) -> uv_pep508::MarkerTree;

pub enum Unconvertible {
    NoMatchspecEquivalent { key: String, detail: String },
    InLikeTest { detail: String },                     // in/not in, no equivalent
    AlwaysConstant { value: bool },                     // reroll's
                                                          // UnconvertablePythonVersionEqualityError
    ExtraMarker,                                         // `extra == "..."` reached this layer
}

/// A dependency's applicability to the one machine `ana` is installing
/// onto. Distinct from `Unconvertible`, which means "we don't know how to
/// represent this" -- `Never` means "we know, and the answer is no,"
/// which the caller should treat as "drop this dependency," not an error.
pub enum Applicability {
    Always,
    Conditionally(rattler_conda_types::MatchSpecCondition),
    Never,
}

pub fn to_matchspec_condition(
    marker: uv_pep508::MarkerTree,
    assumption: uv_pep508::MarkerTree,
) -> Result<Applicability, Unconvertible>;
```

### `ana-pep508-to-matchspec`

```rust
/// Built once per `ana run` invocation (after the target platform list is
/// resolved from project config / `Platform::current()`, per "Where the
/// target list comes from" above), then shared across every dependency
/// conversion. `Arc`, not `&'static`, because `targets` may be a project-
/// filtered subset of `conda_targets()`'s full list, not the whole thing.
#[derive(Clone)]
pub struct MatchspecConverter {
    targets: Arc<[ana_marker_matchspec::CondaTarget]>,
    allow_pre: bool,
}

impl MatchspecConverter {
    pub fn new(targets: Arc<[ana_marker_matchspec::CondaTarget]>, allow_pre: bool) -> Self;

    /// One requirement -> one MatchSpec. No string formatting on the
    /// happy path except the version leaf (see above).
    pub fn convert(
        &self,
        requirement: &uv_pep508::Requirement,
    ) -> Result<rattler_conda_types::MatchSpec, ConvertError>;

    /// Rayon-parallel batch conversion. Index-aligned with `requirements`
    /// so callers can report every failure, not just the first.
    pub fn convert_all<'a>(
        &self,
        requirements: impl rayon::iter::IntoParallelIterator<Item = &'a uv_pep508::Requirement>,
    ) -> Vec<Result<rattler_conda_types::MatchSpec, ConvertError>>;
}

pub enum ConvertError {
    DirectUrl,                       // `name @ url` -- out of scope, same as reroll
    LocalVersionLabel(String),       // `1.0+cpu` -- no conda equivalent
    Prerelease(String),              // pre-release version, allow_pre unset
    ExtraTooLong(String),            // >64 chars once normalized (CEP-29)
    InvalidCondaName(InvalidCondaNameError),
    UnconvertibleMarker(Unconvertible),
}
```

`convert` implementation shape (no string round-trip except the version
leaf, per the headline finding):

1. If `requirement.marker.top_level_extra_name().is_some()` or, more
   generally, the marker contains any `extra` clause at all — reject with
   `ConvertError::UnconvertibleMarker(Unconvertible::ExtraMarker)`, mirroring
   reroll's up-front `"extra" in marker_node` check. (`uv_pep508::MarkerTree`
   has both `visit_extras`/`top_level_extra` for this and a
   `without_extras`/`only_extras` pair we are *deliberately not* using here
   — those exist for uv's own extras-simplification, which is a different
   problem: our marker either has no `extra` reference or we reject the
   whole requirement, we don't try to partially resolve it.)
2. Reject `requirement.version_or_url` being a URL.
3. Validate `requirement.name` (already PEP 503-normalized by
   `uv_normalize`) as a conda `PackageName` via
   `rattler_conda_types::PackageName::try_from` — the identity mapping (see
   "Deferred: name mapping" above). Fires `ConvertError::InvalidCondaName`
   on CEP-26 violations (e.g. the >64-char SEO-spam-name case). This is the
   one call site a future real name-mapping table replaces.
4. Build `VersionSpec` from `requirement.version_or_url`'s
   `VersionSpecifiers`, per-specifier, rejecting local-version-label and
   (unless `allow_pre`) prerelease specifiers — same table reroll's
   `matchspec_specifier.py` already validated, ported to consume
   `uv_pep440::Operator`'s 10 variants (one more explicit variant,
   `EqualStar`/`NotEqualStar`, than reroll had to special-case via string
   suffix checks).
5. `requirement.extras` (already a typed `Box<[ExtraName]>`, already PEP
   503-normalized) → validate each against the CEP-29 length/charset rule
   → `MatchSpec.extras: Some(Vec<String>)` directly. No bracket-string
   formatting.
6. `ana_marker_matchspec::to_matchspec_condition(requirement.marker, self.targets)`
   → `MatchSpec.condition` directly.
7. Assemble the `MatchSpec` struct literal and return it. No parse step.

### `ana-pyproject`

Per `investigations/pyproject_toml.md`'s already-decided scope rules
(reject `dynamic` dependencies, reject legacy Poetry without `[project]`,
validate PEP 735 group names/cycles). One design decision worth recording
here: **we do not depend on the existing `pyproject-toml` crate**
(`docs.rs/pyproject-toml`, PyO3-maintained, does handle PEP 621 + PEP 735
including self-referential-extra resolution) even though it exists and is
directionally what we need, because it's built on the frozen,
no-longer-updated `pep440_rs`/`pep508_rs` crates (the exact ones
`investigations/reroll_deps.md` already flagged as superseded by uv's
internal crates). Taking it on would mean parsing every requirement string
*twice* — once internally by that crate's `pep508_rs::Requirement`, once by
us via `uv_pep508::Requirement` for actual matchspec conversion — for two
independent, non-interoperable marker ASTs. Instead:

Note this diverges from pixi's own choice for this exact job, on purpose:
pixi depends directly on the `pyproject-toml` crate (0.13.7, confirmed in
`pixi`'s root `Cargo.toml`) to parse `[project]`'s PEP 621/735 fields —
`dependencies`, `optional-dependencies`, `dependency-groups`, including
`include-group` expansion and cycle handling — and only uses `toml_edit`
(via its own `pixi_toml`/`pixi_toml_edit` helper crates) for its
`[tool.pixi]` DSL, which has no PEP 621/735 equivalent to delegate to.
That means we get no free ride on pixi's PEP 735 handling here: cycle
detection, `include-group` expansion, and self-referential extras all
have to be implemented in `ana-pyproject::load` itself, per
`investigations/pyproject_toml.md`'s scope. We diverge anyway because
`pyproject-toml` hands back `pep440_rs`/`pep508_rs::Requirement` values —
parsing every requirement string a second time via `uv_pep508::Requirement::from_str`
just to get the type we actually need is the exact double-parse this
design avoids everywhere else.

```rust
pub struct ProjectRequirements {
    pub runtime: Vec<uv_pep508::Requirement>,
    pub extras: indexmap::IndexMap<uv_normalize::ExtraName, Vec<uv_pep508::Requirement>>,
    pub groups: indexmap::IndexMap<uv_normalize::GroupName, Vec<uv_pep508::Requirement>>,
}

pub fn load(path: &Path) -> Result<ProjectRequirements, PyprojectError>;

pub enum PyprojectError {
    NoProjectTable,
    DynamicDependencies,             // `dynamic` contains "dependencies"/"optional-dependencies"
    LegacyPoetryWithoutProject,
    DuplicateGroupName(String),      // PEP 735 name-normalization collision
    IncludeGroupCycle(Vec<String>),
    UnrecognizedGroupEntry,          // PEP 735: error, don't silently skip
    InvalidRequirement { raw: String, source: uv_pep508::Pep508Error },
}
```

`load` parses the TOML structure itself (via `toml_edit`, matching pixi's
own choice for the same job) and, for every leaf PEP 508 string it finds —
across `dependencies`, every `optional-dependencies` list, and every
expanded `dependency-groups` entry — parses it **once**, here, via
`uv_pep508::Requirement::from_str`. `ana-pep508-to-matchspec` never
re-parses a requirement string; it only ever receives already-typed
`Requirement` values. Parsing PEP 735 `{include-group = "..."}` expansion
happens before this point returns — `groups` in the result is already
fully expanded (positionally, undeduplicated, per the investigation's
already-decided semantics), so downstream code just sees flat
`Vec<Requirement>` per group.

## Threading

Two independent, both-embarrassingly-parallel stages, both safe to run on
`rayon`'s work-stealing pool since neither touches shared mutable state
(only the static target list and `uv_pep508`'s own internally-synchronized
interner today; a future static name table would join that same "safe to
share" category — see "Deferred: name mapping"):

1. **String → `Requirement` parsing**, inside `ana-pyproject::load`. A
   `pyproject.toml` with several `dependency-groups` easily has 100+ raw
   requirement strings; each `Requirement::from_str` call does real work
   (tokenizing, marker-tree construction/interning) independent of every
   other string. Parallelize with `par_iter` over the flattened list of
   raw strings collected during TOML walking, before repackaging results
   back into the `runtime`/`extras`/`groups` structure (order doesn't
   matter for `runtime`/each extra/each group's *conversion*, but we
   should preserve source order in the returned `Vec`s regardless, since
   `rayon`'s `par_iter().collect()` preserves index order for free — no
   ordering tax for going parallel here).
2. **`Requirement` → `MatchSpec` conversion**, inside
   `MatchspecConverter::convert_all`. Same shape, same justification.

Deliberately *not* parallelized further: the fast-path/slow-path decision
*within* one `convert` call. Both branches are cheap, sequential-dependent
(only attempt the slow path if the fast path fails), and CPU-only with no
I/O — spawning more tasks at that granularity would add scheduling
overhead without exposing any real additional parallelism (there's nothing
inside `to_matchspec_condition` for one requirement that's independent
work relative to itself).

`rayon`'s default work-stealing scheduler already handles small inputs
(a `pyproject.toml` with 5 dependencies) reasonably cheaply, so v1 should
just always call `par_iter()` rather than hand-rolling a
"parallelize only above N items" threshold — that threshold, if one turns
out to be needed at all, should come from a `criterion` benchmark once we
have a real corpus of `pyproject.toml` files to measure against, not from
a guess baked into the API now.

`MatchspecConverter` itself is `Clone` and holds nothing but a `'static`
slice reference and a `bool`, so callers needing to share it across threads
can freely clone one per task without contention. To be precise about what
that *isn't* for: a single `ana run --group dev --group docs` invocation is
**one** solve, not two or three — per `investigations/pyproject_toml.md`,
multiple `--group` flags are purely additive, so the caller unions
`runtime` + `groups["dev"]` + `groups["docs"]` into one flattened
`Vec<Requirement>` and makes one `convert_all` call over the combined
batch (more items in the same `par_iter`, not a new parallelism axis).
Where `Clone` actually earns its keep is genuinely independent
environments — e.g. a future lockfile stage that solves several *named*
environments in one pass (`default`, `default+dev`, `default+test`, each
a different group union, each its own lock target) can run those
conversions concurrently, one `MatchspecConverter` clone per environment.
That's a future lockfile-stage concern, not something this crate needs to
decide, but the API shouldn't foreclose it.

## Error model summary

Two independent error hierarchies at two layers, deliberately not unified
into one enum (mirrors reroll's own category split, adapted):

- `ana_pyproject::PyprojectError` — a *structural* problem with the
  `pyproject.toml` itself (wrong shape, `dynamic`, cycles). One of these
  means the whole project is out of scope; `ana run` should fail before
  ever attempting a single matchspec conversion.
- `ana_pep508_to_matchspec::ConvertError` — a problem with one specific
  dependency's *value* (its marker, version, or extras). `convert_all`
  returns `Vec<Result<MatchSpec, ConvertError>>`, index-aligned with its
  input, specifically so a caller can report every failing dependency in
  one pass (`ana run` needs the whole environment to succeed, but the
  error message should say *all* of what's wrong, not just the first
  thing hit) rather than fail-fast on the first `Err`.

## Testing strategy (not yet implemented, noted for follow-up)

- reroll's own test suite (3,438 lines, described in `reroll_deps.md` as
  "an executable spec") is the correctness oracle for the marker
  conversion table and the version-specifier table. Port its test
  *cases* (inputs + expected matchspec strings), not its test *code* —
  compare against `matchspec.to_canonical_string()` /
  `matchspec.to_string()` rather than hand-formatted strings, since our
  construction path never formats a string to compare against in the
  first place.
- `MarkerExpression`'s exhaustiveness (5 variants, 3 version keys, 14
  string keys, `extra`) should be enforced with a `match` that has no
  wildcard arm anywhere in `try_fast` — a future `uv-pep508` bump adding a
  new marker key (PEP 751 already added `List`/`extras`/`dependency_groups`
  recently) should be a compile error here, not a silent gap.
- Need a real-world corpus: several hundred `pyproject.toml` files from
  popular PyPI projects, run through `ana-pyproject::load` +
  `MatchspecConverter::convert_all`, tracking the fast-path/slow-path/
  hard-error split the same way reroll's README tracks its own conversion
  stats — this validates the "most dependencies solve via the fast path"
  speculation this design is built around, instead of leaving it a guess.
- `restrict()`'s "may have a different value outside of [the assumption]"
  caveat (its own doc comment, quoted above) is asserted safe for this
  workspace's usage in "Slow path, take 2," but that's a claim about
  *how the residual is used downstream*, not something `restrict()`
  itself guarantees — so it needs its own test coverage, not just the
  doc's reasoning. `ana-marker-matchspec`'s `restrict_semantics` test
  module checks, for a wide sweep of marker shapes (known-key
  equalities/inequalities/orderings, disjunctions and conjunctions mixing
  known and free keys, `extra` clauses alongside environment clauses, and
  the deliberately-excluded `platform_release`/`platform_version` keys
  appearing alongside otherwise-resolvable clauses), that
  `marker.restrict(assumption).and(assumption) == marker.and(assumption)`
  — the same identity `restrict()`'s own upstream test relies on — rather
  than trusting the one worked example in the doc comment to generalize
  to every shape this workspace actually produces.

## Open questions to verify once implementation starts

- Whether `uv_pep508::Requirement::from_str` accepts
  `python_version in "2.7.* 3.x"` (non-version-shaped tokens inside an
  `in` literal) as a parse-time `Pep508Error`, versus reroll's Python
  stack, which accepts it as a syntactically valid marker and only rejects
  it later, semantically, as `UnconvertableMarkerError`. If uv rejects it
  earlier, that specific failure mode moves from `ConvertError` into
  `PyprojectError`'s `InvalidRequirement` case instead — a classification
  change, not a coverage gap, but worth confirming against the real crate
  rather than assumed from its doc comments.
- Whether it's worth upstreaming a typed multi-segment constructor to
  `rattler_conda_types::Version` (today: only `Version::major(u64)` or a
  `semver`-shaped bridge) to remove the one remaining
  format-then-`from_str` step. Not blocking for v1 — the version grammar
  it'd parse is small and non-recursive — but worth a profiler check once
  we have real throughput numbers, before deciding whether it's worth the
  upstream PR.
