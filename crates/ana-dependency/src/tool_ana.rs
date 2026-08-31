//! Shared readers for ana's `[tool.ana]` TOML extension table:
//! `conda-channels` and `matchspec-dependencies`, with one shape
//! validation, whether the document is a `pyproject.toml`
//! (`ana-pyproject`) or a PEP 723 `# /// script` block (`ana-pep723`).
//! Both front ends convert [`InvalidToolAnaField`] into their own error
//! type.

use toml_edit::{Document, Item, TableLike};

/// A `[tool.ana]` field that failed shape validation: dotted TOML path,
/// with arrays addressed by index (`tool.ana.conda-channels[0]`), plus
/// optional detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidToolAnaField {
    pub path: String,
    pub description: Option<String>,
}

impl InvalidToolAnaField {
    fn new(path: String, description: Option<String>) -> Self {
        Self { path, description }
    }
}

/// `[tool.ana]`, if present and table-like. `None` covers both "absent"
/// and "present but not a table": `[tool.ana]` is a foreign namespace
/// (PEP 723 reserves `[tool.<name>]` for tools to use as they see fit),
/// so a malformed `tool`/`tool.ana` is silently treated the same as an
/// absent one; only the specific keys read under it are required to be
/// the right shape once looked up.
pub fn ana_table<'a>(doc: &'a Document<&str>) -> Option<&'a dyn TableLike> {
    doc.get("tool")
        .and_then(Item::as_table_like)
        .and_then(|tool| tool.get("ana"))
        .and_then(Item::as_table_like)
}

/// `[tool.ana] conda-channels`. Missing entirely means `None` (no
/// channel override); present is an array of non-empty strings. An
/// explicitly empty array is rejected, since silently meaning "override
/// to nothing" is never useful.
pub fn conda_channels(doc: &Document<&str>) -> Result<Option<Vec<String>>, InvalidToolAnaField> {
    let Some(ana) = ana_table(doc) else {
        return Ok(None);
    };
    let Some(item) = ana.get("conda-channels") else {
        return Ok(None);
    };
    let Some(arr) = item.as_array() else {
        return Err(InvalidToolAnaField::new(
            "tool.ana.conda-channels".to_string(),
            None,
        ));
    };
    if arr.is_empty() {
        return Err(InvalidToolAnaField::new(
            "tool.ana.conda-channels".to_string(),
            Some("must not be empty".to_string()),
        ));
    }
    let mut channels = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v.as_str().ok_or_else(|| {
            InvalidToolAnaField::new(format!("tool.ana.conda-channels[{i}]"), None)
        })?;
        if s.is_empty() {
            return Err(InvalidToolAnaField::new(
                format!("tool.ana.conda-channels[{i}]"),
                Some("must not be empty".to_string()),
            ));
        }
        channels.push(s.to_string());
    }
    Ok(Some(channels))
}

/// `[tool.ana.matchspec-dependencies]`. Missing entirely means zero
/// matchspec dependencies, not an error; present-but-not-an-array, or a
/// non-string entry, is. Returns `(original array index, raw matchspec
/// string)` pairs -- the index lets a string that fails to *parse*
/// later still be blamed on its real position in the document.
pub fn matchspec_dependencies<'a>(
    doc: &'a Document<&str>,
) -> Result<Vec<(usize, &'a str)>, InvalidToolAnaField> {
    let Some(ana) = ana_table(doc) else {
        return Ok(Vec::new());
    };
    let Some(item) = ana.get("matchspec-dependencies") else {
        return Ok(Vec::new());
    };
    let Some(arr) = item.as_array() else {
        return Err(InvalidToolAnaField::new(
            "tool.ana.matchspec-dependencies".to_string(),
            None,
        ));
    };
    let mut raw = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v.as_str().ok_or_else(|| {
            InvalidToolAnaField::new(format!("tool.ana.matchspec-dependencies[{i}]"), None)
        })?;
        raw.push((i, s));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn doc(source: &str) -> Document<&str> {
        Document::parse(source).unwrap()
    }

    #[test]
    fn an_absent_tool_ana_table_means_no_overrides() {
        let doc = doc("[project]\nname = \"x\"\n");
        assert!(ana_table(&doc).is_none());
        assert_eq!(conda_channels(&doc).unwrap(), None);
        assert_eq!(matchspec_dependencies(&doc).unwrap(), vec![]);
    }

    #[test]
    fn a_malformed_tool_ana_is_treated_as_absent() {
        let doc = doc("[tool]\nana = 1\n");
        assert!(ana_table(&doc).is_none());
        assert_eq!(conda_channels(&doc).unwrap(), None);
        assert_eq!(matchspec_dependencies(&doc).unwrap(), vec![]);
    }

    #[test]
    fn conda_channels_happy_path() {
        let doc = doc("[tool.ana]\nconda-channels = [\"conda-forge\", \"bioconda\"]\n");
        assert_eq!(
            conda_channels(&doc).unwrap(),
            Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
        );
    }

    #[test]
    fn an_empty_conda_channels_array_is_rejected() {
        let doc = doc("[tool.ana]\nconda-channels = []\n");
        let err = conda_channels(&doc).unwrap_err();
        assert_eq!(err.path, "tool.ana.conda-channels");
    }

    #[test]
    fn a_non_string_conda_channel_names_its_index() {
        let doc = doc("[tool.ana]\nconda-channels = [123]\n");
        let err = conda_channels(&doc).unwrap_err();
        assert_eq!(err.path, "tool.ana.conda-channels[0]");
    }

    #[test]
    fn an_empty_conda_channel_string_is_rejected() {
        let doc = doc("[tool.ana]\nconda-channels = [\"\"]\n");
        let err = conda_channels(&doc).unwrap_err();
        assert_eq!(err.path, "tool.ana.conda-channels[0]");
        assert_eq!(err.description, Some("must not be empty".to_string()));
    }

    #[test]
    fn matchspec_dependencies_keep_their_array_indices() {
        let doc = doc("[tool.ana]\nmatchspec-dependencies = [\"numpy\", \"scipy\"]\n");
        assert_eq!(
            matchspec_dependencies(&doc).unwrap(),
            vec![(0, "numpy"), (1, "scipy")]
        );
    }

    #[test]
    fn a_non_array_matchspec_dependencies_field_is_rejected() {
        let doc = doc("[tool.ana]\nmatchspec-dependencies = \"numpy\"\n");
        let err = matchspec_dependencies(&doc).unwrap_err();
        assert_eq!(err.path, "tool.ana.matchspec-dependencies");
    }
}
