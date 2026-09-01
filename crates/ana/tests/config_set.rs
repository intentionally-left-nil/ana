//! Black-box tests for `ana config set`: the real binary runs as a
//! subprocess so `ANA_CONFIG_PATH` is set per-process. Setting it
//! in-process is not an option: `std::env::set_var` is process-wide
//! state, and mutating it races every concurrent test's own environment
//! reads (`cargo test` runs a binary's tests concurrently by default).
#![cfg(not(feature = "commercial-config"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

/// `config set` rejects an invalid channel value -- a `file://` channel,
/// or a misplaced `/*` wildcard -- before ever writing `config.toml`,
/// with the offending key named in the error (via
/// `ana_config::validate_channel`).
#[test]
fn config_set_rejects_invalid_channel_values_with_the_key_named() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    for (value, reason) in [
        ("file:///tmp/local-channel", "local filesystem path"),
        // A `/*` wildcard is legal in `allowed_channels` but not in
        // `default_channels`.
        (
            "https://example.com/pkgs/main/*",
            "only allowed in allowed_channels",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ana"))
            .args(["config", "set", "default_channels", value])
            .env("ANA_CONFIG_PATH", &config_path)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{value} must be rejected");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("`default_channels` is invalid"),
            "the offending key must be named: {stderr}"
        );
        assert!(
            stderr.contains(reason),
            "the reason must be given: {stderr}"
        );
        assert!(
            !config_path.exists(),
            "a rejected value must never be written"
        );
    }
}
