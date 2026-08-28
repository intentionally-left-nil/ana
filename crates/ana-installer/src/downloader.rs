//! [`Downloader`]: the one shared HTTP client, package cache, and wheel
//! cache root for the whole `ana` process -- built once in `main.rs` and
//! handed to both `ana-solver`'s `Gateway` (via [`Downloader::client`])
//! and every [`reconcile`](crate::reconcile) call, per
//! `investigations/package_download_and_install.md`'s "Suggested shape
//! for a new `ana-installer` crate" and recommendation 1 ("one client,
//! one retry policy, for both repodata and package-artifact fetches").

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rattler::install::Installer;
use rattler_cache::package_cache::PackageCache;
use rattler_cache::{PACKAGE_CACHE_DIR, WHEEL_CACHE_DIR};
use rattler_conda_types::Platform;
use rattler_networking::retry_policies::default_retry_policy;
use rattler_networking::LazyClient;
use reqwest_middleware::ClientBuilder;
use reqwest_retry::RetryTransientMiddleware;
use tokio::sync::Semaphore;

use crate::Error;

/// Rattler's own default `io_concurrency_semaphore` size
/// (`InstallDriver`'s built-in default) -- made explicit per
/// recommendation 3, not a different number, just no longer implicit.
const IO_CONCURRENCY: usize = 100;

/// The shared HTTP client, package/wheel caches, and filesystem-
/// concurrency limit for every environment this `ana` invocation
/// reconciles (the default environment plus any `--group`/`--extra`
/// selections). `root` is `rattler_cache::default_cache_dir()` -- the
/// same location `pixi`/`rattler-build`/`rattler-bin` already use --
/// computed and `ensure_cache_dir`-ed exactly once by the caller (see
/// `main.rs`), not re-derived per subsystem.
pub struct Downloader {
    client: LazyClient,
    package_cache: PackageCache,
    wheel_cache_dir: PathBuf,
    io_concurrency_semaphore: Arc<Semaphore>,
}

impl Downloader {
    /// Builds a `Downloader` rooted at `root` (already
    /// `ensure_cache_dir`-ed, or about to be -- this call also runs it,
    /// idempotently, so a caller never has to sequence the two by hand).
    ///
    /// The client is built eagerly, not via `LazyClient::new`'s deferred
    /// closure: `reqwest::Client::builder().build()` can fail (an
    /// unavailable TLS backend, mainly), and this crate -- like the rest
    /// of the workspace -- denies `clippy::unwrap_used`/`expect_used`, so
    /// there is no way to surface that failure from inside a
    /// `FnOnce() -> ClientWithMiddleware` closure that can't itself
    /// return a `Result`. Building eagerly and wrapping the already-built
    /// client via `LazyClient::from` (which forces its own `LazyLock`
    /// immediately) is behaviorally identical for every caller -- the
    /// client is available on first real use either way -- and lets this
    /// constructor return `Result<Self, Error>` instead.
    pub fn new(root: &Path) -> Result<Self, Error> {
        rattler_cache::ensure_cache_dir(root).map_err(|source| Error::Cache {
            path: root.to_path_buf(),
            source,
        })?;

        let inner = reqwest::Client::builder()
            .user_agent(concat!("ana/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(Error::BuildClient)?;
        let client: LazyClient = ClientBuilder::new(inner)
            .with(RetryTransientMiddleware::new_with_policy(
                default_retry_policy(),
            ))
            .build()
            .into();

        let package_cache = PackageCache::new(root.join(PACKAGE_CACHE_DIR));
        let wheel_cache_dir = root.join(WHEEL_CACHE_DIR);

        Ok(Self {
            client,
            package_cache,
            wheel_cache_dir,
            io_concurrency_semaphore: Arc::new(Semaphore::new(IO_CONCURRENCY)),
        })
    }

    /// The shared client -- handed to `ana-solver::RattlerSolver::new`
    /// too, so repodata fetches and package/wheel downloads go through
    /// the same retry policy (closes the "Gap: `ana-solver` currently has
    /// no retry middleware at all" finding).
    pub fn client(&self) -> &LazyClient {
        &self.client
    }

    /// A pre-configured [`Installer`] for one `reconcile` call against
    /// `platform`. Deliberately does **not** call
    /// `.with_max_concurrent_requests`/`.with_concurrent_requests_semaphore`
    /// (recommendation 2: those are a throttle in front of rattler's
    /// already-concurrent-by-default fetch, not a concurrency mechanism
    /// to add).
    pub(crate) fn installer(&self, platform: Platform) -> Installer {
        Installer::new()
            .with_download_client(self.client.clone())
            .with_package_cache(self.package_cache.clone())
            .with_wheel_cache_dir(self.wheel_cache_dir.clone())
            .with_io_concurrency_semaphore(self.io_concurrency_semaphore.clone())
            .with_target_platform(platform)
            .with_execute_link_scripts(true)
    }
}
