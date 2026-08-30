//! The opaque key naming one keyed environment directory (a project's
//! `.ana/<key>/`, or the global cache's `<key>/`).

use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

/// A hex-encoded key naming one keyed environment directory. Which
/// constructor produced it -- and therefore how many hex characters it
/// has -- is not part of its contract: callers compare and display it,
/// never parse it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvironmentKey(String);

impl EnvironmentKey {
    /// SHA-256 over the normalized, sorted, deduplicated, comma-joined
    /// group names, truncated to 8 hex characters -- byte-for-byte the
    /// hash existing projects' `.ana/<hash>/` directories are already
    /// named with.
    pub fn from_symbolic_names(names: &[&str]) -> Self {
        let mut sorted: Vec<&str> = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        Self(hex_prefix(&Sha256::digest(sorted.join(",").as_bytes()), 4))
    }

    /// SHA-256 over the sorted (not deduplicated) canonical matchspec
    /// strings a content-only declaration converts to. The full digest,
    /// since there is no legacy truncated form to match here.
    pub fn from_content(canonical: &[&str]) -> Self {
        let mut sorted: Vec<&str> = canonical.to_vec();
        sorted.sort_unstable();
        Self(hex_prefix(
            &Sha256::digest(sorted.join("\n").as_bytes()),
            32,
        ))
    }

    /// Like [`from_content`](Self::from_content), plus the requested
    /// group names hashed in as well -- for a project declaration
    /// combined with extra ad hoc requirements.
    pub fn from_names_and_content(names: &[&str], canonical: &[&str]) -> Self {
        let mut sorted_names: Vec<&str> = names.to_vec();
        sorted_names.sort_unstable();
        let mut sorted_canonical: Vec<&str> = canonical.to_vec();
        sorted_canonical.sort_unstable();
        let signature = format!(
            "{}\0{}",
            sorted_names.join(","),
            sorted_canonical.join("\n")
        );
        Self(hex_prefix(&Sha256::digest(signature.as_bytes()), 32))
    }

    /// SHA-256 over `path`'s string form -- a script's identity key.
    pub fn from_identity_path(path: &Path) -> Self {
        Self(hex_prefix(
            &Sha256::digest(path.to_string_lossy().as_bytes()),
            32,
        ))
    }

    /// Wraps an already-computed key string verbatim, for a caller that
    /// discovered a directory name on disk (`ana clean`, enumerating
    /// `.ana/`) and needs to re-derive its paths without knowing which
    /// constructor produced it.
    pub fn from_raw(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The first `bytes` bytes of `digest`, hex-encoded.
fn hex_prefix(digest: &[u8], bytes: usize) -> String {
    let mut hex = String::with_capacity(bytes * 2);
    for byte in &digest[..bytes] {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn symbolic_names_match_documented_legacy_vectors() {
        // Worked examples, pinned so the implementation can't silently
        // drift from existing projects' `.ana/<hash>/` directories.
        assert_eq!(
            EnvironmentKey::from_symbolic_names(&["dev"]).as_str(),
            "ef260e9a"
        );
        assert_eq!(
            EnvironmentKey::from_symbolic_names(&["dev", "doc"]).as_str(),
            "e62119cb"
        );
        assert_eq!(
            EnvironmentKey::from_symbolic_names(&["doc", "other"]).as_str(),
            "4a091557"
        );
    }

    #[test]
    fn symbolic_names_are_order_and_repeat_invariant() {
        assert_eq!(
            EnvironmentKey::from_symbolic_names(&["doc", "dev"]),
            EnvironmentKey::from_symbolic_names(&["dev", "doc"]),
        );
        assert_eq!(
            EnvironmentKey::from_symbolic_names(&["dev", "dev"]),
            EnvironmentKey::from_symbolic_names(&["dev"]),
        );
    }

    #[test]
    fn content_keys_are_order_invariant_but_not_dedupe_invariant() {
        assert_eq!(
            EnvironmentKey::from_content(&["numpy", "ruff"]),
            EnvironmentKey::from_content(&["ruff", "numpy"]),
        );
        assert_ne!(
            EnvironmentKey::from_content(&["ruff", "ruff"]),
            EnvironmentKey::from_content(&["ruff"]),
            "content keys sort but do not dedupe"
        );
    }

    #[test]
    fn content_keys_are_64_hex_characters() {
        assert_eq!(EnvironmentKey::from_content(&["numpy"]).as_str().len(), 64);
        assert_eq!(
            EnvironmentKey::from_names_and_content(&["dev"], &["numpy"])
                .as_str()
                .len(),
            64
        );
        assert_eq!(
            EnvironmentKey::from_identity_path(Path::new("/tmp/script.py"))
                .as_str()
                .len(),
            64
        );
    }

    #[test]
    fn names_and_content_differs_from_either_alone() {
        let names_and_content = EnvironmentKey::from_names_and_content(&["dev"], &["numpy"]);
        assert_ne!(names_and_content, EnvironmentKey::from_content(&["numpy"]));
        assert_ne!(
            names_and_content.as_str(),
            EnvironmentKey::from_symbolic_names(&["dev"]).as_str()
        );
    }

    #[test]
    fn identity_path_is_deterministic_and_path_sensitive() {
        let a = EnvironmentKey::from_identity_path(Path::new("/tmp/a.py"));
        let b = EnvironmentKey::from_identity_path(Path::new("/tmp/b.py"));
        assert_eq!(
            a,
            EnvironmentKey::from_identity_path(Path::new("/tmp/a.py"))
        );
        assert_ne!(a, b);
    }

    #[test]
    fn from_raw_wraps_verbatim() {
        assert_eq!(EnvironmentKey::from_raw("abcd1234").as_str(), "abcd1234");
    }
}
