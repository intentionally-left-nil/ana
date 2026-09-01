//! `ana-auth`: authenticates conda-channel HTTP requests (repodata
//! fetch and package download) using an API key a user already
//! obtained via `ana login` (anaconda-cli) or `anaconda login` (Python
//! `anaconda-auth`), by reading `~/.anaconda/keyring` -- the plain JSON
//! credential store both tools write by default. Never the OS keychain,
//! never a login/OAuth flow of its own: a missing or expired credential
//! just means the request goes out unauthenticated, the same as any
//! channel with no stored credential.
//!
//! [`build_middleware`] is the single entry point every caller needs:
//! it reads and parses the keyring ([`keyring::load`]), resolves a
//! request host to its keyring domain through the compiled-in legacy
//! alias table ([`resolve::resolve_domain`]) via [`backend::KeyringBackend`],
//! and returns a ready-to-use `reqwest` middleware plus a diagnostic
//! message for the rare case something about the file itself (not a
//! missing credential) went wrong.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod backend;
mod keyring;
mod resolve;

use std::sync::Arc;

use rattler_networking::{AuthenticationMiddleware, AuthenticationStorage};
use reqwest_middleware::{Middleware, Next};

pub use keyring::{default_keyring_path, keyring_path};
pub use resolve::resolve_domain;

/// What [`build_middleware`] produces: the middleware itself, ready to
/// layer into a `reqwest_middleware::ClientBuilder` chain, plus an
/// optional diagnostic describing a problem with the keyring file
/// itself (corrupt JSON, a permission error) -- never set for the
/// common case of a simply-missing file.
pub struct LoadedAuth {
    pub middleware: Arc<dyn Middleware>,
    pub diagnostic: Option<String>,
}

/// Reads the keyring at `keyring_path` once and builds the auth
/// middleware for `ana`'s shared HTTP client.
/// Never fails from the caller's point of view: a missing, unreadable,
/// or corrupt keyring degrades to an empty/no-op middleware (every
/// request goes out unauthenticated) rather than a hard error for the
/// whole `ana` invocation -- private-channel auth being broken must not
/// block work against public channels.
///
/// Credentials are only ever attached to `https` requests: rattler's
/// middleware matches on host alone, and `ana`'s channel validation
/// permits `http://` channel URLs, so without the gate an API key
/// would go out as a cleartext `Authorization` header.
pub fn build_middleware(keyring_path: Option<&std::path::Path>) -> LoadedAuth {
    let (keyring, diagnostic) = match keyring_path {
        Some(path) => keyring::load(path),
        // No resolvable home directory -- same "no credential found"
        // degradation as a missing file, not a diagnostic-worthy state
        // (an already-degraded environment for every other home-relative
        // path `ana` uses, not specific to this feature).
        None => (keyring::ParsedKeyring::default(), None),
    };

    let mut storage = AuthenticationStorage::empty();
    storage.add_backend(Arc::new(backend::KeyringBackend::new(keyring)));

    LoadedAuth {
        middleware: Arc::new(HttpsOnly(AuthenticationMiddleware::from_auth_storage(
            storage,
        ))),
        diagnostic,
    }
}

/// Delegates to the inner auth middleware for `https` requests only;
/// anything else passes through untouched, so a keyring API key is
/// never sent over a plaintext connection.
struct HttpsOnly<M>(M);

#[async_trait::async_trait]
impl<M: Middleware> Middleware for HttpsOnly<M> {
    async fn handle(
        &self,
        req: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        if req.url().scheme() == "https" {
            self.0.handle(req, extensions, next).await
        } else {
            next.run(req, extensions).await
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc as StdArc;

    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::Engine;
    use http::Extensions;
    use reqwest_middleware::Next;

    use super::*;

    fn write_keyring(dir: &tempfile::TempDir, domain: &str, api_key: &str) -> std::path::PathBuf {
        let credential = serde_json::json!({"domain": domain, "api_key": api_key});
        let blob = BASE64_STANDARD.encode(serde_json::to_vec(&credential).unwrap());
        let mut entries = serde_json::Map::new();
        entries.insert(domain.to_string(), serde_json::Value::String(blob));
        let mut sections = serde_json::Map::new();
        sections.insert(
            "Anaconda Cloud".to_string(),
            serde_json::Value::Object(entries),
        );
        let path = dir.path().join("keyring");
        std::fs::write(&path, serde_json::to_vec(&sections).unwrap()).unwrap();
        path
    }

    /// A terminal middleware that never touches the network: it echoes
    /// whatever `Authorization` header (if any) the request has by the
    /// time it reaches here, as the response body. Layered *after* the
    /// middleware under test in the chain, so it observes exactly what
    /// that middleware set on the request.
    struct EchoAuthorizationHeader;

    #[async_trait::async_trait]
    impl Middleware for EchoAuthorizationHeader {
        async fn handle(
            &self,
            req: reqwest::Request,
            _extensions: &mut Extensions,
            _next: Next<'_>,
        ) -> reqwest_middleware::Result<reqwest::Response> {
            let header = req
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .map(|value| value.to_str().unwrap_or_default().to_string())
                .unwrap_or_default();
            let response = http::Response::builder()
                .status(200)
                .body(header.into_bytes())
                .unwrap();
            Ok(reqwest::Response::from(response))
        }
    }

    /// Runs `middleware` (ahead of [`EchoAuthorizationHeader`]) against a
    /// bare request to `url`, returning the `Authorization` header it
    /// set, if any -- no real network call ever happens.
    fn authorization_header_for(middleware: StdArc<dyn Middleware>, url: &str) -> Option<String> {
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new())
            .with_arc(middleware)
            .with_arc(StdArc::new(EchoAuthorizationHeader))
            .build();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body = runtime.block_on(async {
            let response = client.get(url).send().await.unwrap();
            response.text().await.unwrap()
        });
        if body.is_empty() {
            None
        } else {
            Some(body)
        }
    }

    #[test]
    fn a_request_to_an_aliased_host_is_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_keyring(&dir, "anaconda.com", "secret-key");
        let loaded = build_middleware(Some(&path));

        assert_eq!(loaded.diagnostic, None);
        let header = authorization_header_for(
            loaded.middleware,
            "https://repo.anaconda.cloud/pkgs/main/linux-64/repodata.json",
        );
        assert_eq!(header, Some("Bearer secret-key".to_string()));
    }

    #[test]
    fn a_request_to_an_unrelated_host_is_not_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_keyring(&dir, "anaconda.com", "secret-key");
        let loaded = build_middleware(Some(&path));

        let header = authorization_header_for(
            loaded.middleware,
            "https://conda.anaconda.org/conda-forge/linux-64/repodata.json",
        );
        assert_eq!(header, None);
    }

    /// The same aliased host over plain `http` must NOT be
    /// authenticated: rattler's middleware matches on host alone, so
    /// without the scheme gate the API key would go out as a cleartext
    /// `Authorization` header.
    #[test]
    fn a_plain_http_request_is_never_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_keyring(&dir, "anaconda.com", "secret-key");
        let loaded = build_middleware(Some(&path));

        let header = authorization_header_for(
            loaded.middleware,
            "http://repo.anaconda.cloud/pkgs/main/linux-64/repodata.json",
        );
        assert_eq!(header, None);
    }

    #[test]
    fn a_missing_keyring_file_degrades_to_an_unauthenticated_but_working_middleware() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = build_middleware(Some(&dir.path().join("does-not-exist")));

        assert_eq!(loaded.diagnostic, None);
        let header = authorization_header_for(
            loaded.middleware,
            "https://repo.anaconda.cloud/pkgs/main/linux-64/repodata.json",
        );
        assert_eq!(header, None);
    }
}
