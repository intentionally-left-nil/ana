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
  ana-marker-matchspec/   # the two-pronged marker -> MatchSpecCondition logic,
                           # incl. the fixed conda-subdir target list it needs
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

Also no separate `ana-conda-targets` crate. An earlier draft of this doc
split the fixed conda-subdir list (`CondaTarget`/`conda_targets()`) out on
its own, but that doesn't hold up: it's ~100 lines, it has exactly one
consumer (`ana-marker-matchspec`'s slow path), and its fields
(`assumption: MarkerTree`, `virtual_leaf: MatchSpecCondition`) are
themselves marker-conversion-specific — they're not a generic "which
platforms does ana support" fact (that's just `rattler_conda_types::Platform`,
which already exists as a crate elsewhere) but "what does the *slow path*
need to know per platform." A future consumer that just wants the platform
list reaches for `Platform` directly, not this struct. So it's a module
inside `ana-marker-matchspec`, not a crate: no independent versioning need,
no second consumer, no compilation-isolation win at this size, just an
extra `Cargo.toml` and an extra hop for readers.

Each crate is independently testable and independently useful — in
particular, `ana-pep508-to-matchspec` has no pyproject.toml or TOML concept
at all; it converts one `uv_pep508::Requirement` at a time, same contract
as reroll's `to_matchspec()`, so it can be fuzzed/property-tested against
reroll's own 3,438-line test suite as an oracle without needing any TOML
fixtures.

### Dependency pins

```toml
[workspace.dependencies]
uv-pep508   = { git = "https://github.com/astral-sh/uv", tag = "0.9.7" }
uv-pep440   = { git = "https://github.com/astral-sh/uv", tag = "0.9.7" }
uv-normalize = { git = "https://github.com/astral-sh/uv", tag = "0.9.7" }
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

## What no longer exists (and why)

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

### Slow path: restrict-based partial solve (the "markerpry business")

The fast path fails on exactly one *class* of input: a marker referencing
a key with no matchspec equivalent in isolation (`platform_machine`,
`platform_release`, `implementation_name`, `platform_python_implementation`,
...) — or a comparator unsupported for a key that otherwise has one
(`!=` against `sys_platform`). Every one of these keys' *actual value* is
fully determined the moment we fix two things ana already fixes before
converting anything: **CPython is the only supported interpreter**, and
**the conda subdir being solved for** (which pins `platform_machine` the
same way it pins `sys_platform`/`os_name`/`platform_system`). What's
*not* fixed is `python_version`/`python_full_version` — that's the
solver's job, not ours, and it must remain a free variable.

That's exactly the shape `uv_pep508::MarkerTree::restrict` is for:

```rust
pub fn restrict(self, assumption: Self) -> Self;
```

> Restrict this marker by assuming that `assumption` is true. ... For
> example, restricting `sys_platform == 'linux' and python_version <
> '3.11'` under the assumption `sys_platform == 'linux'` produces
> `python_version < '3.11'`.

This is markerpry's whole job, already implemented, canonical, and
polynomial-time — we don't port markerpry's tree-walking at all, we call
`restrict` on `uv_pep508`'s own BDD-style `MarkerTree` and reuse its
`to_dnf()` for the final leaf-and-recombine step.

```rust
/// One conda subdir's fixed marker environment, expressed as everything
/// `restrict()` needs to know EXCEPT python_version/python_full_version,
/// which stay free.
pub struct CondaTarget {
    pub subdir: Platform,           // rattler_conda_types::Platform
    assumption: MarkerTree,         // platform_system/_machine, sys_platform,
                                     // os_name, implementation_name,
                                     // platform_python_implementation - all `==`
    virtual_leaf: MatchSpecCondition, // __linux / __osx / __win, precomputed
}

/// The fixed, small set of subdirs ana solves for. Built once, reused for
/// every dependency of every project for the lifetime of the process --
/// see "Reusable state" below.
pub fn conda_targets() -> &'static [CondaTarget];
```

### Where the target list comes from (and why nothing here waits on I/O)

Worth being explicit about, since "the fixed, small set of subdirs ana
solves for" hand-waves over a real question: how is that list decided,
and does deciding it cost anything?

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
`pyproject.toml`/`[tool.ana]`, pixi-style), defaulting to
`rattler_conda_types::Platform::current()` (a compile-time-conditional
constant lookup — the binary is built for a specific target, so this
returns a hardcoded enum value, not something it detects at runtime) when
the project doesn't say. Both of those inputs are already fully resolved,
synchronously, before a single dependency gets converted: the platform
list (if any) comes out of the same `pyproject.toml` parse
`ana-pyproject::load` already does, and `Platform::current()` costs
nothing to call. There is no future/promise/background-fetch step for
`targets` to ever be waiting on.

Concretely, that means `MatchspecConverter::new` should take an
already-resolved `&[CondaTarget]` (or an owned `Vec<CondaTarget>` filtered
down from `conda_targets()` by whatever platform list was decided) as a
plain, synchronous argument — never a `Future`/`JoinHandle` the caller has
to await first. There's no "should we delay the slow path until subdir
info is ready" question to answer, because there's no readiness gate:
by construction, `targets` is fully in memory before `MatchspecConverter`
exists at all, and every `restrict()` call the slow path makes afterward
is pure CPU over an already-resolved value.

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
already-negated-if-needed form (`!=` instead of `not(==)`, etc.).

### Orchestration: try fast, fall back to slow, only once

```rust
pub fn to_matchspec_condition(
    marker: MarkerTree,
    targets: &[CondaTarget],
) -> Result<Option<MatchSpecCondition>, Unconvertible> {
    if marker.is_true() {
        return Ok(None); // no `when=` needed at all
    }
    match try_fast_tree(marker) {
        Ok(condition) => Ok(Some(condition)),
        Err(_fast_failure) => try_slow(marker, targets).map(Some),
        // try_slow's own failure (a construct with no matchspec equivalent
        // even after fixing the platform) propagates as the real error --
        // this is reroll's terminal UnconvertableMarkerError case.
    }
}
```

Both branches are pure CPU-bound functions over `Copy` values (`MarkerTree`
is `Clone + Copy` — it's an interned handle, not an owned tree) with no
I/O and no shared mutable state, so nothing about this orchestration is
async or needs a lock.

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

```rust
/// One conda subdir's fixed marker environment -- see "Slow path" above.
/// A module inside this crate, not a separate crate (see "Crate layout").
pub struct CondaTarget { /* ... */ }

/// The fixed, small set of subdirs ana solves for. Built once, reused for
/// every dependency of every project for the lifetime of the process --
/// see "Reusable state" below.
pub fn conda_targets() -> &'static [CondaTarget];    // fixed list: linux-64,
                                                       // linux-aarch64, osx-64,
                                                       // osx-arm64, win-64, win-arm64

pub enum Unconvertible {
    NoMatchspecEquivalent { key: String, detail: String },
    InLikeTest { detail: String },                     // in/not in, no equivalent
    AlwaysConstant { value: bool },                     // reroll's
                                                          // UnconvertablePythonVersionEqualityError
    ExtraMarker,                                         // `extra == "..."` reached this layer
}

pub fn to_matchspec_condition(
    marker: uv_pep508::marker::MarkerTree,
    targets: &[CondaTarget],
) -> Result<Option<rattler_conda_types::MatchSpecCondition>, Unconvertible>;
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
