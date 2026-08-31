//! `ana.lock` generation for `ana`: given a resolved [`ana_environment::Environment`]
//! (its declaration and already-discovered environment paths -- see
//! `ana-environment`), decide whether the environment's lock file needs
//! regenerating and, if so, regenerate it -- safely under concurrent
//! invocations, across possibly more than one platform, and without
//! dirtying the committed lock file for no-op checks.
//!
//! - **`ana.lock` is committed and changes only when a real resolve
//!   happens.** It is partitioned by platform (`[platforms.<subdir>]`), each
//!   section holding only resolve-time data: the canonical matchspecs the
//!   platform was solved from (including a `python` entry derived from
//!   `requires-python`, if any), and the full resolved [`PackageRecord`]
//!   set. No staleness bookkeeping (hashes) lives in the file at all --
//!   staleness is a live set-diff against the environment's declaration.
//! - **`<env_path>/ana.lock` (the "env lock") tracks the environment's own
//!   state**: exactly one platform's section (the one `env_path` is
//!   materialized for) plus a `dirty` bit, local and gitignored (see
//!   [`EnvLock`]). A missing/corrupt env lock is never an error; a
//!   `dirty` one means the last reconcile may have been interrupted, so
//!   the next `ana run` wipes `env_path` recursively rather than trusting
//!   it.
//! - **Three modes.** [`ensure_current_platform`] (default: `ana run`/
//!   `ana install`/`ana sync`) touches only `Platform::current()`'s section
//!   plus the env lock. [`lock_platform`] (cross-platform: `ana lock
//!   --platform <p>`) always solves exactly one named platform's section
//!   and never touches `env_path`, for any platform. [`check`] (CI mode)
//!   verifies every section present in the lock plus every declared
//!   platform, entirely offline, and can optionally re-solve stale
//!   sections (`--fix`).
//!
//! The solver itself is behind the [`Solver`] trait, tested with fakes;
//! `ana-solver`'s `RattlerSolver` is the real implementation.
//!
//! Concurrency: one advisory lock per environment
//! (`<root>/.ana/locks/<key>.lock`, `fd-lock` -- see
//! [`ana_paths::EnvironmentPaths::advisory_lock_path`]), held across the
//! whole check-or-solve sequence; `ana.lock` is
//! written by re-reading the current file under the held lock, splicing in
//! only the solved platform's section, and atomically replacing the file
//! (tempfile-in-same-directory + `rename`), so a writer for platform A can
//! never discard a concurrent platform B section that landed while A was
//! solving.
//!
//! `ana.lock` content is untrusted input, so this crate never
//! `unwrap`/`expect`s its way past a failure outside of tests -- same
//! lint-enforced rule as the rest of the workspace.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod algorithm;
mod env_lock;
mod error;
mod fs_util;
mod lock_file;
mod solver;

pub use algorithm::{
    acquire_environment_lock, check, ensure_current_platform, ensure_current_platform_locked,
    lock_platform, plan_current_platform, plan_platforms, read_lock_section, CheckReport,
    EnsureOutcome, PlatformStatus, SectionPlan, SolveScope,
};
pub use env_lock::EnvLock;
pub use error::Error;
pub use fs_util::{EnvironmentLock, EnvironmentLockGuard};
pub use lock_file::{
    render_sections, LockFile, PlatformSection, RenderedLockFile, LOCK_FILE_VERSION,
};
pub use solver::{SolveRequest, Solver};
