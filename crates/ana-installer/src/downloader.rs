//! [`Downloader`]: the one shared HTTP client, package cache, and wheel
//! cache root for the whole `ana` process -- built once in `main.rs` and
//! handed to both `ana-solver`'s `Gateway` (via [`Downloader::client`])
//! and every [`reconcile`](crate::reconcile) call. Every request this
//! client makes is transparently authenticated against
//! `~/.anaconda/keyring` via [`ana_auth::build_middleware`] -- see
//! [`Downloader::build`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rattler::install::Installer;
use rattler_cache::package_cache::PackageCache;
use rattler_cache::{PACKAGE_CACHE_DIR, WHEEL_CACHE_DIR};
use rattler_conda_types::Platform;
use rattler_networking::retry_policies::default_retry_policy;
use rattler_networking::LazyClient;
use reqwest_middleware::{ClientBuilder, Middleware};
use reqwest_retry::RetryTransientMiddleware;
use tokio::sync::Semaphore;

use crate::Error;

/// Rattler's own default `io_concurrency_semaphore` size
/// (`InstallDriver`'s built-in default), made explicit rather than
/// implicit.
const IO_CONCURRENCY: usize = 100;

/// The shared HTTP client, package/wheel caches, and filesystem-
/// concurrency limit for every environment this `ana` invocation
/// reconciles. `root` is `rattler_cache::default_cache_dir()` -- the
/// same location `pixi`/`rattler-build`/`rattler-bin` already use --
/// computed and `ensure_cache_dir`-ed exactly once by the caller (see
/// `main.rs`).
pub struct Downloader {
    client: LazyClient,
    package_cache: PackageCache,
    wheel_cache_dir: PathBuf,
    io_concurrency_semaphore: Arc<Semaphore>,
}

impl Downloader {
    /// Builds a `Downloader` rooted at `root`, running
    /// `rattler_cache::ensure_cache_dir` on it (idempotently).
    ///
    /// The client is built eagerly rather than via `LazyClient::new`'s
    /// deferred closure: `reqwest::Client::builder().build()` can fail
    /// (an unavailable TLS backend, mainly), and a `FnOnce() ->
    /// ClientWithMiddleware` closure can't itself return a `Result` --
    /// building eagerly lets this constructor return `Result<Self, Error>`
    /// instead, without violating this crate's `unwrap`/`expect` ban.
    pub fn new(root: &Path) -> Result<Self, Error> {
        rattler_cache::ensure_cache_dir(root).map_err(|source| Error::Cache {
            path: root.to_path_buf(),
            source,
        })?;
        Self::build(root, None)
    }

    /// Like [`Downloader::new`], but layers `middleware` (if any) on top
    /// of the same retry policy, ahead of it in the chain -- so it sees
    /// (and can short-circuit) every request before any retry logic would
    /// apply. For tests that need `reconcile`'s real `Installer`/client
    /// wiring exercised end to end without any real network I/O:
    /// `middleware` can intercept a request for a known fixture URL and
    /// answer it from an in-memory [`reqwest::Response`].
    ///
    /// The cache root gets no `CACHEDIR.TAG`/Time Machine exclusion: a
    /// test's cache root is a throwaway tempdir, and the exclusion's
    /// CoreServices call (`CSBackupSetItemExcluded`) crashes
    /// intermittently when entered from many test threads at once.
    pub fn for_testing(
        root: &Path,
        middleware: Option<Arc<dyn Middleware>>,
    ) -> Result<Self, Error> {
        std::fs::create_dir_all(root).map_err(|source| Error::Cache {
            path: root.to_path_buf(),
            source,
        })?;
        Self::build(root, middleware)
    }

    fn build(root: &Path, extra_middleware: Option<Arc<dyn Middleware>>) -> Result<Self, Error> {
        let inner = reqwest::Client::builder()
            .user_agent(concat!("ana/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(Error::BuildClient)?;
        // `ana_auth::build_middleware`'s result is added ahead of both
        // `extra_middleware` and the retry policy: a request that's
        // about to be authenticated shouldn't be retried without its
        // `Authorization` header first, and a test's `extra_middleware`
        // fixture should see the request already authenticated, the
        // same as a real network call would.
        let mut builder =
            ClientBuilder::new(inner).with_arc(ana_auth::build_middleware().middleware);
        if let Some(middleware) = extra_middleware {
            builder = builder.with_arc(middleware);
        }
        let client: LazyClient = builder
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
    /// the same retry policy.
    pub fn client(&self) -> &LazyClient {
        &self.client
    }

    /// A pre-configured [`Installer`] for one `reconcile` call against
    /// `platform`. Deliberately does **not** call
    /// `.with_max_concurrent_requests`/`.with_concurrent_requests_semaphore`:
    /// those are a throttle in front of rattler's already-concurrent-by-
    /// default fetch, not a concurrency mechanism to add. Link scripts
    /// (`post-link`/`pre-unlink`) stay off -- rattler's own default --
    /// since they run arbitrary shell code as part of installation,
    /// before any per-channel sandboxing decision is ever made; enabling
    /// them is a dedicated follow-up, not bundled with this call.
    pub(crate) fn installer(&self, platform: Platform) -> Installer {
        Installer::new()
            .with_download_client(self.client.clone())
            .with_package_cache(self.package_cache.clone())
            .with_wheel_cache_dir(self.wheel_cache_dir.clone())
            .with_io_concurrency_semaphore(self.io_concurrency_semaphore.clone())
            .with_target_platform(platform)
            .with_execute_link_scripts(false)
            .with_reporter(crate::progress::InstallProgress::new())
    }
}
