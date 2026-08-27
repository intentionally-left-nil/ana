//! SHA-256 helpers for the stage-1 staleness shortcut
//! (`investigations/lock_generation_algorithm.md`, "Stage 1 / Stage 2").
//!
//! Two hashes live in the cache file: a whole-file hash of
//! `pyproject.toml` ([`content_hash`]) and a hash of the current platform's
//! *parsed* lock section ([`crate::lock_file::PlatformSection::hash`] --
//! implemented there, over a canonical serialization, so serializer or
//! formatting drift doesn't cause spurious misses).

use sha2::{Digest, Sha256};

/// Lowercase-hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // Writing two hex chars into a String never fails.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn sha256_hex_matches_known_vector() {
        // Well-known test vector: SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // ... and of "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
