//! Integration test proving [`ana_installer::Downloader`]'s shared HTTP
//! client authenticates requests to an aliased Anaconda-hosted channel
//! host end to end: a real `~/.anaconda/keyring`-shaped fixture (via
//! `ANA_KEYRING_PATH`) plus a real `Installer::install` package fetch,
//! not just `ana-auth`'s own unit tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use ana_installer::{reconcile, Downloader, ReconcileMode};
use ana_paths::{discover, EnvironmentLayout};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use rattler_conda_types::package::DistArchiveIdentifier;
use rattler_conda_types::{
    NoArchType, PackageName, PackageRecord, Platform, RepoDataRecord, Version,
};
use reqwest_middleware::{Middleware, Next};

/// `ANA_KEYRING_PATH` is process-wide state -- serialize this file's
/// tests so they can't observe each other's mutations (matches
/// `ana-config`'s own `path.rs` convention for `ANA_CONFIG_PATH`).
static ENV_LOCK: Mutex<()> = Mutex::new(());

const FIXTURE_FILE_NAME: &str = "empty-0.1.0-h4616a5c_0.conda";
const FIXTURE_SHA256: &str = "af8000ad3ad6af83b294b0e700f7c6f17fa85c6b9db08207813f47af8a94d52c";
const FIXTURE_SIZE: u64 = 1538;
const FIXTURE_URL: &str =
    "https://repo.anaconda.cloud/pkgs/main/noarch/empty-0.1.0-h4616a5c_0.conda";

fn fixture_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/packages")
        .join(FIXTURE_FILE_NAME)
}

fn hex_bytes(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = &hex[i * 2..i * 2 + 2];
        *byte = u8::from_str_radix(pair, 16).unwrap();
    }
    out
}

/// A [`RepoDataRecord`] whose URL is `FIXTURE_URL` -- an aliased,
/// Anaconda-hosted-looking host (`repo.anaconda.cloud`), so a real
/// install against it exercises [`ana_auth`]'s legacy alias resolution,
/// not just a bare custom-channel domain.
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
    let url = url::Url::parse(FIXTURE_URL).unwrap();
    RepoDataRecord {
        package_record,
        identifier,
        url,
        channel: None,
    }
}

/// Serves `FIXTURE_URL`'s response from the local fixture archive (so
/// this test never hits the network) and records the `Authorization`
/// header the request carried when it arrived here -- i.e. after
/// `ana_auth::build_middleware`'s middleware, which `Downloader::build`
/// places ahead of this one in the chain, has already run.
struct RecordingMiddleware {
    seen_authorization: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Middleware for RecordingMiddleware {
    async fn handle(
        &self,
        req: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        if req.url().as_str() == FIXTURE_URL {
            let header = req
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .map(|value| value.to_str().unwrap_or_default().to_string());
            *self.seen_authorization.lock().unwrap() = header;
            let body = fs::read(fixture_path()).unwrap();
            let response = http::Response::builder().status(200).body(body).unwrap();
            Ok(reqwest::Response::from(response))
        } else {
            next.run(req, extensions).await
        }
    }
}

/// Writes a `~/.anaconda/keyring`-shaped fixture at `path`, storing
/// `api_key` under the `anaconda.com` domain -- the domain
/// `repo.anaconda.cloud` (this test's `FIXTURE_URL` host) resolves to
/// via `ana_auth`'s compiled-in legacy alias table.
fn write_keyring_fixture(path: &Path, api_key: &str) {
    let credential = serde_json::json!({"domain": "anaconda.com", "api_key": api_key});
    let blob = BASE64_STANDARD.encode(serde_json::to_vec(&credential).unwrap());
    let mut entries = serde_json::Map::new();
    entries.insert("anaconda.com".to_string(), serde_json::Value::String(blob));
    let mut sections = serde_json::Map::new();
    sections.insert(
        "Anaconda Cloud".to_string(),
        serde_json::Value::Object(entries),
    );
    fs::write(path, serde_json::to_vec(&sections).unwrap()).unwrap();
}

fn run<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// The whole chain, end to end: a real `~/.anaconda/keyring` fixture (via
/// `ANA_KEYRING_PATH`) for an aliased host, wired through
/// `Downloader::build` into a real `Installer::install` package fetch --
/// the fetched request must carry `Authorization: Bearer <api_key>`.
#[test]
fn a_real_install_against_an_aliased_host_is_authenticated() {
    let _guard = ENV_LOCK.lock().unwrap();

    let keyring_dir = tempfile::tempdir().unwrap();
    let keyring_path = keyring_dir.path().join("keyring");
    write_keyring_fixture(&keyring_path, "secret-key");
    std::env::set_var("ANA_KEYRING_PATH", &keyring_path);

    let seen_authorization = Arc::new(Mutex::new(None));
    let middleware = Arc::new(RecordingMiddleware {
        seen_authorization: seen_authorization.clone(),
    });

    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let downloader = Downloader::for_testing(cache.path(), Some(middleware)).unwrap();

    std::env::remove_var("ANA_KEYRING_PATH");

    let paths = discover(EnvironmentLayout::ProjectDefault {
        root: project.path(),
    });
    let mut lock = ana_lockfile::acquire_environment_lock(&paths).unwrap();
    let lock_guard = lock.acquire().unwrap();

    run(reconcile(
        &lock_guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![fixture_record()],
        ReconcileMode::Exact,
    ))
    .unwrap();

    assert_eq!(
        *seen_authorization.lock().unwrap(),
        Some("Bearer secret-key".to_string()),
        "the request to the aliased host must carry the keyring's api_key as a bearer token"
    );
}

/// The same real install, but with no matching keyring entry at all --
/// the request must go out unauthenticated (no `Authorization` header),
/// and the install must still succeed. Proves the graceful-degradation
/// path reaches a real `Downloader`/`Installer`, not just `ana-auth`'s
/// own unit tests.
#[test]
fn a_real_install_with_no_keyring_entry_is_unauthenticated_but_succeeds() {
    let _guard = ENV_LOCK.lock().unwrap();

    let keyring_dir = tempfile::tempdir().unwrap();
    // A missing file, not a fixture with no matching domain -- exercises
    // the same silent-degradation path a user who never ran `ana
    // login`/`anaconda login` hits.
    std::env::set_var("ANA_KEYRING_PATH", keyring_dir.path().join("keyring"));

    let seen_authorization = Arc::new(Mutex::new(None));
    let middleware = Arc::new(RecordingMiddleware {
        seen_authorization: seen_authorization.clone(),
    });

    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let downloader = Downloader::for_testing(cache.path(), Some(middleware)).unwrap();

    std::env::remove_var("ANA_KEYRING_PATH");

    let paths = discover(EnvironmentLayout::ProjectDefault {
        root: project.path(),
    });
    let mut lock = ana_lockfile::acquire_environment_lock(&paths).unwrap();
    let lock_guard = lock.acquire().unwrap();

    run(reconcile(
        &lock_guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![fixture_record()],
        ReconcileMode::Exact,
    ))
    .unwrap();

    assert_eq!(*seen_authorization.lock().unwrap(), None);
    assert!(paths
        .env_path
        .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
        .exists());
}

/// Same again, but with a keyring file that exists yet is corrupt (not
/// valid JSON at all) rather than simply missing -- the real install
/// must still succeed unauthenticated, proving the graceful-degradation
/// path for a *broken* file (not just an absent one) reaches a real
/// `Downloader`/`Installer`.
#[test]
fn a_real_install_with_a_corrupt_keyring_is_unauthenticated_but_succeeds() {
    let _guard = ENV_LOCK.lock().unwrap();

    let keyring_dir = tempfile::tempdir().unwrap();
    let keyring_path = keyring_dir.path().join("keyring");
    fs::write(&keyring_path, b"not valid json").unwrap();
    std::env::set_var("ANA_KEYRING_PATH", &keyring_path);

    let seen_authorization = Arc::new(Mutex::new(None));
    let middleware = Arc::new(RecordingMiddleware {
        seen_authorization: seen_authorization.clone(),
    });

    let project = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let downloader = Downloader::for_testing(cache.path(), Some(middleware)).unwrap();

    std::env::remove_var("ANA_KEYRING_PATH");

    let paths = discover(EnvironmentLayout::ProjectDefault {
        root: project.path(),
    });
    let mut lock = ana_lockfile::acquire_environment_lock(&paths).unwrap();
    let lock_guard = lock.acquire().unwrap();

    run(reconcile(
        &lock_guard,
        &downloader,
        &paths,
        Platform::current(),
        vec![fixture_record()],
        ReconcileMode::Exact,
    ))
    .unwrap();

    assert_eq!(*seen_authorization.lock().unwrap(), None);
    assert!(paths
        .env_path
        .join("conda-meta/empty-0.1.0-h4616a5c_0.json")
        .exists());
}
