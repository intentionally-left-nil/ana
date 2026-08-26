//! Thin HTTP abstraction behind [`HttpClient`], so the state machine in
//! `refresh` can be unit-tested against an in-memory fake without adding a
//! mock-HTTP-server dependency to the workspace just for this crate. The
//! real implementation ([`UreqHttpClient`]) is a thin wrapper over `ureq`.

use std::time::Duration;

use ureq::Agent;

/// Short timeouts everywhere: this crate's whole point is to never let a
/// slow or absent network noticeably stall `ana`'s hot path. A connect
/// timeout bounds how long a genuinely offline host takes to fail; the
/// overall timeout bounds a server that accepts the connection but never
/// responds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const OVERALL_TIMEOUT: Duration = Duration::from_secs(10);

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
    Transport(#[from] ureq::Error),
    #[error("unexpected HTTP status {0}")]
    UnexpectedStatus(u16),
}

/// Implemented by the real `ureq`-backed client and, in tests, by an
/// in-memory fake. `Send + Sync` because a background refresh runs this on
/// a spawned `std::thread`.
pub(crate) trait HttpClient: Send + Sync {
    fn head(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<HeadResponse, HttpError>;

    fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<GetResponse, HttpError>;
}

pub(crate) struct UreqHttpClient {
    agent: Agent,
}

impl UreqHttpClient {
    pub(crate) fn new() -> Self {
        let config = Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(OVERALL_TIMEOUT))
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build();
        Self {
            agent: config.into(),
        }
    }
}

fn header_value(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

impl HttpClient for UreqHttpClient {
    fn head(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<HeadResponse, HttpError> {
        let mut request = self.agent.head(url);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header("If-Modified-Since", last_modified);
        }

        let response = request.call()?;
        match response.status().as_u16() {
            304 => Ok(HeadResponse::NotModified),
            200 => Ok(HeadResponse::Changed),
            405 | 501 => Ok(HeadResponse::Unsupported),
            other => Err(HttpError::UnexpectedStatus(other)),
        }
    }

    fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<GetResponse, HttpError> {
        let mut request = self.agent.get(url);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        if let Some(last_modified) = last_modified {
            request = request.header("If-Modified-Since", last_modified);
        }

        let mut response = request.call()?;
        match response.status().as_u16() {
            304 => Ok(GetResponse::NotModified),
            200 => {
                let etag = header_value(&response, "etag");
                let last_modified = header_value(&response, "last-modified");
                let body = response.body_mut().read_to_vec()?;
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
