//! Where things live on disk for `ana`: the single source of truth for
//! `investigations/env_storage.md`'s filesystem layout, shared by every
//! crate that needs to know it (lockfile generation today, environment
//! creation and running next).
//!
//! Two pieces of knowledge, both from that doc:
//!
//! - **Environment paths** ([`discover_paths`], [`EnvironmentPaths`]):
//!   which `lock_path`/`env_path` pair an invocation's `--group` flags map
//!   to, given a project root. No flags is the fixed, unhashed default
//!   (`<root>/ana.lock`, `<root>/.env`); any flags is
//!   `<root>/.ana/<hash>/`, where the hash is [`environment_hash`] over
//!   the normalized, sorted, deduplicated group names. The hash is
//!   trusted blindly -- no `selection.toml` sidecar.
//! - **Advisory locks** ([`EnvironmentPaths::advisory_lock_path`]): every
//!   environment's cross-process lock file lives under
//!   `<root>/.ana/locks/`, keyed by environment, so a single gitignore
//!   rule covers them all and environment recreation (which may delete
//!   `env_path`) can never delete a lock out from under a holder.
//!
//! What happens *inside* those paths (lock generation, environment
//! materialization) is the caller crates' business; this crate's remit
//! ends at the paths themselves. Everything here is pure computation --
//! no filesystem access at all. (The project root is the *caller's*
//! input: `ana` must be invoked from the directory containing
//! `pyproject.toml` -- see `env_storage.md`'s amendment history for why
//! there is deliberately no walk-up discovery.)
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod environment;

pub use environment::{discover_paths, environment_hash, EnvironmentPaths};
