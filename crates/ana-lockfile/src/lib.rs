//! `ana.lock` generation for `ana`: given a project root and an
//! already-discovered bucket (`lock_path`/`env_path` pair, per
//! `investigations/env_storage.md`), decide whether the bucket's lock file
//! needs regenerating and, if so, regenerate it -- safely under concurrent
//! invocations, across possibly more than one platform, and without
//! dirtying the committed lock file for no-op checks.
//!
//! This crate implements `investigations/lock_generation_algorithm.md`
//! end to end. The design in one paragraph:
//!
//! - **`ana.lock` is committed and changes only when a real resolve
//!   happens.** It is partitioned by platform (`[platforms.<subdir>]`), each
//!   section holding only resolve-time data: the canonical matchspecs the
//!   platform was solved from, `requires_python`, and the full resolved
//!   [`PackageRecord`] set. No staleness bookkeeping lives in the file.
//! - **The stage-1 hash lives in a separate cache file inside `env_path`**
//!   (`pyproject_hash.json`), which is already gitignored and already
//!   single-platform-scoped by directory. A missing/corrupt/stale cache can
//!   only ever cause extra work, never an incorrect answer.
//! - **Three modes.** [`ensure_current_platform`] (default: `ana run`/
//!   `ana install`/`ana sync`) touches only `Platform::current()`'s section
//!   and the cache. [`lock_platform`] (cross-platform: `ana lock
//!   --platform <p>`) always solves exactly one named platform's section
//!   and never touches the environment or cache (unless `p` *is* the
//!   current platform). [`check`] (CI mode) verifies every section present
//!   in the lock plus every declared platform, entirely offline, and can
//!   optionally re-solve stale sections (`--fix`).
//!
//! The solver itself is behind the [`Solver`] trait: no solver crate is in
//! the workspace yet (the investigation's open TODO), so the algorithm is
//! written against the seam and tested with fakes. Wiring in
//! `rattler_solve` (or equivalent) is a separate change that touches only
//! a caller-provided [`Solver`] impl, not this crate.
//!
//! Concurrency: one advisory lock per bucket (`<bucket_dir>/.lock`,
//! `fd-lock`), held across the whole check-or-solve sequence; `ana.lock` is
//! written by re-reading the current file under the held lock, splicing in
//! only the solved platform's section, and atomically replacing the file
//! (tempfile-in-same-directory + `rename`), so a writer for platform A can
//! never discard a concurrent platform B section that landed while A was
//! solving. The cache file needs no read-modify-write (it is a single
//! scalar record, always overwritten whole) but gets the same atomic
//! rename.
//!
//! `pyproject.toml` content and `ana.lock` content are untrusted input, so
//! this crate never `unwrap`/`expect`s its way past a failure outside of
//! tests -- same lint-enforced rule as the rest of the workspace.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod algorithm;
mod cache;
mod error;
mod fs_util;
mod hash;
mod lock_file;
mod matchspec;
mod project;
mod solver;

pub use algorithm::{
    check, ensure_current_platform, lock_platform, Bucket, CheckReport, EnsureOutcome,
    PlatformStatus,
};
pub use error::Error;
pub use lock_file::{LockFile, LockedRequirement, PlatformSection};
pub use project::{Project, SelectedRequirement};
pub use solver::{SolveRequest, Solver, DEFAULT_CHANNELS};
