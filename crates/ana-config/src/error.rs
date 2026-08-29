//! [`ConfigError`], every way reading/parsing/writing `config.toml` (or a
//! `commercial-config` build's compiled-in equivalent) can fail.

/// Every way `config.toml` I/O, parsing, or validation can fail.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[error("could not write {path}: {source}")]
    Write {
        path: std::path::PathBuf,
        source: std::io::Error,
    },

    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: std::path::PathBuf,
        source: toml_edit::TomlError,
    },

    #[error("`{key}` is invalid: {message}")]
    InvalidField { key: crate::Key, message: String },

    #[error("`{key}` is not a valid pypi_to_conda_uri: {reason}")]
    InvalidUri { key: crate::Key, reason: String },

    #[error(
        "{path} is {size} bytes, larger than the {max}-byte limit for a config.toml; \
         refusing to read it"
    )]
    TooLarge {
        path: std::path::PathBuf,
        size: u64,
        max: u64,
    },
}
