//! `ana`'s user-editable `config.toml`: schema, validation, and
//! comment-preserving read/write.
//!
//! - [`schema`] defines the fields ([`AnaConfig`]) and the [`Key`]
//!   enum `ana config get`/`set` address them by.
//! - [`document`] ([`ConfigDocument`]) reads/writes `config.toml` through
//!   `toml_edit`'s own API directly, so unknown keys, tables, and
//!   comments survive untouched -- only the known keys are ever
//!   read or written.
//! - [`path`] resolves `config.toml`'s OS-appropriate location
//!   ([`config_path`]/[`default_config_path`]), matching the pattern
//!   `ana-pypi-conda-map/src/cache_dir.rs` uses for the cache dir.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod document;
mod error;
mod path;
mod schema;

pub use document::ConfigDocument;
pub use error::ConfigError;
pub use path::{config_path, default_config_path};
pub use schema::{
    parse_uri, validate_channel, validate_sandbox_policy, AnaConfig, Key, ParseKeyError,
    DEFAULT_ALLOWED_CHANNELS, DEFAULT_CHANNELS, DEFAULT_DRY_SOLVE_CHANNELS,
    DEFAULT_PYPI_TO_CONDA_URI, DEFAULT_SANDBOXED_CHANNELS,
};

/// Parse `text` directly (no file I/O) into a validated [`AnaConfig`] --
/// used by `ana`'s `build.rs` to validate a compiled-in config.
pub fn parse_str(text: &str) -> Result<AnaConfig, ConfigError> {
    let doc = ConfigDocument::parse(text).map_err(|source| ConfigError::Parse {
        path: std::path::PathBuf::from("<compiled config>"),
        source,
    })?;
    doc.to_config()
}

/// The on-disk config at `path`. A missing file reads as
/// [`AnaConfig::default`] -- never an error; only a *present but
/// invalid* file is.
pub fn load(path: &std::path::Path) -> Result<AnaConfig, ConfigError> {
    ConfigDocument::read(path)?.to_config()
}
