//! [`ConfigDocument`]: comment- and unknown-field-preserving `config.toml`
//! read/write, backed directly by `toml_edit`'s `Item`/`Array`/`Value` API
//! (no `serde`) -- only the four known keys are ever touched, so
//! everything else in the file survives untouched.

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Value};
use url::Url;

use crate::error::ConfigError;
use crate::schema::{parse_uri, reject_file_channel, AnaConfig, Key};

/// A parsed `config.toml`, held as a `toml_edit::DocumentMut` so writes
/// (`set_channels`/`set_uri`) can replace one key in place while leaving
/// every other key, table, and comment byte-identical.
pub struct ConfigDocument {
    doc: DocumentMut,
}

/// A `config.toml` this large is never legitimate. Enforced by
/// [`ConfigDocument::read`] via `stat`, before the file is read into
/// memory.
const MAX_CONFIG_FILE_SIZE: u64 = 1024 * 1024;

impl ConfigDocument {
    /// An empty document, as if `config.toml` didn't exist.
    pub fn empty() -> Self {
        Self {
            doc: DocumentMut::new(),
        }
    }

    /// Parse `text` directly (no file I/O).
    pub fn parse(text: &str) -> Result<Self, toml_edit::TomlError> {
        Ok(Self { doc: text.parse()? })
    }

    /// Missing file reads as [`Self::empty`] -- never an error. A file
    /// over [`MAX_CONFIG_FILE_SIZE`] is [`ConfigError::TooLarge`].
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::metadata(path) {
            Ok(metadata) => {
                let size = metadata.len();
                if size > MAX_CONFIG_FILE_SIZE {
                    return Err(ConfigError::TooLarge {
                        path: path.to_path_buf(),
                        size,
                        max: MAX_CONFIG_FILE_SIZE,
                    });
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty())
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }

        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Every field this document has, validated. A field with the wrong
    /// TOML shape or an invalid `pypi_to_conda_uri` fails the whole call,
    /// not just that field.
    pub fn to_config(&self) -> Result<AnaConfig, ConfigError> {
        Ok(AnaConfig {
            default_channels: self.get_channels(Key::DefaultChannels)?,
            allowed_channels: self.get_channels(Key::AllowedChannels)?,
            dry_solve_channels: self.get_channels(Key::DrySolveChannels)?,
            pypi_to_conda_uri: self.get_uri(Key::PypiToCondaUri)?,
        })
    }

    /// Read `key` as an array of strings; `None` if the key is absent.
    pub fn get_channels(&self, key: Key) -> Result<Option<Vec<String>>, ConfigError> {
        let Some(item) = self.doc.get(key.as_str()) else {
            return Ok(None);
        };
        let array = item.as_array().ok_or_else(|| ConfigError::InvalidField {
            key,
            message: "expected an array of strings".to_string(),
        })?;
        let mut out = Vec::with_capacity(array.len());
        for (i, value) in array.iter().enumerate() {
            let s = value.as_str().ok_or_else(|| ConfigError::InvalidField {
                key,
                message: format!("element {i} is not a string"),
            })?;
            reject_file_channel(key, s)?;
            out.push(s.to_string());
        }
        Ok(Some(out))
    }

    /// Read `key` as a validated URI; `None` if the key is absent.
    pub fn get_uri(&self, key: Key) -> Result<Option<Url>, ConfigError> {
        let Some(item) = self.doc.get(key.as_str()) else {
            return Ok(None);
        };
        let s = item.as_str().ok_or_else(|| ConfigError::InvalidField {
            key,
            message: "expected a string".to_string(),
        })?;
        parse_uri(s).map(Some)
    }

    /// Replaces (or inserts) `key` as a single-line array, leaving every
    /// other key/comment/table in the document untouched. `values` may be
    /// empty (`key = []`).
    pub fn set_channels(&mut self, key: Key, values: &[String]) {
        let mut array = Array::new();
        for value in values {
            array.push(value.as_str());
        }
        self.doc[key.as_str()] = Item::Value(Value::Array(array));
    }

    /// Replaces (or inserts) `key` as a single string value.
    pub fn set_uri(&mut self, key: Key, value: &Url) {
        self.doc[key.as_str()] = toml_edit::value(value.as_str());
    }

    /// Write this document out, atomically.
    pub fn write(&self, path: &Path) -> Result<(), ConfigError> {
        ana_fs_util::write_atomic(path, self.doc.to_string().as_bytes()).map_err(|source| {
            ConfigError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const ALL_FIELDS: &str = r#"
default_channels = ["conda-forge", "bioconda"]
allowed_channels = ["conda-forge"]
dry_solve_channels = ["defaults"]
pypi_to_conda_uri = "https://example.com/mapping.json"
"#;

    #[test]
    fn valid_config_with_all_fields_set() {
        let doc = ConfigDocument::parse(ALL_FIELDS).unwrap();
        let config = doc.to_config().unwrap();
        assert_eq!(
            config.default_channels,
            Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
        );
        assert_eq!(
            config.allowed_channels,
            Some(vec!["conda-forge".to_string()])
        );
        assert_eq!(
            config.dry_solve_channels,
            Some(vec!["defaults".to_string()])
        );
        assert_eq!(
            config.pypi_to_conda_uri,
            Some(Url::parse("https://example.com/mapping.json").unwrap())
        );
    }

    #[test]
    fn each_field_individually_absent() {
        let doc = ConfigDocument::parse(r#"default_channels = ["conda-forge"]"#).unwrap();
        let config = doc.to_config().unwrap();
        assert_eq!(config.allowed_channels, None);
        assert_eq!(config.dry_solve_channels, None);
        assert_eq!(config.pypi_to_conda_uri, None);
    }

    #[test]
    fn empty_document_is_default_config() {
        let doc = ConfigDocument::empty();
        assert_eq!(doc.to_config().unwrap(), AnaConfig::default());
    }

    #[test]
    fn non_array_default_channels_fails_the_whole_read() {
        let doc = ConfigDocument::parse(r#"default_channels = "oops""#).unwrap();
        assert!(matches!(
            doc.to_config(),
            Err(ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn non_string_element_fails_the_whole_read() {
        let doc = ConfigDocument::parse("default_channels = [1, 2]").unwrap();
        assert!(matches!(
            doc.to_config(),
            Err(ConfigError::InvalidField {
                key: Key::DefaultChannels,
                ..
            })
        ));
    }

    #[test]
    fn file_scheme_channel_fails_the_whole_read() {
        let doc =
            ConfigDocument::parse(r#"allowed_channels = ["file:///tmp/local-channel"]"#).unwrap();
        assert!(matches!(
            doc.to_config(),
            Err(ConfigError::InvalidField {
                key: Key::AllowedChannels,
                ..
            })
        ));
    }

    #[test]
    fn non_string_uri_fails_the_whole_read() {
        let doc = ConfigDocument::parse("pypi_to_conda_uri = 1").unwrap();
        assert!(matches!(
            doc.to_config(),
            Err(ConfigError::InvalidField {
                key: Key::PypiToCondaUri,
                ..
            })
        ));
    }

    #[test]
    fn bad_scheme_uri_fails() {
        let doc = ConfigDocument::parse(r#"pypi_to_conda_uri = "ftp://example.com/x""#).unwrap();
        assert!(matches!(
            doc.to_config(),
            Err(ConfigError::InvalidUri {
                key: Key::PypiToCondaUri,
                ..
            })
        ));
    }

    #[test]
    fn file_and_https_uris_succeed() {
        let doc = ConfigDocument::parse(r#"pypi_to_conda_uri = "file:///tmp/x.json""#).unwrap();
        assert!(doc.to_config().is_ok());
        let doc =
            ConfigDocument::parse(r#"pypi_to_conda_uri = "https://example.com/x.json""#).unwrap();
        assert!(doc.to_config().is_ok());
    }

    #[test]
    fn set_channels_preserves_other_keys_and_comments() {
        let mut doc = ConfigDocument::parse(
            r#"
# a comment
allowed_channels = ["conda-forge"]

[some_table]
key = "value"
"#,
        )
        .unwrap();
        doc.set_channels(
            Key::DefaultChannels,
            &["bioconda".to_string(), "defaults".to_string()],
        );
        let rendered = doc.doc.to_string();
        assert!(rendered.contains("# a comment"));
        assert!(rendered.contains("[some_table]"));
        assert!(rendered.contains(r#"key = "value""#));
        assert!(rendered.contains(r#"allowed_channels = ["conda-forge"]"#));

        let reparsed = ConfigDocument::parse(&rendered).unwrap();
        let config = reparsed.to_config().unwrap();
        assert_eq!(
            config.default_channels,
            Some(vec!["bioconda".to_string(), "defaults".to_string()])
        );
        assert_eq!(
            config.allowed_channels,
            Some(vec!["conda-forge".to_string()])
        );
    }

    #[test]
    fn set_uri_preserves_other_keys_and_comments() {
        let mut doc = ConfigDocument::parse(
            r#"
# a comment
allowed_channels = ["conda-forge"]
"#,
        )
        .unwrap();
        let url = Url::parse("https://example.com/mapping.json").unwrap();
        doc.set_uri(Key::PypiToCondaUri, &url);
        let rendered = doc.doc.to_string();
        assert!(rendered.contains("# a comment"));
        assert!(rendered.contains(r#"allowed_channels = ["conda-forge"]"#));

        let reparsed = ConfigDocument::parse(&rendered).unwrap();
        assert_eq!(reparsed.to_config().unwrap().pypi_to_conda_uri, Some(url));
    }

    #[test]
    fn read_of_a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist/config.toml");
        let doc = ConfigDocument::read(&path).unwrap();
        assert_eq!(doc.to_config().unwrap(), AnaConfig::default());
    }

    #[test]
    fn read_of_a_corrupt_file_is_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"not [toml").unwrap();
        assert!(matches!(
            ConfigDocument::read(&path),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn read_of_an_oversized_file_is_rejected_before_loading_its_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // A sparse file one byte over the cap, exercising the `stat`
        // check without writing a real megabyte to disk.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_FILE_SIZE + 1).unwrap();
        drop(file);

        assert!(matches!(
            ConfigDocument::read(&path),
            Err(ConfigError::TooLarge {
                size,
                max: MAX_CONFIG_FILE_SIZE,
                ..
            }) if size == MAX_CONFIG_FILE_SIZE + 1
        ));
    }

    #[test]
    fn read_of_a_file_at_exactly_the_cap_still_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Padding is a TOML comment, so the file is still valid.
        let padding = "#"
            .repeat((MAX_CONFIG_FILE_SIZE - "default_channels = [\"x\"]\n".len() as u64) as usize);
        std::fs::write(&path, format!("default_channels = [\"x\"]\n{padding}")).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_CONFIG_FILE_SIZE
        );

        let doc = ConfigDocument::read(&path).unwrap();
        assert_eq!(
            doc.to_config().unwrap().default_channels,
            Some(vec!["x".to_string()])
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut doc = ConfigDocument::empty();
        doc.set_channels(Key::DefaultChannels, &["conda-forge".to_string()]);
        doc.write(&path).unwrap();

        let reread = ConfigDocument::read(&path).unwrap();
        assert_eq!(
            reread.to_config().unwrap().default_channels,
            Some(vec!["conda-forge".to_string()])
        );
    }
}
