//! Integration tests for the `commercial-config` feature: only meaningful
//! when built with it, so the whole file is gated on it.
#![cfg(feature = "commercial-config")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Exercises `crates/ana/tests/fixtures/compiled_config.toml` (baked in by
/// `build.rs` via `ANA_COMPILED_CONFIG_PATH`) against `ana::config`'s
/// public API. Kept as a single test function because `ANA_CONFIG_PATH`
/// is process-wide state and `cargo test` runs tests in the same binary
/// concurrently by default.
#[test]
fn compiled_config_replaces_disk_wholesale_and_disables_set() {
    let resolved = ana::config::resolve_config().unwrap();
    assert_eq!(resolved.default_channels, vec!["conda-forge".to_string()]);
    assert_eq!(
        resolved.allowed_channels,
        Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
    );
    // Unlike a community build, a `commercial-config` build never picks
    // up `ana_config::DEFAULT_DRY_SOLVE_CHANNELS` for an absent
    // `dry_solve_channels`: the fixture doesn't set it, so it must stay
    // unset here.
    assert_eq!(resolved.dry_solve_channels, None);
    // The fixture sets this to a value deliberately different from
    // `ana_config::DEFAULT_PYPI_TO_CONDA_URI`, so the assertion can only
    // pass if the compiled value round-trips through `build.rs`'s codegen.
    assert_eq!(
        resolved.pypi_to_conda_uri.as_str(),
        "https://custom.invalid/pypi_to_conda.json"
    );

    // A different, disk-backed config.toml must still be ignored.
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
