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
/// pypi/conda names differ. Returns `Ok(None)` on a 304 -- the caller
/// already knows the data it has is current.
///
/// Names that fail PEP 503 / CEP-26 normalization on either side are
/// skipped rather than failing the whole batch: one malformed entry in an
/// upstream table of thousands of names shouldn't take the rest down with
/// it.
pub(crate) fn fetch_full(
    client: &dyn HttpClient,
    url: &str,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Option<FetchedMapping>, FetchError> {
    let response = client.get(url, etag, last_modified)?;
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
    // The vast majority of entries have identical names on both sides --
    // only a small fraction survive this filter in practice.
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

    /// `PackageName::from_str("")` used to succeed under this crate's
    /// original `uv-normalize` `0.9.7` pin -- `normalize`/`is_normalized`
    /// in `uv-normalize`'s own `lib.rs` iterate over the input's bytes and
    /// never initialize a "seen a character" flag, so an empty string's
    /// loop body simply never runs and both functions report success,
    /// which `validate_and_normalize_ref` then passed straight through as
    /// `Ok(SmallString::from(""))`. That means an upstream mapping entry
    /// with an empty `pypi_name` or `conda_name` string did *not* hit this
    /// function's `else { continue }` skip branch on the old pin: it
    /// normalized to `""` on whichever side was empty and, since `""` !=
    /// the other (non-empty) side's normalized name in practice, got
    /// inserted into the filtered mapping as a genuinely bogus entry (a
    /// spurious `PackageName("")` masquerading as a real conda or PyPI
    /// name) instead of being dropped as malformed input the way
    /// `skips_entries_that_fail_normalization` above already covers for
    /// leading-punctuation and embedded-space names.
    ///
    /// **Fixed by the `uv-normalize` 0.9.7 -> 0.12.6 bump**: uv#19435
    /// ("Reject empty string as an invalid package name") adds an
    /// early-return guard for the empty case to both `normalize` and
    /// `is_normalized`, so `PackageName::from_str("")` now correctly fails
    /// -- confirmed directly against `uv_normalize` 0.12.6 (and against
    /// `0.9.7`, where this exact test failed before the bump), not
    /// assumed from the changelog. `normalize_and_filter`'s own code
    /// didn't change at all for this fix to take effect: the `let-else`
    /// guard on each `PackageName::from_str` call was always correct, it
    /// was `uv-normalize` silently returning `Ok` for `""` that was wrong.
    #[test]
    fn skips_entries_with_an_empty_name_on_either_side() {
        let mut raw = HashMap::new();
        raw.insert(String::new(), "conda-name".to_string());
        raw.insert("pypi-name".to_string(), String::new());

        let filtered = normalize_and_filter(raw);

        assert!(filtered.is_empty(), "{filtered:?}");
    }
}
