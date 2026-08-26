# reroll → Rust port: dependency analysis

Scope: a Rust port of reroll with mappers removed from scope (the
`NameMapper` chain — `conda_lock_mapper`, `grayskull_mapper`,
`overrides_mapper`, `parselmouth_mapper/`, `default_mappers.py`, and the
pluggable machinery in `name_mapping.py`) replaced by a hardcoded
PyPI→conda translation list (a plain lookup table). That drops
`conda-lock`, `grayskull`, `ruamel.yaml`, `platformdirs`, and stdlib
`sqlite3`/`gzip` entirely — none of those need a Rust replacement.
Remaining in-scope source: ~4,000 lines across filename parsing, METADATA
parsing, marker/dependency conversion, and matchspec generation (vs.
~1,556 lines of mapper code being dropped).

## Method

For every proposed Rust crate, usage was verified directly against the
`Cargo.toml` files of `astral-sh/uv`, `prefix-dev/pixi`, and
`conda/rattler` (including `py-rattler`'s own `Cargo.toml`) rather than
inferred from documentation — see "Confirmed usage" per row below.

## Final dependency table

| Python dependency / capability | Used for | Rust replacement | Origin | Used by uv | Used by pixi |
|---|---|---|---|---|---|
| `packaging.utils.parse_wheel_filename` / `packaging.tags.Tag` | Wheel filename → name/version/build/tags | `uv-distribution-filename` (`WheelFilename`) | **uv umbrella** | Direct (own crate) | Direct (git dep, pinned tag) |
| `packaging.utils.canonicalize_name` | PEP 503 name normalization | `uv-normalize` (`PackageName`) | **uv umbrella** | Direct | Direct (git dep) |
| `packaging.version` / `packaging.specifiers` | PEP 440 versions/specifiers | `uv-pep440` | **uv umbrella** | Direct | Direct (git dep) — *and* pixi also directly depends on the standalone, no-longer-updated `pep440_rs` (via `pixi_pypi_spec`) for its own manifest-spec layer |
| `packaging.requirements` + `packaging.markers` | PEP 508 requirement parsing + marker AST/algebra | `uv-pep508` (incl. `MarkerTree`/`MarkerExpression`) | **uv umbrella** | Direct | Direct (git dep) — *and* pixi also directly depends on the standalone `pep508_rs` (via `pixi_pypi_spec`) |
| `packaging.licenses` | SPDX expression canonicalization | `spdx` (EmbarkStudios) | Independent | Direct (`uv-build-backend/Cargo.toml`) | Direct (`pixi_manifest/Cargo.toml`) |
| `packaging.metadata.parse_email` | RFC822-style METADATA header parsing | `uv-pypi-types` (metadata module) | **uv umbrella** | Direct | Direct (git dep) |
| `packaging.metadata` + `zipfile` (`wheel_archive.py`) | Extract & locate METADATA inside the wheel zip | `uv-metadata` | **uv umbrella** | Direct | **Indirect** — not in pixi's own dependency list; pulled in transitively via `uv-distribution` |
| `reroll.lenient_parser` (already a hand-port of uv's Rust code) | Lenient PEP 440/508 fixups | `uv-pypi-types::{LenientRequirement, LenientVersionSpecifiers}` | **uv umbrella** | Direct (it's the literal source reroll's Python port copies) | Direct (via `uv-pypi-types` git dep) |
| `rattler` / `rattler.MatchSpec` (py-rattler binding) | MatchSpec construction/validation | `rattler_conda_types::MatchSpec` | Independent (conda/rattler org) | N/A (uv has no conda concept) | Direct (`pixi/Cargo.toml`) — and confirmed as the exact crate `py-rattler` itself wraps (its `Cargo.toml` path-deps `rattler_conda_types` directly) |
| regex validators (`conda_package_name.py`, `matchspec.py`) | CEP-26/CEP-29 grammar checks | `regex` | Independent | Direct | Direct |
| `pydantic` / `pydantic-core` | Validated data models | *(none — native structs + validating constructors)* | — | — | — |
| `urllib.request`/`tempfile`/`json` (`python_latest_release.py`) | endoflife.date cache fetch | `reqwest` + `std::fs` | Independent | Direct (uv uses `reqwest` throughout) | Direct (`reqwest` in pixi's workspace deps) |
| `markerpry` (reroll's own dependency) | Matchspec-oriented marker rewriting/range-tightening | *(no off-the-shelf equivalent — from-scratch port, but can consume `uv-pep508`'s `MarkerTree`/`to_dnf()` as its parsing/algebra layer instead of reimplementing that part)* | — | — | — |

## Notes

- Every "uv umbrella" row is one of the `0.0.x`-versioned internal uv
  crates (`description: "This is an internal component crate of uv"`) —
  no semver stability guarantee. pixi's own mitigation is to pin them as
  **git dependencies at a fixed uv tag** (currently `0.11.15`) rather than
  track crates.io releases; a reroll Rust port should do the same and
  accept periodic pin-bump maintenance.
- `spdx` and `rattler_conda_types` are the two replacements that are *not*
  uv umbrella crates, and both have direct, first-party confirmation from
  the actual `Cargo.toml` files of uv and/or pixi.
- `pep440_rs`/`pep508_rs` (the pre-uv-internalization standalone crates)
  are real pixi dependencies but are effectively frozen/unmaintained since
  uv absorbed their functionality into `uv-pep440`/`uv-pep508` (last
  updated ~Dec 2024/Jan 2025). Prefer the actively-maintained uv umbrella
  crates pixi itself uses for anything resolution/installation-related.
- `markerpry` and the pydantic-replacement row are the two places with no
  existing crate to point to — one is genuine new engineering, the other
  is just an idiom change with no gap (Rust's native structs +
  constructors substitute for pydantic's runtime validation).

### `packaging.markers` specifically

`uv-pep508`'s marker submodule (`parse.rs`, `tree.rs`, `algebra.rs`,
`simplify.rs`, `lowering.rs`, `environment.rs`) is a full-fidelity PEP 508
marker parser and canonical representation
(`MarkerTree`/`MarkerExpression`/`MarkerValueVersion`/`MarkerValueString`/
`MarkerOperator`), and it is *fully public API* — unlike
`packaging.markers`, whose raw parse tree markerpry's Python
`parser.py` can only reach by digging into the private
`Marker._markers` attribute and the underscore-prefixed
`packaging._parser.{Op,Value,Variable}` module. The Rust port is on
firmer API footing than the Python original here.

One architectural wrinkle: `MarkerTree` is not a plain syntax tree but a
canonical, structurally-shared BDD-style representation (interned nodes,
`to_dnf()`, built-in `and`/`or`/`negate`/`is_disjoint`/`evaluate`,
`simplify_python_versions`/`complexify_python_versions`). A markerpry
Rust port would adapt to this representation (e.g. consuming `to_dnf()`
or walking `MarkerTree::kind()`) rather than transliterating the current
nested-tuple-flattening logic in `parser.py`. `simplify_python_versions`/
`complexify_python_versions` in particular already do something close to
what reroll's `environment.py` and markerpry's own `RangeConstraint`/
`tighten_ranges` do today, which may let a Rust markerpry lean on
`uv-pep508` for generic marker algebra and keep its own scope limited to
the reroll/matchspec-specific rewriting that has no upstream equivalent
at all (the `python_version in "<literal>"` expansion, the
`sys_platform`/`os_name` → conda virtual-package mapping, and the
CEP-29 `when=` string formatting).

## Bottom line

Every parsing primitive reroll needs (wheel filenames, PEP 440/508, SPDX,
core metadata, MatchSpec, marker parsing/algebra) already has a
battle-tested Rust implementation reused by uv and/or pixi — porting
those is dependency-swapping, not re-derivation. The only from-scratch
component is `markerpry`'s matchspec-specific layer, and it's small
(current Python source is 811 lines, with 3,438 lines of tests acting as
an executable spec) and can offload its parsing/algebra needs to
`uv-pep508`. The dominant cost of the port is transcribing reroll's own
~4,000 lines of CEP/wheel-tag business logic (and its test suite) into
idiomatic Rust — a large but mechanical effort, not a research problem.
