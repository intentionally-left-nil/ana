//! Where things live on disk for `ana`: the single source of truth for
//! its filesystem layout, shared by every crate that needs to know it.
//!
//! Three pieces of knowledge:
//!
//! - **Environment paths** ([`discover`], [`EnvironmentPaths`]): given an
//!   [`EnvironmentLayout`], which `lock_path`/`env_path` pair it maps to.
//!   [`EnvironmentLayout::ProjectDefault`] is the fixed, unkeyed default
//!   (`<root>/ana.lock`, `<root>/.env`); [`EnvironmentLayout::ProjectKeyed`]
//!   and [`EnvironmentLayout::Global`] are named by an opaque
//!   [`EnvironmentKey`]. *Which* constructor produces that key for a
//!   given invocation (a `--group` selection, ad hoc CLI requirements,
//!   ...) is a decision made above this crate; this crate only turns an
//!   already-decided layout into paths.
//! - **Advisory locks** ([`EnvironmentPaths::advisory_lock_path`]): every
//!   environment's cross-process lock file lives under one shared
//!   `locks/` directory per root (or per global cache), so a single
//!   gitignore rule covers them all and environment recreation (which
//!   may delete `env_path`) can never delete a lock out from under a
//!   holder.
//! - **The global cache root** ([`global_cache_root`]): where a
//!   project-less environment lives, OS-appropriate and independent of
//!   any project root.
//!
//! What happens *inside* those paths (lock generation, environment
//! materialization) is the caller crates' business; this crate's remit
//! ends at the paths themselves. Everything here is pure computation --
//! no filesystem access at all. (A project root is the *caller's* input:
//! `ana` must be invoked from the directory containing `pyproject.toml`
//! -- there is deliberately no walk-up discovery.)
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod cache;
mod environment;
mod key;

pub use cache::global_cache_root;
pub use environment::{discover, EnvironmentLayout, EnvironmentPaths};
pub use key::EnvironmentKey;
