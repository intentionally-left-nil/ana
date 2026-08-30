//! Integration test for [`ana_installer::reconcile`] against a real,
//! `file://`-backed conda package -- no live channel: everything here is
//! a genuinely tiny archive on disk that `Installer::install` extracts,
//! hash-verifies, and links for real.
//!
//! `reconcile` itself doesn't short-circuit or track interruption --
//! that bookkeeping lives in `ana-lockfile`'s env lock, one layer up.
//! These tests cover only what `reconcile` still owns: a real install,
//! and the `Exact`/`Inexact` extraneous-package policy.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::str::FromStr;

use ana_installer::{reconcile, Downloader, ReconcileMode};
use ana_paths::{discover, EnvironmentLayout};
use rattler_conda_types::package::DistArchiveIdentifier;
use rattler_conda_types::{
    NoArchType, PackageName, PackageRecord, Platform, RepoDataRecord, Version,
};

/// The fixture package copied from `intentionally-left-nil/rattler`'s own
/// test data -- see `tests/fixtures/README.md` for provenance/license.
const FIXTURE_FILE_NAME: &str = "empty-0.1.0-h4616a5c_0.conda";
const FIXTURE_SHA256: &str = "af8000ad3ad6af83b294b0e700f7c6f17fa85c6b9db08207813f47af8a94d52c";
const FIXTURE_SIZE: u64 = 1538;

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/packages")
        .join(FIXTURE_FILE_NAME)
}

/// Decode a hex-encoded sha256 digest into the fixed-size array
/// `PackageRecord::sha256`'s `GenericArray` converts from -- avoids
/// pulling in `rattler_digest` just for a test fixture.
fn hex_bytes(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).unwrap();
    }
    out
}

/// Build a real [`RepoDataRecord`] pointing at the on-disk fixture
/// archive via a `file://` URL -- `Installer::install` fetches/verifies/
/// links from `record.url` the same way it would any `https://` archive,
/// so this exercises the real pipeline, not a stand-in.
fn fixture_record() -> RepoDataRecord {
    let mut package_record = PackageRecord::new(
        PackageName::new_unchecked("empty"),
        Version::from_str("0.1.0").unwrap(),
        "h4616a5c_0".to_string(),
    );
    package_record.subdir = "noarch".to_string();
    package_record.noarch = NoArchType::generic();
    package_record.sha256 = Some(hex_bytes(FIXTURE_SHA256).into());
    package_record.size = Some(FIXTURE_SIZE);

    let identifier = DistArchiveIdentifier::try_from_filename(FIXTURE_FILE_NAME).unwrap();
    let url = url::Url::from_file_path(fixture_path()).unwrap();
    RepoDataRecord {
        package_record,
        identifier,
        url,
        channel: None,
    }
}

/// A fresh `Downloader` rooted at its own temp cache dir, so tests never
/// share cache state (or its global lock) with each other or with a real
/// `~/.cache/rattler`.
fn downloader(cache_root: &Path) -> Downloader {
    Downloader::new(cache_root).unwrap()
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn first_install_writes_conda_meta() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let paths = discover(EnvironmentLayout::ProjectDefault {
        root: project.path(),
    });
    let downloader = downloader(cache.path());

    let mut lock = ana_lockfile::acquire_environment_lock(&paths).unwrap();
    let guard = lock.acquire().unwrap();

    let transaction = run(reconcile(
        &guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![fixture_record()],
        ReconcileMode::Exact,
    ))
    .unwrap();

    assert!(!transaction.operations.is_empty());
    assert!(
        paths
            .env_path
            .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
            .exists(),
        "the package's conda-meta record must exist after a real install"
    );
}

#[test]
fn inexact_mode_leaves_an_extraneous_package_installed() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let paths = discover(EnvironmentLayout::ProjectDefault {
        root: project.path(),
    });
    let downloader = downloader(cache.path());

    let mut lock = ana_lockfile::acquire_environment_lock(&paths).unwrap();
    let guard = lock.acquire().unwrap();

    run(reconcile(
        &guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![fixture_record()],
        ReconcileMode::Exact,
    ))
    .unwrap();

    run(reconcile(
        &guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![],
        ReconcileMode::Inexact,
    ))
    .unwrap();

    assert!(
        paths
            .env_path
            .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
            .exists(),
        "inexact mode must not remove a package absent from `desired`"
    );
}

#[test]
fn exact_mode_removes_an_extraneous_package() {
    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let paths = discover(EnvironmentLayout::ProjectDefault {
        root: project.path(),
    });
    let downloader = downloader(cache.path());

    let mut lock = ana_lockfile::acquire_environment_lock(&paths).unwrap();
    let guard = lock.acquire().unwrap();

    run(reconcile(
        &guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![fixture_record()],
        ReconcileMode::Exact,
    ))
    .unwrap();

    run(reconcile(
        &guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![],
        ReconcileMode::Exact,
    ))
    .unwrap();

    assert!(
        !paths
            .env_path
            .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
            .exists(),
        "exact mode must remove a package absent from `desired`"
    );
}
