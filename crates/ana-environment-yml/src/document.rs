//! [`EnvironmentYml::parse`] turns `environment.yml` source text into a
//! typed [`EnvironmentYml`], collecting every requirement/matchspec
//! *parse* failure into one [`EnvironmentYmlError`] instead of stopping
//! at the first. See that function's docs for the full two-tier
//! contract.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use ana_dependency::MatchspecError;
use rattler_conda_types::MatchSpec;
use rayon::prelude::*;
use uv_pep508::{Pep508Error, Requirement};
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{ScanError, Yaml};

/// Below this many total requirement/matchspec strings in the document,
/// parse them sequentially instead of handing them to `rayon`.
const PARALLEL_PARSE_THRESHOLD: usize = 64;

/// A dependency declared in `environment.yml`: a conda `MatchSpec` (a
/// `dependencies` entry) or a PEP 508 requirement (a `dependencies[].pip`
/// entry).
pub use ana_dependency::Dependency;

/// The parts of an `environment.yml` that `ana` consumes.
///
/// `name` and `variables` (and any other unrecognized top-level key) are
/// read by conda but not by `ana`, so [`EnvironmentYml::parse`] ignores
/// them rather than rejecting them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvironmentYml {
    /// `channels:`, in file order. `None` when the key is absent,
    /// meaning no override.
    pub channels: Option<Vec<String>>,
    /// `dependencies:`, with each entry's own `pip:` sub-list expanded
    /// in place: a conda `MatchSpec` entry contributes one
    /// [`Dependency::Matchspec`] at its position, and a `{pip: [...]}`
    /// entry contributes one [`Dependency::Pep508`] per string in its
    /// list, in list order, all in place of that one entry. `dependencies`
    /// missing entirely means zero dependencies, not an error.
    pub dependencies: Vec<Dependency>,
}

impl EnvironmentYml {
    /// Parse `environment.yml` source text into an [`EnvironmentYml`].
    ///
    /// Only a deliberately small subset of YAML is understood, matching
    /// what a hand-written `environment.yml` actually uses:
    ///
    /// - The document must be a single top-level mapping (or absent/
    ///   empty/`null`, meaning no channels and no dependencies).
    /// - `channels`, if present, must be a non-empty sequence of
    ///   non-empty strings.
    /// - `dependencies`, if present, must be a sequence whose entries
    ///   are each either a plain string (a conda `MatchSpec`) or a
    ///   one-key mapping `{pip: [...]}` whose value is a sequence of
    ///   strings (PEP 508 requirements). Any other entry shape is
    ///   rejected.
    ///
    /// Structural checks return on the first problem found. Once the
    /// document's shape checks out, every literal `MatchSpec`/PEP 508
    /// string is parsed and *every* failure among them is collected
    /// into one [`EnvironmentYmlError`] instead of stopping at the
    /// first.
    pub fn parse(yaml: &str) -> Result<Self, EnvironmentYmlError> {
        let docs = load_yaml_rejecting_anchors(yaml).map_err(|err| {
            EnvironmentYmlError::from(InvalidField::new("", Some(err.to_string())))
        })?;

        // An empty file, or a document that is nothing but `~`/`null`,
        // means no channels and no dependencies -- not an error.
        let Some(doc) = docs.into_iter().next() else {
            return Ok(Self::default());
        };
        if doc.is_null() {
            return Ok(Self::default());
        }
        let top = doc.into_hash().ok_or_else(|| {
            EnvironmentYmlError::from(InvalidField::new(
                "",
                Some("environment.yml must be a YAML mapping".to_string()),
            ))
        })?;

        let channels = extract_channels(&top).map_err(EnvironmentYmlError::from)?;
        let entries = extract_dependency_entries(&top).map_err(EnvironmentYmlError::from)?;

        // Flatten into two lists, one per string kind, so all
        // matchspecs and all pip requirements are each parsed as one
        // batch.
        let matchspec_count = entries
            .iter()
            .filter(|entry| matches!(entry, DepEntry::Matchspec(..)))
            .count();
        let pip_count: usize = entries
            .iter()
            .map(|entry| match entry {
                DepEntry::Pip(_, raw) => raw.len(),
                DepEntry::Matchspec(..) => 0,
            })
            .sum();

        let mut flat_matchspec: Vec<&str> = Vec::with_capacity(matchspec_count);
        let mut flat_pip: Vec<&str> = Vec::with_capacity(pip_count);
        for entry in &entries {
            match entry {
                DepEntry::Matchspec(_, s) => flat_matchspec.push(s),
                DepEntry::Pip(_, raw) => flat_pip.extend(raw.iter().map(|&(_, s)| s)),
            }
        }

        let parsed_matchspec: Vec<Result<MatchSpec, MatchspecError>> =
            if flat_matchspec.len() >= PARALLEL_PARSE_THRESHOLD {
                flat_matchspec
                    .into_par_iter()
                    .map(ana_dependency::parse_matchspec)
                    .collect()
            } else {
                flat_matchspec
                    .into_iter()
                    .map(ana_dependency::parse_matchspec)
                    .collect()
            };
        let mut parsed_matchspec = parsed_matchspec.into_iter();

        let parsed_pip: Vec<Result<Requirement, Pep508Error>> =
            if flat_pip.len() >= PARALLEL_PARSE_THRESHOLD {
                flat_pip
                    .into_par_iter()
                    .map(Requirement::from_str)
                    .collect()
            } else {
                flat_pip.into_iter().map(Requirement::from_str).collect()
            };
        let mut parsed_pip = parsed_pip.into_iter();

        // Reassemble in file order: each `dependencies` entry expands
        // to one or more `Dependency`s in place, so a matchspec entry
        // and a `pip:` entry's requirements interleave exactly as they
        // appeared in the document.
        let mut errors: Vec<InvalidField> = Vec::new();
        let mut dependencies = Vec::with_capacity(matchspec_count + pip_count);
        for entry in entries {
            match entry {
                DepEntry::Matchspec(i, _) => {
                    match next_parsed_matchspec(&mut parsed_matchspec, || {
                        format!("dependencies[{i}]")
                    }) {
                        Ok(spec) => dependencies.push(Dependency::Matchspec(Box::new(spec))),
                        Err(err) => errors.push(err),
                    }
                }
                DepEntry::Pip(i, raw) => {
                    for (j, _) in raw {
                        match next_parsed_pep508(&mut parsed_pip, || {
                            format!("dependencies[{i}].pip[{j}]")
                        }) {
                            Ok(req) => dependencies.push(Dependency::Pep508(req)),
                            Err(err) => errors.push(err),
                        }
                    }
                }
            }
        }

        if !errors.is_empty() {
            return Err(EnvironmentYmlError::new(errors));
        }

        Ok(EnvironmentYml {
            channels,
            dependencies,
        })
    }
}

/// Parses `yaml` into a `Yaml` tree like
/// `yaml_rust2::YamlLoader::load_from_str`, except any YAML anchor
/// definition (`&name`) or alias reference (`*name`) is a hard error
/// instead of being resolved.
///
/// `yaml_rust2`'s alias resolution clones an anchor's node on every
/// reference; nested anchors let a document a few hundred bytes long
/// expand to unbounded memory well before
/// [`crate::EnvironmentYml::parse`]'s caller-side file-size cap would
/// stop it. Rejecting anchors/aliases at the event level means no such
/// tree is ever built.
fn load_yaml_rejecting_anchors(yaml: &str) -> Result<Vec<Yaml>, ScanError> {
    let mut parser = Parser::new(yaml.chars());
    let mut receiver = NoAliasLoader::default();
    parser.load(&mut receiver, true)?;
    match receiver.error {
        Some(err) => Err(err),
        None => Ok(receiver.docs),
    }
}

/// Event receiver for [`load_yaml_rejecting_anchors`]: builds the same
/// `Yaml` tree `yaml_rust2::YamlLoader` would, but treats any anchor
/// definition or alias reference as a parse error.
#[derive(Default)]
struct NoAliasLoader {
    docs: Vec<Yaml>,
    doc_stack: Vec<Yaml>,
    key_stack: Vec<Yaml>,
    error: Option<ScanError>,
}

impl MarkedEventReceiver for NoAliasLoader {
    fn on_event(&mut self, ev: Event, mark: Marker) {
        if self.error.is_some() {
            return;
        }
        if let Err(err) = self.on_event_impl(ev, mark) {
            self.error = Some(err);
        }
    }
}

impl NoAliasLoader {
    fn on_event_impl(&mut self, ev: Event, mark: Marker) -> Result<(), ScanError> {
        match ev {
            Event::DocumentStart | Event::Nothing | Event::StreamStart | Event::StreamEnd => {}
            Event::DocumentEnd => match self.doc_stack.len() {
                0 => self.docs.push(Yaml::BadValue),
                1 => {
                    let doc = self.pop_doc_stack(mark)?;
                    self.docs.push(doc);
                }
                _ => return Err(internal_error(mark, "unbalanced document stack")),
            },
            Event::SequenceStart(anchor_id, _) => {
                reject_anchor(anchor_id, mark)?;
                self.doc_stack.push(Yaml::Array(Vec::new()));
            }
            Event::SequenceEnd => {
                let node = self.pop_doc_stack(mark)?;
                self.insert_new_node(node, mark)?;
            }
            Event::MappingStart(anchor_id, _) => {
                reject_anchor(anchor_id, mark)?;
                self.doc_stack.push(Yaml::Hash(Hash::new()));
                self.key_stack.push(Yaml::BadValue);
            }
            Event::MappingEnd => {
                if self.key_stack.pop().is_none() {
                    return Err(internal_error(mark, "empty key stack"));
                }
                let node = self.pop_doc_stack(mark)?;
                self.insert_new_node(node, mark)?;
            }
            Event::Scalar(v, style, anchor_id, _tag) => {
                reject_anchor(anchor_id, mark)?;
                // Explicit YAML type tags (`!!bool`, `!!int`, ...) are
                // not part of the plain-YAML subset this crate
                // supports, so every scalar is typed the same way
                // regardless of any tag: a plain (unquoted) scalar is
                // type-inferred, anything quoted stays a string.
                let node = if style == TScalarStyle::Plain {
                    Yaml::from_str(&v)
                } else {
                    Yaml::String(v)
                };
                self.insert_new_node(node, mark)?;
            }
            Event::Alias(_) => {
                return Err(ScanError::new_string(
                    mark,
                    "YAML aliases are not supported in environment.yml".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Pops the top of `doc_stack`, the node a just-closed
    /// sequence/mapping/document built.
    fn pop_doc_stack(&mut self, mark: Marker) -> Result<Yaml, ScanError> {
        self.doc_stack
            .pop()
            .ok_or_else(|| internal_error(mark, "empty document stack"))
    }

    /// Places a fully-built node into its parent container, or as the
    /// document root if `doc_stack` is now empty.
    fn insert_new_node(&mut self, node: Yaml, mark: Marker) -> Result<(), ScanError> {
        let Some(parent) = self.doc_stack.last_mut() else {
            self.doc_stack.push(node);
            return Ok(());
        };
        match parent {
            Yaml::Array(v) => v.push(node),
            Yaml::Hash(h) => {
                let Some(cur_key) = self.key_stack.last_mut() else {
                    return Err(internal_error(mark, "empty key stack"));
                };
                if cur_key.is_badvalue() {
                    *cur_key = node;
                } else {
                    let mut key = Yaml::BadValue;
                    std::mem::swap(&mut key, cur_key);
                    if h.insert(key, node).is_some() {
                        return Err(ScanError::new_string(
                            mark,
                            "duplicated key in mapping".to_string(),
                        ));
                    }
                }
            }
            _ => return Err(internal_error(mark, "invalid parser state")),
        }
        Ok(())
    }
}

/// Errors out if `anchor_id` marks an anchor definition (`&name`) --
/// nonzero means the node this event opens/is was written as `&name
/// ...`. `environment.yml` never needs one.
fn reject_anchor(anchor_id: usize, mark: Marker) -> Result<(), ScanError> {
    if anchor_id != 0 {
        return Err(ScanError::new_string(
            mark,
            "YAML anchors are not supported in environment.yml".to_string(),
        ));
    }
    Ok(())
}

/// An event sequence [`NoAliasLoader`] didn't expect from a well-formed
/// parser -- never actually reachable, but reported as a parse error
/// rather than panicking, since this crate never `unwrap`/`expect`s
/// outside tests.
fn internal_error(mark: Marker, message: &str) -> ScanError {
    ScanError::new_string(mark, format!("internal error: {message}"))
}

/// One classified `dependencies` array entry, before its literal
/// string(s) have been parsed.
enum DepEntry<'a> {
    /// A plain string entry: `(original array index, raw matchspec
    /// string)`.
    Matchspec(usize, &'a str),
    /// A `{pip: [...]}` entry: `(original array index, [(original
    /// pip-array index, raw PEP 508 string), ...])`.
    Pip(usize, Vec<(usize, &'a str)>),
}

/// Pull the next parsed result off the matchspec flat cursor, converting
/// it into either a `MatchSpec` or an [`InvalidField`] at `path()`.
fn next_parsed_matchspec(
    parsed: &mut std::vec::IntoIter<Result<MatchSpec, MatchspecError>>,
    path: impl FnOnce() -> String,
) -> Result<MatchSpec, InvalidField> {
    match parsed.next() {
        Some(Ok(spec)) => Ok(spec),
        Some(Err(err)) => Err(InvalidField::new(&path(), Some(err.to_string()))),
        None => Err(InvalidField::new(
            &path(),
            Some("internal error: ran out of parsed matchspecs".to_string()),
        )),
    }
}

/// Pull the next parsed result off the PEP 508 flat cursor, converting
/// it into either a `Requirement` or an [`InvalidField`] at `path()`.
fn next_parsed_pep508(
    parsed: &mut std::vec::IntoIter<Result<Requirement, Pep508Error>>,
    path: impl FnOnce() -> String,
) -> Result<Requirement, InvalidField> {
    match parsed.next() {
        Some(Ok(req)) => Ok(req),
        Some(Err(err)) => Err(InvalidField::new(&path(), Some(err.to_string()))),
        None => Err(InvalidField::new(
            &path(),
            Some("internal error: ran out of parsed requirements".to_string()),
        )),
    }
}

/// Looks up `key` in a YAML mapping already known to be a [`Hash`].
fn get<'a>(top: &'a Hash, key: &str) -> Option<&'a Yaml> {
    top.get(&Yaml::String(key.to_string()))
}

/// `channels:`. Missing entirely means `None` (no channel override);
/// present must be a non-empty sequence of non-empty strings -- an
/// empty list is rejected rather than accepted as an override to zero
/// channels.
fn extract_channels(top: &Hash) -> Result<Option<Vec<String>>, InvalidField> {
    let Some(item) = get(top, "channels") else {
        return Ok(None);
    };
    let Some(arr) = item.as_vec() else {
        return Err(InvalidField::new("channels", None));
    };
    if arr.is_empty() {
        return Err(InvalidField::new(
            "channels",
            Some("must not be empty".to_string()),
        ));
    }
    let mut channels = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| InvalidField::new(&format!("channels[{i}]"), None))?;
        if s.is_empty() {
            return Err(InvalidField::new(
                &format!("channels[{i}]"),
                Some("must not be empty".to_string()),
            ));
        }
        channels.push(s.to_string());
    }
    Ok(Some(channels))
}

/// `dependencies:`. Missing entirely means zero dependencies, not an
/// error; present-but-wrong-shape is -- including a single entry that is
/// neither a string nor a `{pip: [...]}` mapping, which stops the walk
/// right there rather than collecting every bad entry (only the
/// requirement/matchspec-*parsing* tier aggregates; see
/// [`EnvironmentYml::parse`]'s docs).
fn extract_dependency_entries(top: &Hash) -> Result<Vec<DepEntry<'_>>, InvalidField> {
    let Some(item) = get(top, "dependencies") else {
        return Ok(Vec::new());
    };
    let Some(arr) = item.as_vec() else {
        return Err(InvalidField::new("dependencies", None));
    };
    let mut entries = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        match v {
            Yaml::String(s) => entries.push(DepEntry::Matchspec(i, s.as_str())),
            Yaml::Hash(h) => entries.push(extract_pip_entry(h, i)?),
            _ => return Err(InvalidField::new(&format!("dependencies[{i}]"), None)),
        }
    }
    Ok(entries)
}

/// One `dependencies` entry already known to be a YAML mapping: must be
/// exactly `{pip: [<string>, ...]}`. `i` is the entry's own index in
/// `dependencies`, for error paths.
fn extract_pip_entry(h: &Hash, i: usize) -> Result<DepEntry<'_>, InvalidField> {
    if h.len() != 1 {
        return Err(InvalidField::new(
            &format!("dependencies[{i}]"),
            Some("a mapping entry in `dependencies` must have exactly one key, `pip`".to_string()),
        ));
    }
    // `h.len() == 1` was just checked, so this always yields exactly
    // one pair.
    let Some((key, value)) = h.iter().next() else {
        return Err(InvalidField::new(&format!("dependencies[{i}]"), None));
    };
    let Some(key_str) = key.as_str() else {
        return Err(InvalidField::new(&format!("dependencies[{i}]"), None));
    };
    if key_str != "pip" {
        return Err(InvalidField::new(
            &format!("dependencies[{i}]"),
            Some(format!(
                "unsupported key `{key_str}` (only `pip` is supported)"
            )),
        ));
    }
    let Some(pip_arr) = value.as_vec() else {
        return Err(InvalidField::new(&format!("dependencies[{i}].pip"), None));
    };
    let mut raw = Vec::with_capacity(pip_arr.len());
    for (j, pv) in pip_arr.iter().enumerate() {
        let s = pv
            .as_str()
            .ok_or_else(|| InvalidField::new(&format!("dependencies[{i}].pip[{j}]"), None))?;
        raw.push((j, s));
    }
    Ok(DepEntry::Pip(i, raw))
}

/// Every invalid field found in one `environment.yml`. Never constructed
/// with an empty field list -- if nothing is invalid, parsing succeeds.
///
/// **One field** means a structural check failed (the document itself,
/// a wrong-shaped `channels`/`dependencies`, or a `dependencies` entry
/// that is neither a string nor a `{pip: [...]}` mapping) --
/// [`EnvironmentYml::parse`] returns on the first one found, so there is
/// never more than one. **One or more fields**, every one a
/// requirement/matchspec *parse* failure, means every structural check
/// already passed and every invalid string is collected rather than
/// just the first.
#[derive(Debug)]
pub struct EnvironmentYmlError {
    fields: Vec<InvalidField>,
}

impl EnvironmentYmlError {
    fn new(fields: Vec<InvalidField>) -> Self {
        debug_assert!(
            !fields.is_empty(),
            "EnvironmentYmlError must carry at least one invalid field"
        );
        Self { fields }
    }

    /// All invalid fields, in document order.
    pub fn fields(&self) -> &[InvalidField] {
        &self.fields
    }
}

impl From<InvalidField> for EnvironmentYmlError {
    /// Wraps a single structural-check failure.
    fn from(field: InvalidField) -> Self {
        Self::new(vec![field])
    }
}

impl Display for EnvironmentYmlError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid environment.yml:")?;
        for field in &self.fields {
            writeln!(f, "  {field}")?;
        }
        Ok(())
    }
}

impl std::error::Error for EnvironmentYmlError {}

/// A single invalid field: where it is, plus optional detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidField {
    /// Dotted path, with arrays addressed by index --
    /// `channels[0]`, `dependencies[2]`, `dependencies[1].pip[0]`. The
    /// empty path means the document itself (a YAML syntax error, or a
    /// non-mapping top level). Intended for human consumption, not
    /// machine navigation.
    pub path: String,
    /// Optional detail: the offending value and why it was rejected
    /// (YAML scanner message, PEP 508/matchspec parser message).
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

impl Display for InvalidField {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "document"
        } else {
            self.path.as_str()
        };
        match &self.description {
            Some(description) => write!(f, "{path} not valid: {description}"),
            None => write!(f, "{path} not valid"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! End-to-end tests for [`EnvironmentYml::parse`]: YAML text in, a
    //! typed [`EnvironmentYml`] (or an aggregated field-error list) out.

    use super::*;

    fn matchspec_dep(spec: &str) -> Dependency {
        Dependency::Matchspec(Box::new(ana_dependency::parse_matchspec(spec).unwrap()))
    }

    fn pep508_dep(spec: &str) -> Dependency {
        Dependency::Pep508(Requirement::from_str(spec).unwrap())
    }

    fn parse_ok(yaml: &str) -> EnvironmentYml {
        EnvironmentYml::parse(yaml).unwrap()
    }

    fn parse_err(yaml: &str) -> Vec<InvalidField> {
        EnvironmentYml::parse(yaml).unwrap_err().fields().to_vec()
    }

    fn paths(fields: &[InvalidField]) -> Vec<&str> {
        fields.iter().map(|f| f.path.as_str()).collect()
    }

    fn invalid(path: &str) -> InvalidField {
        InvalidField {
            path: path.to_string(),
            description: None,
        }
    }

    #[test]
    fn empty_document_has_no_channels_and_no_dependencies() {
        assert_eq!(parse_ok(""), EnvironmentYml::default());
        assert_eq!(parse_ok("   \n"), EnvironmentYml::default());
    }

    #[test]
    fn explicit_null_document_is_also_empty() {
        assert_eq!(parse_ok("null\n"), EnvironmentYml::default());
        assert_eq!(parse_ok("~\n"), EnvironmentYml::default());
    }

    #[test]
    fn a_non_mapping_document_is_rejected() {
        assert_eq!(paths(&parse_err("- 1\n- 2\n")), vec![""]);
        assert_eq!(paths(&parse_err("just a string\n")), vec![""]);
    }

    #[test]
    fn invalid_yaml_syntax_is_rejected() {
        let fields = parse_err("channels: [conda-forge\n");
        assert_eq!(paths(&fields), vec![""]);
    }

    #[test]
    fn name_and_variables_are_ignored() {
        let env = parse_ok(
            "name: myproj\nchannels:\n  - conda-forge\nvariables:\n  FOO: bar\ndependencies:\n  - numpy\n",
        );
        assert_eq!(env.channels, Some(vec!["conda-forge".to_string()]));
        assert_eq!(env.dependencies, vec![matchspec_dep("numpy")]);
    }

    #[test]
    fn unrecognized_top_level_keys_are_ignored() {
        let env = parse_ok("prefix: /opt/conda/envs/myproj\ndependencies:\n  - numpy\n");
        assert_eq!(env.dependencies, vec![matchspec_dep("numpy")]);
    }

    #[test]
    fn missing_channels_is_none() {
        let env = parse_ok("dependencies:\n  - numpy\n");
        assert_eq!(env.channels, None);
    }

    #[test]
    fn channels_are_parsed_in_file_order() {
        let env = parse_ok("channels:\n  - conda-forge\n  - bioconda\n");
        assert_eq!(
            env.channels,
            Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
        );
    }

    #[test]
    fn empty_channels_list_is_rejected() {
        let fields = parse_err("channels: []\n");
        assert_eq!(paths(&fields), vec!["channels"]);
        assert_eq!(fields[0].description, Some("must not be empty".to_string()));
    }

    #[test]
    fn channels_not_a_sequence_is_rejected() {
        assert_eq!(
            paths(&parse_err("channels: conda-forge\n")),
            vec!["channels"]
        );
    }

    #[test]
    fn a_non_string_channel_names_its_index() {
        assert_eq!(
            paths(&parse_err("channels:\n  - conda-forge\n  - 123\n")),
            vec!["channels[1]"]
        );
    }

    #[test]
    fn an_empty_channel_string_is_rejected() {
        let fields = parse_err("channels:\n  - \"\"\n");
        assert_eq!(paths(&fields), vec!["channels[0]"]);
        assert_eq!(fields[0].description, Some("must not be empty".to_string()));
    }

    #[test]
    fn missing_dependencies_key_is_empty() {
        let env = parse_ok("channels:\n  - conda-forge\n");
        assert!(env.dependencies.is_empty());
    }

    #[test]
    fn empty_dependencies_list_is_empty() {
        let env = parse_ok("dependencies: []\n");
        assert!(env.dependencies.is_empty());
    }

    #[test]
    fn dependencies_not_a_sequence_is_rejected() {
        assert_eq!(
            paths(&parse_err("dependencies: numpy\n")),
            vec!["dependencies"]
        );
    }

    #[test]
    fn plain_matchspec_entries_are_parsed_in_order() {
        let env = parse_ok("dependencies:\n  - python=3.10\n  - numpy>=1.26\n");
        assert_eq!(
            env.dependencies,
            vec![matchspec_dep("python=3.10"), matchspec_dep("numpy>=1.26")]
        );
    }

    #[test]
    fn a_channelled_matchspec_entry_is_accepted() {
        let env = parse_ok("dependencies:\n  - conda-forge::numpy\n");
        let Dependency::Matchspec(spec) = &env.dependencies[0] else {
            panic!("expected a matchspec dependency");
        };
        assert!(spec.channel.is_some());
    }

    #[test]
    fn pip_subkey_entries_become_pep508_dependencies() {
        let env =
            parse_ok("dependencies:\n  - python\n  - pip:\n      - requests\n      - click>=8\n");
        assert_eq!(
            env.dependencies,
            vec![
                matchspec_dep("python"),
                pep508_dep("requests"),
                pep508_dep("click>=8"),
            ]
        );
    }

    #[test]
    fn pip_entries_interleave_at_their_own_position() {
        let env = parse_ok("dependencies:\n  - numpy\n  - pip:\n      - requests\n  - scipy\n");
        assert_eq!(
            env.dependencies,
            vec![
                matchspec_dep("numpy"),
                pep508_dep("requests"),
                matchspec_dep("scipy"),
            ]
        );
    }

    #[test]
    fn empty_pip_list_contributes_nothing() {
        let env = parse_ok("dependencies:\n  - numpy\n  - pip: []\n  - scipy\n");
        assert_eq!(
            env.dependencies,
            vec![matchspec_dep("numpy"), matchspec_dep("scipy")]
        );
    }

    #[test]
    fn pip_key_with_a_sibling_key_is_rejected() {
        let fields = parse_err("dependencies:\n  - pip:\n      - requests\n    extra: true\n");
        assert_eq!(paths(&fields), vec!["dependencies[0]"]);
    }

    #[test]
    fn a_mapping_entry_with_a_key_other_than_pip_is_rejected() {
        let fields = parse_err("dependencies:\n  - conda:\n      - numpy\n");
        assert_eq!(paths(&fields), vec!["dependencies[0]"]);
        assert!(fields[0]
            .description
            .as_ref()
            .unwrap()
            .contains("unsupported key"));
    }

    #[test]
    fn pip_value_not_a_sequence_is_rejected() {
        assert_eq!(
            paths(&parse_err("dependencies:\n  - pip: requests\n")),
            vec!["dependencies[0].pip"]
        );
    }

    #[test]
    fn a_non_string_pip_entry_names_its_index() {
        assert_eq!(
            paths(&parse_err(
                "dependencies:\n  - pip:\n      - requests\n      - 123\n"
            )),
            vec!["dependencies[0].pip[1]"]
        );
    }

    #[test]
    fn a_yaml_typed_scalar_dependency_entry_is_rejected() {
        // `true`/`7`/`null` never collide with a real matchspec or PEP
        // 508 string, so a bare, unquoted one is a real shape error,
        // not something to coerce.
        assert_eq!(
            paths(&parse_err("dependencies:\n  - true\n")),
            vec!["dependencies[0]"]
        );
        assert_eq!(
            paths(&parse_err("dependencies:\n  - 7\n")),
            vec!["dependencies[0]"]
        );
        assert_eq!(
            paths(&parse_err("dependencies:\n  - null\n")),
            vec!["dependencies[0]"]
        );
    }

    #[test]
    fn a_nested_sequence_dependency_entry_is_rejected() {
        assert_eq!(
            paths(&parse_err("dependencies:\n  - [numpy, scipy]\n")),
            vec!["dependencies[0]"]
        );
    }

    #[test]
    fn invalid_matchspec_syntax_is_reported() {
        let fields = parse_err("dependencies:\n  - \"this is [ not valid\"\n");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependencies[0]");
    }

    #[test]
    fn invalid_pep508_syntax_is_reported() {
        let fields =
            parse_err("dependencies:\n  - pip:\n      - \"not a valid==requirement==string\"\n");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependencies[0].pip[0]");
    }

    #[test]
    fn every_bad_entry_is_collected_not_just_the_first() {
        let fields = parse_err(
            "dependencies:\n  - \"this is [ not valid\"\n  - numpy\n  - pip:\n      - \"not a valid==requirement==string\"\n",
        );
        assert_eq!(
            paths(&fields),
            vec!["dependencies[0]", "dependencies[2].pip[0]"]
        );
    }

    #[test]
    fn duplicate_top_level_keys_are_rejected_by_the_yaml_scanner() {
        let fields = parse_err("channels:\n  - conda-forge\nchannels:\n  - bioconda\n");
        assert_eq!(paths(&fields), vec![""]);
    }

    #[test]
    fn an_anchor_definition_is_rejected() {
        let fields = parse_err("shared: &shared\n  - numpy\n  - scipy\ndependencies: *shared\n");
        assert_eq!(paths(&fields), vec![""]);
    }

    #[test]
    fn an_alias_reference_is_rejected() {
        let fields = parse_err("dependencies:\n  - &a numpy\n  - *a\n");
        assert_eq!(paths(&fields), vec![""]);
    }

    #[test]
    fn a_nested_anchor_expansion_bomb_is_rejected_not_expanded() {
        // Six levels of ten-wide anchor chains: if aliases were resolved
        // instead of rejected, this ~300-byte document would expand to
        // roughly 10^6 leaf scalars.
        let bomb = "a: &a [x,x,x,x,x,x,x,x,x,x]\n\
                    b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a,*a]\n\
                    c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b,*b]\n\
                    d: &d [*c,*c,*c,*c,*c,*c,*c,*c,*c,*c]\n\
                    e: &e [*d,*d,*d,*d,*d,*d,*d,*d,*d,*d]\n\
                    dependencies: *e\n";
        let fields = parse_err(bomb);
        assert_eq!(paths(&fields), vec![""]);
    }

    #[test]
    fn full_document_matches_a_realistic_environment_yml() {
        let env = parse_ok(
            r#"
name: myproj
channels:
  - conda-forge
  - defaults
dependencies:
  - python=3.11
  - numpy>=1.26
  - pip:
      - requests
      - "click>=8"
variables:
  FOO: bar
"#,
        );
        assert_eq!(
            env.channels,
            Some(vec!["conda-forge".to_string(), "defaults".to_string()])
        );
        assert_eq!(
            env.dependencies,
            vec![
                matchspec_dep("python=3.11"),
                matchspec_dep("numpy>=1.26"),
                pep508_dep("requests"),
                pep508_dep("click>=8"),
            ]
        );
    }

    #[test]
    fn error_display_lists_every_field() {
        let err = EnvironmentYml::parse("dependencies: numpy\n").unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("dependencies not valid"));
    }

    #[test]
    fn invalid_field_at_the_empty_path_is_the_document_itself() {
        assert_eq!(invalid("").to_string(), "document not valid");
    }
}
