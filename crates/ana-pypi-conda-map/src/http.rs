//! Thin HTTP abstraction behind [`HttpClient`], so the state machine in
//! `refresh` can be unit-tested against an in-memory fake without adding a
//! mock-HTTP-server dependency to the workspace just for this crate. The
//! real implementation ([`ReqwestHttpClient`]) is a thin wrapper over a
//! [`rattler_networking::LazyClient`] -- the same client `ana-installer`'s
//! downloads and `ana-solver`'s repodata fetches share (one client, one
//! retry policy, process-wide).
//!
//! `HttpClient` needs `#[async_trait]` rather than native async-fn-in-trait:
//! the existing `&dyn HttpClient`/`Arc<dyn HttpClient>` usage
//! ([`crate::load`]'s background-refresh thread hands the trait object
//! across a `std::thread::spawn` boundary) requires object safety, which
//! native async-fn-in-trait does not provide.

use rattler_networking::LazyClient;

/// Bounds a single request (connect and response) so a slow or absent
/// network can't stall `ana`'s hot path. Applied per-request via
/// `RequestBuilder::timeout` rather than the client's own
/// `connect_timeout`/`timeout` builder options: the underlying `reqwest`
/// client is shared process-wide (`rattler_networking::LazyClient`, built
/// once in `ana-installer::Downloader`), so this crate can't rebuild it
/// with its own shorter timeouts without imposing that on every other
/// consumer. A per-request timeout still bounds a hung connect attempt
/// (included in, and no longer than, the overall request time), just not
/// as a separately-named phase.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Hard cap on a single GET response body, enforced regardless of what
/// (if anything) the server's own `Content-Length` claims -- the mapping
/// endpoint (`pypi_to_conda_uri`) is network-supplied and could be
/// compromised, MITM'd, or simply misconfigured to point somewhere that
/// returns an oversized or unbounded body; without a cap, that body
/// would be buffered in full (see [`GetResponse::Ok`]'s `body`) before
/// any of `crate::fetch`'s per-entry validation ever runs.
const MAX_RESPONSE_BODY_BYTES: u64 = 50 * 1024 * 1024;

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
    #[error("response body exceeds the {MAX_RESPONSE_BODY_BYTES}-byte limit")]
    ResponseTooLarge,
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

        let mut response = request.send().await?;
        match response.status().as_u16() {
            304 => Ok(GetResponse::NotModified),
            200 => {
                let etag = header_value(&response, "etag");
                let last_modified = header_value(&response, "last-modified");
                // A `Content-Length` the server itself claims exceeds the
                // cap is rejected before reading a single byte of body;
                // an absent or understated one is still caught below, as
                // chunks actually arrive, since nothing here trusts the
                // header alone.
                if response
                    .content_length()
                    .is_some_and(|len| len > MAX_RESPONSE_BODY_BYTES)
                {
                    return Err(HttpError::ResponseTooLarge);
                }
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|err| HttpError::Transport(err.into()))?
                {
                    if exceeds_response_cap(body.len() as u64, chunk.len() as u64) {
                        return Err(HttpError::ResponseTooLarge);
                    }
                    body.extend_from_slice(&chunk);
                }
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

/// Whether accumulating `additional` more bytes on top of `so_far` would
/// exceed [`MAX_RESPONSE_BODY_BYTES`] -- the arithmetic
/// [`ReqwestHttpClient::get`]'s streaming loop uses to enforce the cap
/// chunk by chunk (the fallback that catches a body with no, or an
/// understated, `Content-Length`), factored out so it's unit-testable
/// without transferring an actually-oversized body. `saturating_add`
/// guards against overflow on a 32-bit `usize` target, where a chunk
/// length alone could in principle overflow a `u64` sum on repeated
/// (implausibly large) additions -- not a realistic scenario for a
/// single response, but cheap to make unconditionally correct.
fn exceeds_response_cap(so_far: u64, additional: u64) -> bool {
    so_far.saturating_add(additional) > MAX_RESPONSE_BODY_BYTES
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::{Read, Write};
    use std::net::TcpListener;

    use rattler_networking::LazyClient;

    use super::*;

    #[test]
    fn exceeds_response_cap_is_false_strictly_under_the_limit() {
        assert!(!exceeds_response_cap(0, MAX_RESPONSE_BODY_BYTES - 1));
    }

    #[test]
    fn exceeds_response_cap_is_false_exactly_at_the_limit() {
        assert!(!exceeds_response_cap(MAX_RESPONSE_BODY_BYTES - 1, 1));
    }

    #[test]
    fn exceeds_response_cap_is_true_one_byte_over_the_limit() {
        assert!(exceeds_response_cap(MAX_RESPONSE_BODY_BYTES, 1));
    }

    #[test]
    fn exceeds_response_cap_does_not_overflow_on_huge_inputs() {
        assert!(exceeds_response_cap(u64::MAX, u64::MAX));
    }

    /// A minimal, single-request-single-response raw TCP server: accepts
    /// one connection, discards whatever request it sends (just reads
    /// until the client stops writing, which happens once `reqwest` has
    /// sent its request and is waiting on the response), writes `raw`
    /// verbatim, then closes. Returns the port it bound to.
    fn spawn_raw_response_server(raw: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Drain the request headers (up to the blank line) without
            // needing to parse them -- this server only ever serves one
            // canned response regardless of what was asked for.
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            loop {
                let n = stream.read(&mut buf).unwrap();
                seen.extend_from_slice(&buf[..n]);
                if n == 0 || seen.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream.write_all(raw).unwrap();
            stream.flush().unwrap();
        });
        port
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// A server that (truthfully or not) declares a `Content-Length`
    /// larger than [`MAX_RESPONSE_BODY_BYTES`] must be rejected before
    /// `ReqwestHttpClient::get` ever reads a body byte -- proven here
    /// against the real `reqwest`/`LazyClient` stack, not a fake, with a
    /// server that sends no actual body at all (so the test is fast
    /// regardless of whether the pre-check works): if the pre-check were
    /// missing, this would hang waiting on bytes that never arrive
    /// rather than erroring out promptly.
    #[test]
    fn get_rejects_a_response_whose_content_length_header_exceeds_the_cap() {
        let over_the_cap = MAX_RESPONSE_BODY_BYTES + 1;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {over_the_cap}\r\nConnection: close\r\n\r\n"
        );
        let port = spawn_raw_response_server(Box::leak(response.into_bytes().into_boxed_slice()));

        let client = ReqwestHttpClient::new(LazyClient::default());
        let result = runtime().block_on(client.get(
            &format!("http://127.0.0.1:{port}/mapping.json"),
            None,
            None,
        ));

        assert!(
            matches!(result, Err(HttpError::ResponseTooLarge)),
            "expected ResponseTooLarge, got {}",
            match &result {
                Ok(_) => "Ok(_)".to_string(),
                Err(err) => err.to_string(),
            }
        );
    }

    /// A normal, well-under-the-cap response is unaffected by either
    /// check -- the pre-check and the streaming loop both let it
    /// through.
    #[test]
    fn get_accepts_a_response_under_the_cap() {
        let body = b"{\"opencv-python\":\"py-opencv\"}";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut raw = response.into_bytes();
        raw.extend_from_slice(body);
        let port = spawn_raw_response_server(Box::leak(raw.into_boxed_slice()));

        let client = ReqwestHttpClient::new(LazyClient::default());
        let result = runtime()
            .block_on(client.get(&format!("http://127.0.0.1:{port}/mapping.json"), None, None))
            .unwrap();

        let GetResponse::Ok { body: got, .. } = result else {
            panic!("expected GetResponse::Ok");
        };
        assert_eq!(got, body);
    }
}
