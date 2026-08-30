//! `ana-installer`: real environment materialization on top of
//! `rattler::install::Installer`, providing a shared cache location and
//! HTTP client ([`Downloader`]).
//!
//! [`reconcile`] is the one entry point callers (`ana::run_command`) use:
//! given the environment's already-held advisory lock
//! ([`ana_lockfile::EnvironmentLockGuard`], layered inside the existing
//! lock rather than a second one), the environment's paths, the target
//! platform, and the resolved `desired` record set from `ana.lock`, it
//! builds and runs a real `Installer`.
//!
//! This crate owns no short-circuit or crash-recovery bookkeeping of its
//! own: `ana-lockfile`'s env lock (`<env_path>/ana.lock`, a `dirty` bit
//! plus the last-reconciled section) tracks that instead. The caller
//! decides whether `reconcile` is even worth calling at all, and a
//! `dirty` env lock's recursive wipe of `env_path` (handled by
//! `ana-lockfile`, before this crate is ever invoked) means there is
//! nothing left in `conda-meta` for a fresh install to consider
//! already-installed.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod downloader;
mod error;
mod progress;

use std::collections::HashSet;
use std::path::PathBuf;

use ana_lockfile::EnvironmentLockGuard;
use ana_paths::EnvironmentPaths;
use rattler::install::{InstallationResultRecord, Transaction};
use rattler_conda_types::{
    MinimalPrefixCollection, PackageName, Platform, PrefixRecord, RepoDataRecord,
};

pub use downloader::Downloader;
pub use error::Error;

/// Whether extraneous, already-installed packages (present in the
/// environment, absent from `desired`) are removed.
///
/// This is an `ana`-only policy over `Installer::with_ignored_packages` --
/// rattler itself has no such concept; exact vs. inexact maps onto the
/// `ignored` parameter, not a different code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileMode {
    /// `ana install`/`ana sync`'s default: extraneous names are removed
    /// (rattler's own default behavior -- no `ignored` set at all).
    Exact,
    /// `ana run`'s default: extraneous names are left alone. Computed as
    /// `names(current) − names(desired)` and passed to
    /// `Installer::with_ignored_packages`.
    Inexact,
}

/// Reconcile `paths.env_path` against `desired`, the resolved target-set
/// records for `platform`. Always performs a real install; the caller
/// decides whether this is even worth calling, by comparing `desired`
/// against the env lock's previously-reconciled packages.
///
/// `_guard` is proof `paths`' advisory lock is already held by the
/// caller, for the whole span from before this call through the env
/// lock's post-install write -- this function acquires nothing itself.
///
/// 1. For [`ReconcileMode::Inexact`], read the environment's currently-
///    installed package names (a minimal, sparse `conda-meta` read) and
///    compute `ignored = names(current) − names(desired)`.
/// 2. Build and run the `Installer`.
///
/// Returns the resulting [`Transaction`] (boxed: it carries every
/// operation's full before/after records, a comparatively large value to
/// move around by copy).
pub async fn reconcile(
    _guard: &EnvironmentLockGuard<'_>,
    downloader: &Downloader,
    paths: &EnvironmentPaths,
    platform: Platform,
    desired: Vec<RepoDataRecord>,
    mode: ReconcileMode,
) -> Result<Box<Transaction<InstallationResultRecord, RepoDataRecord>>, Error> {
    // Only `Inexact` mode needs the environment's current package names
    // -- `Exact` mode passes no `ignored` set.
    let ignored: HashSet<PackageName> = match mode {
        ReconcileMode::Inexact => {
            let current =
                PrefixRecord::collect_minimal_from_prefix(&paths.env_path).map_err(|source| {
                    Error::ReadPrefix {
                        path: paths.env_path.clone(),
                        source,
                    }
                })?;
            let desired_names: HashSet<&str> = desired
                .iter()
                .map(|record| record.package_record.name.as_normalized())
                .collect();
            current
                .into_iter()
                .filter(|record| !desired_names.contains(record.name.as_normalized()))
                .map(|record| record.name)
                .collect()
        }
        ReconcileMode::Exact => HashSet::new(),
    };

    let mut installer = downloader.installer(platform);
    if !ignored.is_empty() {
        installer = installer.with_ignored_packages(ignored);
    }

    let env_path: PathBuf = paths.env_path.clone();
    let result = installer
        .install(&paths.env_path, desired)
        .await
        .map_err(|source| Error::Install {
            path: env_path,
            source: Box::new(source),
        })?;

    Ok(Box::new(result.transaction))
}
