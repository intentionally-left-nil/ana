//! Black-box test for the `ANA_CONFIG_PATH` wiring end to end: the real
//! `ana` binary runs as a subprocess with the override in the child's
//! environment, so this process's environment is never mutated.
#![cfg(not(feature = "commercial-config"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn config_set_writes_to_and_reads_back_from_the_ana_config_path_override() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_ana"))
        .args(["config", "set", "default_channels", "conda-forge"])
        .env("ANA_CONFIG_PATH", &config_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "a valid value must be accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        config_path.exists(),
        "the override location must be written"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ana"))
        .args(["config", "get", "default_channels"])
        .env("ANA_CONFIG_PATH", &config_path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("conda-forge"),
        "the written value must read back: {stdout}"
    );
}
