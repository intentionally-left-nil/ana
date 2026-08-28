//! Thin HTTP abstraction behind [`HttpClient`], so the state machine in
//! `refresh` can be unit-tested against an in-memory fake without adding a
//! mock-HTTP-server dependency to the workspace just for this crate. The
//! real implementation ([`ReqwestHttpClient`]) is a thin wrapper over a
//! [`rattler_networking::LazyClient`] -- the same one `ana-installer`'s
//! downloads and `ana-solver`'s repodata fetches share, per
//! `investigations/package_download_and_install_implementation_plan.md`'s
//! "one client, one retry policy, process-wide."
//!
//! `HttpClient` needs `#[async_trait]` rather than native async-fn-in-trait:
//! the existing `&dyn HttpClient`/`Arc<dyn HttpClient>` usage
//! ([`crate::load`]'s background-refresh thread hands the trait object
//! across a `std::thread::spawn` boundary) requires object safety, which
//! native async-fn-in-trait does not provide.

use rattler_networking::LazyClient;

/// Bounds a single request (connect *and* response) at a level this
/// crate's whole point is to never let a slow or absent network noticeably
/// stall `ana`'s hot path with. Applied per-request via `RequestBuilder::timeout`
/// rather than the client's own `connect_timeout`/`timeout` builder options
/// (the old `ureq`-backed client's approach): the underlying `reqwest`
/// client is now the one shared process-wide
/// (`rattler_networking::LazyClient`, built once in `ana-installer::Downloader`),
/// so this crate can't rebuild it with its own, shorter timeouts without
/// giving every other consumer of that client the same short bound. A
/// single per-request timeout still bounds a hung connect attempt (it's
/// included in, and therefore no longer than, the overall request time),
/// just not as a separately-named phase.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

const USER_AGENT_HEADER: &str = "User-Agent";
const USER_AGENT: &str = concat!("ana/", env!("CARGO_PKG_VERSION"));

pub(crate) enum HeadResponse {
    NotModified,
    /// Server has newer content than the validators we sent. No body --
    /// the caller still needs a follow-up GET.
    Changed,
    /// Server doesn't support HEAD for this resource (405/501). Caller
    /// should fall back to a conditional GET as the check instead.
    Unsupported,
}

pub(crate) enum GetResponse {
    NotModified,
    Ok {
        body: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("request failed: {0}")]
    Transport(#[from] reqwest_middleware::Error),
    #[error("unexpected HTTP status {0}")]
    UnexpectedStatus(u16),
}

/// Implemented by the real `LazyClient`-backed client and, in tests, by an
/// in-memory fake. `Send + Sync` because a background refresh runs this on
/// a spawned `std::thread` (via `tokio::runtime::Handle::block_on`, see
/// `crate::load`).
#[async_trait::async_trait]
pub(crate) trait HttpClient: Send + Sync {
    async fn head(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<HeadResponse, HttpError>;

    async fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<GetResponse, HttpError>;
}

/// Wraps the process-wide [`LazyClient`] (built once in
/// `ana-installer::Downloader` and handed to every consumer that talks
/// HTTP, this crate included) rather than owning its own `reqwest::Client`.
pub(crate) struct ReqwestHttpClient {
    client: LazyClient,
}

impl ReqwestHttpClient {
    pub(crate) fn new(client: LazyClient) -> Self {
        Self { client }
    }
}

fn header_value(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[async_trait::async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn head(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<HeadResponse, HttpError> {
        let mut request = self
            .client
            .client()
            .head(url)
            .header(USER_AGENT_HEADER, USER_AGENT)
            .timeout(REQUEST_TIMEOUT);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header("If-Modified-Since", last_modified);
        }

        let response = request.send().await?;
        match response.status().as_u16() {
            304 => Ok(HeadResponse::NotModified),
            200 => Ok(HeadResponse::Changed),
            405 | 501 => Ok(HeadResponse::Unsupported),
            other => Err(HttpError::UnexpectedStatus(other)),
        }
    }

    async fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<GetResponse, HttpError> {
        let mut request = self
            .client
            .client()
            .get(url)
            .header(USER_AGENT_HEADER, USER_AGENT)
            .timeout(REQUEST_TIMEOUT);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header("If-Modified-Since", last_modified);
        }

        let response = request.send().await?;
        match response.status().as_u16() {
            304 => Ok(GetResponse::NotModified),
            200 => {
                let etag = header_value(&response, "etag");
                let last_modified = header_value(&response, "last-modified");
                let body = response
                    .bytes()
                    .await
                    .map_err(|err| HttpError::Transport(err.into()))?
                    .to_vec();
                Ok(GetResponse::Ok {
                    body,
                    etag,
                    last_modified,
                })
            }
            other => Err(HttpError::UnexpectedStatus(other)),
        }
    }
}
