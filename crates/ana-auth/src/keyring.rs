//! Reads and parses `~/.anaconda/keyring` -- the plain-JSON credential
//! store `anaconda-auth`'s `AnacondaKeyring` backend and the Rust
//! `anaconda-cli`/`ana` binary both write by default. Never writes it
//! back: `ana` only ever reads a credential a user already obtained via
//! `ana login`/`anaconda login`.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use serde::Deserialize;

/// The fixed outer JSON key every real writer uses.
const KEYRING_SECTION: &str = "Anaconda Cloud";

/// `ANA_KEYRING_PATH` overrides the default `~/.anaconda/keyring`
/// location -- mirrors `ana-config`'s `ANA_CONFIG_PATH` pattern.
pub fn keyring_path() -> Option<PathBuf> {
    resolve(std::env::var_os("ANA_KEYRING_PATH").map(PathBuf::from))
}

/// `~/.anaconda/keyring`'s default, platform-appropriate location -- no
/// `ANA_KEYRING_PATH` override applied.
pub fn default_keyring_path() -> Option<PathBuf> {
    ana_paths::home_dir().map(|home| home.join(".anaconda").join("keyring"))
}

fn resolve(override_path: Option<PathBuf>) -> Option<PathBuf> {
    override_path.or_else(default_keyring_path)
}

/// One domain's stored credential, decoded from its base64 blob. Only
/// `api_key` is consumed by [`crate::build_middleware`]. Everything
/// else real writers include (`domain`, `username`, `repo_tokens`, an
/// integer `version`, ...) is left out of the struct on purpose:
/// serde ignores unknown fields, so their shape can never reject an
/// otherwise-valid blob.
#[derive(Clone, Deserialize, Default)]
struct Credential {
    #[serde(default)]
    api_key: Option<String>,
}

/// `domain -> api_key`, parsed from the keyring file. Empty if the file
/// is missing, unreadable, corrupt, or contains no domain with a stored
/// `api_key`.
#[derive(Clone, Default)]
pub struct ParsedKeyring {
    api_keys: HashMap<String, String>,
}

/// Redacted: the map values are API keys, so only the domains print.
impl std::fmt::Debug for ParsedKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedKeyring")
            .field("domains", &self.api_keys.keys())
            .finish()
    }
}

impl ParsedKeyring {
    pub fn api_key(&self, domain: &str) -> Option<&str> {
        self.api_keys.get(domain).map(String::as_str)
    }
}

/// Reads and parses the keyring at `path`. Returns `(parsed,
/// diagnostic)` -- `diagnostic` is `Some` only when something
/// unexpected happened (the file exists but couldn't be read, or its
/// contents are corrupt) worth surfacing to the user. A simply-missing
/// file -- the common case for anyone who hasn't run `ana
/// login`/`anaconda login` -- resolves silently to an empty
/// [`ParsedKeyring`], same as any other channel with no stored
/// credential.
pub fn load(path: &Path) -> (ParsedKeyring, Option<String>) {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return (ParsedKeyring::default(), None);
        }
        Err(err) => {
            return (
                ParsedKeyring::default(),
                Some(format!("could not read {}: {err}", path.display())),
            );
        }
    };

    let root: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(root) => root,
        Err(err) => {
            return (
                ParsedKeyring::default(),
                Some(format!("could not parse {}: {err}", path.display())),
            );
        }
    };

    // Only the fixed `"Anaconda Cloud"` key is ours -- a real keyring
    // file may hold other services' entries at the top level, in
    // whatever shape those services chose, and this must never fail to
    // parse because of them.
    let Some(entries) = root
        .get(KEYRING_SECTION)
        .and_then(|value| value.as_object())
    else {
        return (ParsedKeyring::default(), None);
    };

    let mut api_keys = HashMap::new();
    for (domain, blob) in entries {
        // Real writers only ever use literal domains. A `*` key would
        // be honored as a wildcard credential by rattler's storage
        // lookup (`*.com` matching every `.com` host), spraying the key
        // to hosts it was never meant for.
        if domain.contains('*') {
            continue;
        }
        // A single malformed domain entry (not a string, bad base64,
        // unparseable JSON, no `api_key`) is skipped rather than
        // dropping every other domain in the file.
        let Some(blob) = blob.as_str() else {
            continue;
        };
        let Ok(decoded) = BASE64_STANDARD.decode(blob) else {
            continue;
        };
        let Ok(credential) = serde_json::from_slice::<Credential>(&decoded) else {
            continue;
        };
        if let Some(api_key) = credential.api_key {
            api_keys.insert(domain.clone(), api_key);
        }
    }

    (ParsedKeyring { api_keys }, None)
}

/// Test-only construction of a [`ParsedKeyring`] directly from a map,
/// for `backend.rs`'s tests -- everything else in this crate only ever
/// sees one produced by [`load`], from real file bytes.
#[cfg(test)]
pub(crate) mod test_support {
    use super::ParsedKeyring;
    use std::collections::HashMap;

    pub(crate) fn from_map(api_keys: HashMap<String, String>) -> ParsedKeyring {
        ParsedKeyring { api_keys }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A synthetic fixture matching the real schema shape: outer
    /// `"Anaconda Cloud"` key, one domain, a base64-encoded JSON blob
    /// with every documented field present (no real key material).
    /// `version` is an integer, matching both real writers
    /// (anaconda-auth's `TokenInfo.version`, anaconda-cli's `u32`).
    fn fixture_blob(api_key: &str) -> String {
        let credential = serde_json::json!({
            "domain": "anaconda.com",
            "api_key": api_key,
            "username": "someone",
            "user_id": "00000000-0000-0000-0000-000000000000",
            "repo_tokens": [{"token": "tok", "org_name": "myorg"}],
            "version": 2,
        });
        BASE64_STANDARD.encode(serde_json::to_vec(&credential).unwrap())
    }

    fn write_fixture(dir: &tempfile::TempDir, domains: &[(&str, &str)]) -> PathBuf {
        let mut entries = serde_json::Map::new();
        for (domain, api_key) in domains {
            entries.insert(
                domain.to_string(),
                serde_json::Value::String(fixture_blob(api_key)),
            );
        }
        let mut sections = serde_json::Map::new();
        sections.insert(
            KEYRING_SECTION.to_string(),
            serde_json::Value::Object(entries),
        );
        let path = dir.path().join("keyring");
        std::fs::write(&path, serde_json::to_vec(&sections).unwrap()).unwrap();
        path
    }

    #[test]
    fn missing_file_resolves_to_empty_with_no_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let (parsed, diagnostic) = load(&dir.path().join("does-not-exist"));
        assert_eq!(parsed.api_key("anaconda.com"), None);
        assert_eq!(diagnostic, None);
    }

    #[test]
    fn corrupt_json_resolves_to_empty_with_a_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyring");
        std::fs::write(&path, b"not json").unwrap();
        let (parsed, diagnostic) = load(&path);
        assert_eq!(parsed.api_key("anaconda.com"), None);
        assert!(diagnostic.is_some());
    }

    #[test]
    fn a_real_shaped_fixture_parses_the_api_key_for_its_domain() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir, &[("anaconda.com", "secret-key")]);
        let (parsed, diagnostic) = load(&path);
        assert_eq!(diagnostic, None);
        assert_eq!(parsed.api_key("anaconda.com"), Some("secret-key"));
        assert_eq!(parsed.api_key("conda.anaconda.org"), None);
    }

    #[test]
    fn multiple_domains_each_resolve_independently() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            &dir,
            &[("anaconda.com", "key-a"), ("repo.mycompany.com", "key-b")],
        );
        let (parsed, _) = load(&path);
        assert_eq!(parsed.api_key("anaconda.com"), Some("key-a"));
        assert_eq!(parsed.api_key("repo.mycompany.com"), Some("key-b"));
    }

    /// A domain with no `api_key` in its blob (e.g. only `repo_tokens`
    /// populated) resolves to "no credential", not a hard error --
    /// repo-token auth is out of scope for v1.
    #[test]
    fn a_domain_with_no_api_key_has_no_credential() {
        let dir = tempfile::tempdir().unwrap();
        let credential = serde_json::json!({"domain": "anaconda.com", "version": 2});
        let blob = BASE64_STANDARD.encode(serde_json::to_vec(&credential).unwrap());
        let mut entries = serde_json::Map::new();
        entries.insert("anaconda.com".to_string(), serde_json::Value::String(blob));
        let mut sections = serde_json::Map::new();
        sections.insert(
            KEYRING_SECTION.to_string(),
            serde_json::Value::Object(entries),
        );
        let path = dir.path().join("keyring");
        std::fs::write(&path, serde_json::to_vec(&sections).unwrap()).unwrap();

        let (parsed, diagnostic) = load(&path);
        assert_eq!(diagnostic, None);
        assert_eq!(parsed.api_key("anaconda.com"), None);
    }

    /// One domain's malformed blob (bad base64) doesn't drop every other
    /// domain in the same file.
    #[test]
    fn a_malformed_entry_does_not_drop_other_domains() {
        let dir = tempfile::tempdir().unwrap();
        let mut entries = serde_json::Map::new();
        entries.insert(
            "broken.example.com".to_string(),
            serde_json::Value::String("not-valid-base64!!".to_string()),
        );
        entries.insert(
            "anaconda.com".to_string(),
            serde_json::Value::String(fixture_blob("secret-key")),
        );
        let mut sections = serde_json::Map::new();
        sections.insert(
            KEYRING_SECTION.to_string(),
            serde_json::Value::Object(entries),
        );
        let path = dir.path().join("keyring");
        std::fs::write(&path, serde_json::to_vec(&sections).unwrap()).unwrap();

        let (parsed, diagnostic) = load(&path);
        assert_eq!(diagnostic, None);
        assert_eq!(parsed.api_key("anaconda.com"), Some("secret-key"));
        assert_eq!(parsed.api_key("broken.example.com"), None);
    }

    /// A `*` domain key is never written by real writers and would be
    /// honored as a wildcard credential by rattler's storage lookup --
    /// it must be dropped at parse time.
    #[test]
    fn a_wildcard_domain_key_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(&dir, &[("*.com", "secret-key"), ("anaconda.com", "key-a")]);
        let (parsed, diagnostic) = load(&path);
        assert_eq!(diagnostic, None);
        assert_eq!(parsed.api_key("*.com"), None);
        assert_eq!(parsed.api_key("anaconda.com"), Some("key-a"));
    }

    #[test]
    fn an_override_wins_over_the_default() {
        assert_eq!(
            resolve(Some(PathBuf::from("/tmp/custom/keyring"))),
            Some(PathBuf::from("/tmp/custom/keyring"))
        );
    }

    #[test]
    fn without_an_override_the_default_is_used() {
        assert_eq!(resolve(None), default_keyring_path());
    }
}
