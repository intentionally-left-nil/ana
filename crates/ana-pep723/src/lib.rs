//! [PEP 723](https://peps.python.org/pep-0723/) inline script metadata:
//! [`parse`] reads a Python source file's `# /// script ... # ///`
//! comment block, if any, into the same [`ana_dependency::Dependency`]/
//! `requires-python`/channel-override pieces `ana-pyproject`'s
//! `[tool.ana]` extension produces from a `pyproject.toml` --
//! `conda-channels` and `matchspec-dependencies`, under the same
//! `[tool.ana]` table PEP 723 itself reserves for tool-specific
//! configuration.
//!
//! Deciding whether a candidate file is even worth reading this way (and
//! unifying a [`ScriptRequirements`] into an
//! `ana_requirements::RequirementSet`) is a caller's job -- see `ana`'s
//! own `script` module.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod block;

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use ana_dependency::Dependency;
use toml_edit::Document;
use uv_pep440::VersionSpecifiers;
use uv_pep508::Requirement;

/// What a PEP 723 `# /// script` block declares: [`ana_dependency`]
/// dependencies (PEP 508 `dependencies`, plus ana's own
/// `tool.ana.matchspec-dependencies` extension -- PEP 508 entries first,
/// then matchspec entries, the same merge order `ana-pyproject`'s
/// `[tool.ana]` extension uses), its `requires-python`, and its
/// `tool.ana.conda-channels` override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRequirements {
    pub dependencies: Vec<Dependency>,
    pub requires_python: Option<VersionSpecifiers>,
    pub channels: Option<Vec<String>>,
}

/// Parse `source` (a Python file's full text) for its PEP 723 `script`
/// metadata block.
///
/// `Ok(None)` when no such block exists at all: the file is not a PEP
/// 723 script, not an error. `Err` once a block is found but fails to
/// parse -- bad TOML, an invalid requirement/matchspec string, or a
/// second `script` block (PEP 723 permits at most one).
pub fn parse(source: &str) -> Result<Option<ScriptRequirements>, Pep723Error> {
    let content = match block::extract_script_block(source) {
        Ok(Some(content)) => content,
        Ok(None) => return Ok(None),
        Err(block::MultipleBlocks) => {
            return Err(InvalidField::new(
                "",
                Some(
                    "multiple `# /// script` metadata blocks found (PEP 723 permits at most one)"
                        .to_string(),
                ),
            )
            .into());
        }
    };

    let doc = Document::<&str>::parse(&content)
        .map_err(|err| InvalidField::new("script", Some(err.to_string())))?;

    let requires_python = extract_requires_python(&doc)?;
    let channels = ana_dependency::tool_ana::conda_channels(&doc).map_err(InvalidField::from)?;
    let dependencies_raw = extract_dependencies(&doc)?;
    let matchspec_raw =
        ana_dependency::tool_ana::matchspec_dependencies(&doc).map_err(InvalidField::from)?;

    let mut errors: Vec<InvalidField> = Vec::new();
    let mut dependencies = Vec::with_capacity(dependencies_raw.len() + matchspec_raw.len());

    for (i, raw) in &dependencies_raw {
        match Requirement::from_str(raw) {
            Ok(req) => dependencies.push(Dependency::Pep508(req)),
            Err(err) => errors.push(InvalidField::new(
                &format!("dependencies[{i}]"),
                Some(err.to_string()),
            )),
        }
    }
    for (i, raw) in &matchspec_raw {
        match ana_dependency::parse_matchspec(raw) {
            Ok(spec) => dependencies.push(Dependency::Matchspec(Box::new(spec))),
            Err(err) => errors.push(InvalidField::new(
                &format!("tool.ana.matchspec-dependencies[{i}]"),
                Some(err.to_string()),
            )),
        }
    }

    if !errors.is_empty() {
        return Err(Pep723Error::new(errors));
    }

    Ok(Some(ScriptRequirements {
        dependencies,
        requires_python,
        channels,
    }))
}

/// Every invalid field found in one `# /// script` block. Never
/// constructed with an empty field list -- if nothing is invalid,
/// parsing succeeds. Mirrors `ana_pyproject::PyprojectError`'s own
/// contract: one field means a structural check failed and parsing
/// stopped there; more than one means every structural check passed and
/// every invalid requirement/matchspec string was collected instead of
/// just the first.
#[derive(Debug)]
pub struct Pep723Error {
    fields: Vec<InvalidField>,
}

impl Pep723Error {
    fn new(fields: Vec<InvalidField>) -> Self {
        debug_assert!(
            !fields.is_empty(),
            "Pep723Error must carry at least one invalid field"
        );
        Self { fields }
    }

    /// All invalid fields, in document order.
    pub fn fields(&self) -> &[InvalidField] {
        &self.fields
    }
}

impl From<InvalidField> for Pep723Error {
    /// Wraps a single structural-check failure.
    fn from(field: InvalidField) -> Self {
        Self::new(vec![field])
    }
}

impl Display for Pep723Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid PEP 723 script metadata:")?;
        for field in &self.fields {
            writeln!(f, "  {field}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Pep723Error {}

/// A single invalid field within a `# /// script` block: where it is,
/// plus optional detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidField {
    /// Dotted TOML path, with arrays addressed by index --
    /// `requires-python`, `dependencies[2]`,
    /// `tool.ana.matchspec-dependencies[0]`. The empty path means the
    /// block itself (a TOML syntax error, or a second `script` block).
    /// Intended for human consumption, not machine navigation.
    pub path: String,
    /// Optional detail: the offending value and why it was rejected.
    /// `None` means a bare "not valid".
    pub description: Option<String>,
}

impl InvalidField {
    fn new(path: &str, description: Option<String>) -> Self {
        Self {
            path: path.to_string(),
            description,
        }
    }
}

impl From<ana_dependency::tool_ana::InvalidToolAnaField> for InvalidField {
    fn from(err: ana_dependency::tool_ana::InvalidToolAnaField) -> Self {
        Self::new(&err.path, err.description)
    }
}

impl Display for InvalidField {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "script metadata"
        } else {
            self.path.as_str()
        };
        match &self.description {
            Some(description) => write!(f, "{path} not valid: {description}"),
            None => write!(f, "{path} not valid"),
        }
    }
}

/// `(original array index, raw string)` pairs for one array of literal
/// strings, before parsing -- the index lets a string that fails to
/// *parse* later still be blamed on its real position in the block.
type RawRequirements<'a> = Vec<(usize, &'a str)>;

/// `requires-python`. Missing entirely means `None` (no interpreter
/// constraint), not an error; present-but-not-a-string or
/// present-but-unparseable as a PEP 440 specifier set is.
fn extract_requires_python(
    doc: &Document<&str>,
) -> Result<Option<VersionSpecifiers>, InvalidField> {
    let Some(item) = doc.get("requires-python") else {
        return Ok(None);
    };
    let raw = item
        .as_str()
        .ok_or_else(|| InvalidField::new("requires-python", None))?;
    VersionSpecifiers::from_str(raw)
        .map(Some)
        .map_err(|err| InvalidField::new("requires-python", Some(err.to_string())))
}

/// `dependencies`. Missing entirely means zero dependencies, not an
/// error; present-but-wrong-shape is -- including a single non-string
/// entry. Returns `(original array index, raw PEP 508 string)` pairs.
fn extract_dependencies<'a>(
    doc: &'a Document<&'a str>,
) -> Result<RawRequirements<'a>, InvalidField> {
    let Some(item) = doc.get("dependencies") else {
        return Ok(Vec::new());
    };
    let Some(arr) = item.as_array() else {
        return Err(InvalidField::new("dependencies", None));
    };
    let mut raw = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| InvalidField::new(&format!("dependencies[{i}]"), None))?;
        raw.push((i, s));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn no_block_is_none() {
        assert_eq!(parse("print('hi')\n").unwrap(), None);
    }

    #[test]
    fn dependencies_and_requires_python() {
        let source = "\
# /// script
# requires-python = \">=3.11\"
# dependencies = [
#   \"requests<3\",
#   \"rich\",
# ]
# ///
print('hi')
";
        let script = parse(source).unwrap().unwrap();
        assert_eq!(script.dependencies.len(), 2);
        assert_eq!(
            ana_dependency::bare_name(&script.dependencies[0]),
            Some("requests".to_string())
        );
        assert_eq!(
            ana_dependency::bare_name(&script.dependencies[1]),
            Some("rich".to_string())
        );
        assert_eq!(script.requires_python.unwrap().to_string(), ">=3.11");
        assert_eq!(script.channels, None);
    }

    #[test]
    fn missing_dependencies_defaults_to_empty() {
        let source = "\
# /// script
# requires-python = \">=3.12\"
# ///
";
        let script = parse(source).unwrap().unwrap();
        assert_eq!(script.dependencies, vec![]);
    }

    #[test]
    fn ana_extension_channels_and_matchspec_dependencies() {
        let source = "\
# /// script
# dependencies = [\"requests\"]
#
# [tool.ana]
# conda-channels = [\"conda-forge\"]
# matchspec-dependencies = [\"numpy>=1.26\"]
# ///
";
        let script = parse(source).unwrap().unwrap();
        assert_eq!(script.channels, Some(vec!["conda-forge".to_string()]));
        // PEP 508 entries first, then matchspec entries -- same merge
        // order as `ana_pyproject`'s `[tool.ana]` extension.
        assert_eq!(script.dependencies.len(), 2);
        assert_eq!(
            ana_dependency::bare_name(&script.dependencies[0]),
            Some("requests".to_string())
        );
        assert_eq!(
            ana_dependency::bare_name(&script.dependencies[1]),
            Some("numpy".to_string())
        );
        assert!(matches!(script.dependencies[1], Dependency::Matchspec(_)));
    }

    #[test]
    fn an_invalid_pep508_dependency_names_its_index() {
        let source = "\
# /// script
# dependencies = [\"requests\", \"!!!not valid!!!\"]
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields().len(), 1);
        assert_eq!(err.fields()[0].path, "dependencies[1]");
    }

    #[test]
    fn an_invalid_matchspec_dependency_names_its_index() {
        let source = "\
# /// script
# dependencies = []
#
# [tool.ana]
# matchspec-dependencies = [\"!!!not valid!!!\"]
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields().len(), 1);
        assert_eq!(err.fields()[0].path, "tool.ana.matchspec-dependencies[0]");
    }

    #[test]
    fn every_invalid_dependency_is_collected_not_just_the_first() {
        let source = "\
# /// script
# dependencies = [\"!!!bad!!!\", \"also !!! bad\"]
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields().len(), 2);
    }

    #[test]
    fn an_invalid_requires_python_is_an_error() {
        let source = "\
# /// script
# requires-python = \"not a specifier\"
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields()[0].path, "requires-python");
    }

    #[test]
    fn an_empty_conda_channels_array_is_rejected() {
        let source = "\
# /// script
# dependencies = []
#
# [tool.ana]
# conda-channels = []
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields()[0].path, "tool.ana.conda-channels");
    }

    #[test]
    fn a_non_array_dependencies_field_is_rejected() {
        let source = "\
# /// script
# dependencies = \"requests\"
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields()[0].path, "dependencies");
    }

    #[test]
    fn malformed_toml_is_an_error() {
        let source = "\
# /// script
# this = is not [ valid toml
# ///
";
        assert!(parse(source).is_err());
    }

    #[test]
    fn multiple_script_blocks_is_an_error() {
        let source = "\
# /// script
# dependencies = []
# ///
# /// script
# dependencies = [\"requests\"]
# ///
";
        let err = parse(source).unwrap_err();
        assert_eq!(err.fields().len(), 1);
        assert_eq!(err.fields()[0].path, "");
    }

    #[test]
    fn an_unrelated_block_type_does_not_count_as_script_metadata() {
        let source = "\
# /// pyproject
# [tool.foo]
# ///
print('hi')
";
        assert_eq!(parse(source).unwrap(), None);
    }
}
