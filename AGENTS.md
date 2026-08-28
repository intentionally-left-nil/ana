# AGENTS.md

`ana` is a Rust CLI that provides project-scoped conda environments for
Python projects: it resolves `pyproject.toml`/`requirements.txt`
dependencies (PEP 508, plus conda `MatchSpec` extensions), solves and
locks them against conda channels, and materializes/installs the
resulting environment. The workspace is split into single-concern crates
under `crates/` (parsing, matchspec conversion, solving, installing,
lockfile management, etc.) composed by the `ana` binary crate.

## Commands

- `make build` — build all workspace crates
- `make test` — run all tests
- `make fmt` / `make fmt-check` — rustfmt
- `make clippy` — clippy, warnings as errors
- `make lint` — fmt-check + clippy
- `make check` — type-check without building artifacts
- `make ci` — everything CI runs (lint + test); run this before opening a PR

## Code conventions

- Tests live inline in `#[cfg(test)] mod tests { ... }` at the bottom of
  the file they test, not in a separate `tests/` directory. (`crates/ana`
  and `crates/ana-installer` additionally have `tests/` for end-to-end
  integration tests that need real fixtures.)
- Every crate denies `clippy::unwrap_used`/`clippy::expect_used` at the
  crate root (`#![deny(...)]` in `lib.rs`) and re-allows them only inside
  `#[cfg(test)] mod tests`. Untrusted input (CLI args, `pyproject.toml`,
  `requirements.txt`, network/solve results) must be handled with real
  error types, never `unwrap`/`expect`, outside of tests.
- Within a file, public items come first, in the order a reader would
  want to encounter them; private (`_`-prefixed or non-`pub`) helpers go
  at the bottom.
- Crate-level docs (`//!` in `lib.rs`) give a reader the shape of the
  crate: what it's for, and a one-line pointer to each public module/type
  they'd need next. Module and function docs describe the current
  contract and behavior, not a tour of every internal helper.
- Performance is the highest priority. Be cautious to not clone() objects
  without reason (such as it being a non-hot path, or an external API requires it). 
Prefer lifetimes where possible. Do not re-compute the same data in a loop
  Utilize multithreading where feasible, balancing off thread-pool costs for effectivenes

## Docstrings and comments describe now, not history

A docstring or comment is for someone who has never seen this
conversation and never will. State the current contract/behavior and,
if truly non-obvious, the one invariant a caller could violate or the one
fact not discernable from the code immediately around it. Nothing else.

Do not include: why an earlier approach was rejected, what changed based
on feedback, comparisons to a sibling crate/function's design ("unlike
`x`...", "mirrors `y`'s pattern..."), performance rationale for constants
unless the constant's value would otherwise look arbitrary, or any other
narration of how the code got this way. That's what commit messages and
PR descriptions are for — write it there, once, for a reviewer, not into
the file for every future reader.

A docstring is concise and describes overall behavior, not every edge
case or branch — those are visible in the code itself. If a doc comment
runs past a few lines, or restates something the reader can already see
by looking at the function body, that's a signal to cut it down, not a
sign it's thorough.

## Never suppress, always fix

Do not add `#[allow(clippy::...)]`, `#[allow(dead_code)]`, or any other
lint suppression to silence a failing check outside of the established
`#[cfg(test)] mod tests { #![allow(clippy::unwrap_used, ...)] }` idiom.
A lint failure usually means the code — or the API it's calling — needs
to change. Treat a new suppression as a signal to redesign, not a way to
get `make ci` green.

If you believe a suppression is genuinely the right call, stop and ask
the user for explicit permission before adding it, and say why you think
no fix exists.
