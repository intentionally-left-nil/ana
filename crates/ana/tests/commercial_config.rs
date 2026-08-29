//! Integration tests for the `commercial-config` feature: only meaningful
//! when built with it, so the whole file is gated on it.
#![cfg(feature = "commercial-config")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Exercises `crates/ana/tests/fixtures/compiled_config.toml` (baked in by
/// `build.rs` via `ANA_COMPILED_CONFIG_PATH`, set by `make
/// test-commercial-config`) against `ana::config`'s public API. A single
/// test function, not several: `ANA_CONFIG_PATH` is process-wide state,
/// and `cargo test` runs tests in the same binary concurrently by
/// default, so every env-touching assertion lives in one test to avoid a
/// race with another test over the same environment variable.
#[test]
fn compiled_config_replaces_disk_wholesale_and_disables_set() {
    // The compiled values, with no `config.toml` in play at all yet.
    let resolved = ana::config::resolve_config().unwrap();
    assert_eq!(resolved.default_channels, vec!["conda-forge".to_string()]);
    assert_eq!(
        resolved.allowed_channels,
        Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
    );

    // Point `ANA_CONFIG_PATH` at a *different*, disk-backed config.toml
    // and confirm `resolve_config()` still returns the fixture's
    // compiled values, unchanged -- proving disk is genuinely never
    // consulted in this build, not just "compiled wins when both are
    // set."
    let dir = tempfile::tempdir().unwrap();
    let disk_config_path = dir.path().join("config.toml");
    std::fs::write(
        &disk_config_path,
        "default_channels = [\"this-should-never-be-read\"]\n",
    )
    .unwrap();
    std::env::set_var("ANA_CONFIG_PATH", &disk_config_path);

    let resolved_again = ana::config::resolve_config().unwrap();
    assert_eq!(resolved_again, resolved, "disk must never be consulted");

    // `config_set` is disabled outright and never touches the disk file.
    let before = std::fs::read_to_string(&disk_config_path).unwrap();
    let result = ana::config::config_set(ana_config::Key::DefaultChannels, &["x".to_string()]);
    assert!(matches!(result, Err(ana::Error::ConfigSetDisabled)));
    assert_eq!(
        std::fs::read_to_string(&disk_config_path).unwrap(),
        before,
        "a disabled `set` must never touch config.toml, even one ANA_CONFIG_PATH points at"
    );

    std::env::remove_var("ANA_CONFIG_PATH");
}
