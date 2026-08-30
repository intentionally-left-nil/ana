//! Fetching and normalizing the upstream `{"pypi_name": "conda_name"}`
//! table down to just the entries that differ.

use std::collections::HashMap;
use std::str::FromStr;

use uv_normalize::PackageName;

use crate::error::FetchError;
use crate::http::{GetResponse, HttpClient};

/// Result of a successful full download: the filtered, normalized mapping
/// plus the validators to persist alongside it.
pub(crate) struct FetchedMapping {
    pub mapping: HashMap<String, String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Performs a (conditionally, if validators are given) GET, parses the JSON
/// body, and filters it down to only the entries whose normalized
/// pypi/conda names differ. Returns `Ok(None)` on a 304.
///
/// Names that fail PEP 503 / CEP-26 normalization on either side are
/// skipped rather than failing the whole batch.
pub(crate) async fn fetch_full(
    client: &dyn HttpClient,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Option<FetchedMapping>, FetchError> {
    let response = client.get(url, etag, last_modified).await?;
    let (body, etag, last_modified) = match response {
        GetResponse::NotModified => return Ok(None),
        GetResponse::Ok {
            body,
            etag,
            last_modified,
        } => (body, etag, last_modified),
    };

    let raw: HashMap<String, String> = serde_json::from_slice(&body)?;
    let mapping = normalize_and_filter(raw);
    Ok(Some(FetchedMapping {
        mapping,
        etag,
        last_modified,
    }))
}

fn normalize_and_filter(raw: HashMap<String, String>) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for (pypi_name, conda_name) in raw {
        let Ok(pypi_norm) = PackageName::from_str(&pypi_name) else {
            continue;
        };
        let Ok(conda_norm) = PackageName::from_str(&conda_name) else {
            continue;
        };
        if pypi_norm.as_str() != conda_norm.as_str() {
            result.insert(pypi_norm.to_string(), conda_norm.to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn keeps_only_differing_normalized_names() {
        let mut raw = HashMap::new();
        raw.insert("Foo-Bar".to_string(), "foo-bar".to_string()); // same after normalization
        raw.insert("beautifulsoup4".to_string(), "beautifulsoup4".to_string()); // identical
        raw.insert("PyYAML".to_string(), "pyyaml".to_string()); // same after normalization
        raw.insert("opencv-python".to_string(), "py-opencv".to_string()); // genuinely differs

        let filtered = normalize_and_filter(raw);

        assert_eq!(filtered.len(), 1);
        assert_eq!(
            filtered.get("opencv-python"),
            Some(&"py-opencv".to_string())
        );
    }

    #[test]
    fn skips_entries_that_fail_normalization() {
        let mut raw = HashMap::new();
        raw.insert("-leading-punctuation".to_string(), "conda-name".to_string());
        raw.insert(
            "valid-name".to_string(),
            "also valid but has a space".to_string(),
        );

        let filtered = normalize_and_filter(raw);

        assert!(filtered.is_empty());
    }

    /// `PackageName::from_str("")` must fail; an entry with an empty
    /// `pypi_name` or `conda_name` must be skipped, not inserted as a
    /// bogus `PackageName("")`.
    #[test]
    fn skips_entries_with_an_empty_name_on_either_side() {
        let mut raw = HashMap::new();
        raw.insert(String::new(), "conda-name".to_string());
        raw.insert("pypi-name".to_string(), String::new());

        let filtered = normalize_and_filter(raw);

        assert!(filtered.is_empty(), "{filtered:?}");
    }
}
