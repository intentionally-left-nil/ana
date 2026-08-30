//! The `requirements.txt` front end: [`RequirementsTxt::parse`] turns
//! already-joined logical lines (from [`crate::lines`]) into typed
//! [`Dependency`]s, rejecting every pip requirements-file feature this
//! crate does not support (see the crate docs).
//!
//! Every logical line is independent of every other, so a bad line
//! doesn't invalidate anything around it: [`RequirementsTxt::parse`]
//! keeps going after a bad line and collects every [`LineError`] into
//! one [`RequirementsTxtError`], rather than stopping at the first.
//!
//! ## The `# ana-channels: <list>` directive
//!
//! [`crate::lines::logical_lines`] recognizes the line shape; this
//! module owns what the directive actually *means*: a comma-separated
//! list of channel names/URLs, trimmed entry by entry, becoming
//! [`RequirementsTxt::channels`]. Unlike `# ana-matchspec:`, which may
//! appear once per dependency, this is file-level state -- there is no
//! good answer for "which one wins" if it appears twice, so a second
//! occurrence is rejected outright ([`LineErrorKind::DuplicateChannelsDirective`])
//! rather than the first/last silently taking precedence.

use std::borrow::Cow;
use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use rayon::prelude::*;
use uv_pep508::{Requirement, VersionOrUrl};

use crate::lines::{logical_lines, LogicalLine};

/// Below this many logical lines, parse sequentially instead of handing
/// them to `rayon`; above it, entering `rayon`'s parallel region pays
/// off its fixed setup cost.
const PARALLEL_PARSE_THRESHOLD: usize = 64;

/// A `requirements.txt`, parsed: every accepted requirement/matchspec
/// line it contains, in file order, plus any file-level `# ana-channels:`
/// override.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequirementsTxt {
    /// One entry per accepted line, in the order they appear in the file.
    pub requirements: Vec<RequirementEntry>,
    /// The file's `# ana-channels: <list>` directive, split on `,` and
    /// trimmed entry by entry. `None` when the directive is absent --
    /// see the module docs for why at most one occurrence is accepted.
    pub channels: Option<Vec<String>>,
}

/// One accepted line: its parsed [`Dependency`], plus the physical line
/// it started on for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementEntry {
    /// The parsed dependency.
    pub dependency: Dependency,
    /// The 1-indexed physical line this entry started on. For a
    /// backslash-continued requirement, this is the first physical
    /// line, not any continuation line that followed it.
    pub line: usize,
}

/// A single requirement/matchspec line's parsed form: a PEP 508
/// requirement, or a conda `MatchSpec` declared via an
/// `# ana-matchspec:` directive comment.
pub use ana_dependency::Dependency;

impl RequirementsTxt {
    /// Parses `requirements.txt` source text into a [`RequirementsTxt`].
    ///
    /// Performs no I/O and does not follow `-r`/`-c` includes; a line
    /// using one of those (or `-e`, `--hash`, a direct URL, or any other
    /// unsupported pip requirements-file feature) is reported as a
    /// [`LineError`] rather than acted on. Every invalid or unsupported
    /// line is collected into one [`RequirementsTxtError`] (see the
    /// module docs).
    pub fn parse(text: &str) -> Result<Self, RequirementsTxtError> {
        // `# ana-channels:` lines are file-level state, not a dependency
        // line, so they're split out before the dependency parsing pass
        // below rather than threaded through `parse_logical_line`.
        let mut dep_lines = Vec::new();
        let mut channels_lines: Vec<(usize, Cow<'_, str>)> = Vec::new();
        for line in logical_lines(text) {
            match line {
                LogicalLine::Channels { line, text } => channels_lines.push((line, text)),
                LogicalLine::Requirement { line, text } => {
                    dep_lines.push(DepLine::Requirement { line, text });
                }
                LogicalLine::Matchspec { line, text } => {
                    dep_lines.push(DepLine::Matchspec { line, text });
                }
            }
        }

        let outcomes: Vec<(usize, Result<Dependency, LineErrorKind>)> =
            if dep_lines.len() >= PARALLEL_PARSE_THRESHOLD {
                dep_lines.into_par_iter().map(parse_logical_line).collect()
            } else {
                dep_lines.into_iter().map(parse_logical_line).collect()
            };

        let mut requirements = Vec::with_capacity(outcomes.len());
        let mut errors = Vec::new();
        for (line, outcome) in outcomes {
            match outcome {
                Ok(dependency) => requirements.push(RequirementEntry { dependency, line }),
                Err(kind) => errors.push(LineError { line, kind }),
            }
        }

        // Only the first `# ana-channels:` occurrence is ever parsed as
        // the file's channel list; every further occurrence is rejected
        // at its own line -- see the module docs.
        let mut channels = None;
        for (i, (line, text)) in channels_lines.iter().enumerate() {
            if i == 0 {
                match parse_channels_directive(text) {
                    Ok(list) => channels = Some(list),
                    Err(kind) => errors.push(LineError { line: *line, kind }),
                }
            } else {
                errors.push(LineError {
                    line: *line,
                    kind: LineErrorKind::DuplicateChannelsDirective,
                });
            }
        }

        if !errors.is_empty() {
            // `dep_lines`/`channels_lines` were parsed as two separate
            // passes, so their errors arrive in two separate runs rather
            // than one file-order pass; re-sort by line so the reported
            // order matches the file regardless of which pass found what.
            errors.sort_by_key(|error| error.line);
            return Err(RequirementsTxtError::new(errors));
        }

        Ok(RequirementsTxt {
            requirements,
            channels,
        })
    }
}

/// A logical line that is (or claims to be) a dependency declaration --
/// [`LogicalLine`] minus its `Channels` variant, which is file-level
/// state handled separately in [`RequirementsTxt::parse`] and never
/// reaches [`parse_logical_line`].
enum DepLine<'a> {
    Requirement { line: usize, text: Cow<'a, str> },
    Matchspec { line: usize, text: Cow<'a, str> },
}

/// Parses one already-classified logical line into its line number and
/// parsed-or-rejected outcome. A plain function, rather than a closure,
/// so the same expression can be handed to both the sequential and
/// `rayon` parallel branch in [`RequirementsTxt::parse`].
fn parse_logical_line(logical: DepLine<'_>) -> (usize, Result<Dependency, LineErrorKind>) {
    match logical {
        DepLine::Requirement { line, text } => (line, parse_pep508_line(&text)),
        DepLine::Matchspec { line, text } => (line, parse_matchspec_line(&text)),
    }
}

/// Classifies and parses one ordinary (non-`# ana-matchspec:`) logical
/// line. Directives and direct URL/VCS/local-path requirements are
/// rejected before reaching [`Requirement::from_str`], so the error
/// names the actual reason rather than a generic PEP 508 syntax
/// complaint.
fn parse_pep508_line(text: &str) -> Result<Dependency, LineErrorKind> {
    if let Some(directive) = unsupported_directive(text) {
        return Err(LineErrorKind::UnsupportedDirective(directive));
    }

    match Requirement::from_str(text) {
        Ok(requirement) => match &requirement.version_or_url {
            // `name @ url` parses as valid PEP 508 but has no matchspec
            // equivalent.
            Some(VersionOrUrl::Url(_)) => Err(LineErrorKind::DirectUrlOrPath),
            _ => Ok(Dependency::Pep508(requirement)),
        },
        Err(err) => {
            if looks_like_url_or_path(text) {
                Err(LineErrorKind::DirectUrlOrPath)
            } else {
                Err(LineErrorKind::InvalidRequirement(err.to_string()))
            }
        }
    }
}

/// Parses one `# ana-matchspec: <spec>` directive's already-trimmed
/// spec text into a [`Dependency::Matchspec`].
fn parse_matchspec_line(text: &str) -> Result<Dependency, LineErrorKind> {
    if text.is_empty() {
        return Err(LineErrorKind::EmptyMatchspecDirective);
    }
    match ana_dependency::parse_matchspec(text) {
        Ok(spec) => Ok(Dependency::Matchspec(Box::new(spec))),
        Err(err) => Err(LineErrorKind::InvalidMatchspec(err.to_string())),
    }
}

/// Parses one `# ana-channels: <list>` directive's already-trimmed
/// value into a channel list: split on `,`, each entry trimmed. Both
/// "nothing after the colon" and "an individual comma-separated entry
/// is blank" report the same [`LineErrorKind::EmptyChannelsDirective`]
/// -- neither has a sensible list to return.
fn parse_channels_directive(text: &str) -> Result<Vec<String>, LineErrorKind> {
    if text.trim().is_empty() {
        return Err(LineErrorKind::EmptyChannelsDirective);
    }
    let mut channels = Vec::new();
    for entry in text.split(',') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(LineErrorKind::EmptyChannelsDirective);
        }
        channels.push(trimmed.to_string());
    }
    Ok(channels)
}

/// If `text` is a pip requirements-file directive this crate does not
/// support, returns the token that names it (for the error message).
/// Matches either a whole line starting with `-`/`--` (`-r`, `-c`, `-e`,
/// `-i`, `--no-index`, etc.), or a `--hash`/`--hash=...` token anywhere
/// in the line -- hash pins are conventionally attached via a
/// backslash-continued line, so they don't necessarily start with `-`
/// once [`crate::lines::logical_lines`] has joined the line.
fn unsupported_directive(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
        if token == "--hash" || token.starts_with("--hash=") {
            return Some("--hash".to_string());
        }
    }
    if text.starts_with('-') {
        return Some(text.split_whitespace().next().unwrap_or(text).to_string());
    }
    None
}

/// Heuristically recognizes a bare URL, VCS reference, or local path --
/// pip requirements-file forms that don't start with a PEP 508 name.
/// Only called once [`Requirement::from_str`] has already failed.
fn looks_like_url_or_path(text: &str) -> bool {
    const VCS_PREFIXES: [&str; 4] = ["git+", "hg+", "svn+", "bzr+"];

    let Some(first_token) = text.split_whitespace().next() else {
        return false;
    };
    let lower = first_token.to_ascii_lowercase();

    VCS_PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
        || first_token.contains("://")
        || first_token.starts_with("./")
        || first_token.starts_with("../")
        || first_token.starts_with('/')
        || first_token.starts_with('~')
}

/// Every invalid or unsupported line found in one `requirements.txt`.
/// Never constructed with an empty line list -- if nothing is invalid,
/// parsing succeeds.
#[derive(Debug)]
pub struct RequirementsTxtError {
    errors: Vec<LineError>,
}

impl RequirementsTxtError {
    fn new(errors: Vec<LineError>) -> Self {
        debug_assert!(
            !errors.is_empty(),
            "RequirementsTxtError must carry at least one line error"
        );
        Self { errors }
    }

    /// Every invalid or unsupported line, in file order.
    pub fn errors(&self) -> &[LineError] {
        &self.errors
    }
}

impl Display for RequirementsTxtError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid requirements.txt:")?;
        for error in &self.errors {
            writeln!(f, "  {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RequirementsTxtError {}

/// A single invalid or unsupported line: which physical line it started
/// on, plus why it was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineError {
    /// The 1-indexed physical line this logical line started on -- see
    /// [`RequirementEntry::line`].
    pub line: usize,
    /// Why the line was rejected.
    pub kind: LineErrorKind,
}

impl Display for LineError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.kind)
    }
}

/// Why one line was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineErrorKind {
    /// Not a valid PEP 508 requirement string. Carries the rendered
    /// message from `uv_pep508`'s parser (a [`uv_pep508::Pep508Error`]),
    /// stored as text since that error type isn't `Clone`/`PartialEq`.
    InvalidRequirement(String),
    /// A direct URL or VCS reference, which has no conda matchspec
    /// equivalent: either `name @ url` (valid PEP 508, but
    /// `version_or_url` is a URL) or a bare URL/VCS ref/local path (not
    /// valid PEP 508 at all).
    DirectUrlOrPath,
    /// A pip requirements-file directive this crate does not support:
    /// `-r`/`--requirement`, `-c`/`--constraint`, `-e`/`--editable`,
    /// `--hash`, or any other `-`/`--`-prefixed option line. Carries the
    /// offending token (e.g. `"-r"`, `"--hash"`).
    UnsupportedDirective(String),
    /// An `# ana-matchspec:` directive with nothing after the directive
    /// name.
    EmptyMatchspecDirective,
    /// An `# ana-matchspec:` directive's spec text is not valid conda
    /// `MatchSpec` syntax. Carries the rendered message from
    /// `rattler_conda_types`'s parser, stored as text for the same
    /// reason as [`LineErrorKind::InvalidRequirement`].
    InvalidMatchspec(String),
    /// An `# ana-channels:` directive with nothing after the directive
    /// name, or with a blank entry among its comma-separated list.
    EmptyChannelsDirective,
    /// A second (or further) `# ana-channels:` directive in the same
    /// file -- this is file-level state, so there is no good answer for
    /// "which one wins"; see the module docs.
    DuplicateChannelsDirective,
}

impl Display for LineErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequirement(message) => {
                write!(f, "invalid PEP 508 requirement: {message}")
            }
            Self::DirectUrlOrPath => write!(
                f,
                "direct URL/VCS/local-path requirements are not supported \
                 (no matchspec equivalent)"
            ),
            Self::UnsupportedDirective(directive) => {
                write!(f, "`{directive}` is not supported")
            }
            Self::EmptyMatchspecDirective => {
                write!(f, "`# ana-matchspec:` directive has no matchspec text")
            }
            Self::InvalidMatchspec(message) => {
                write!(f, "invalid conda matchspec: {message}")
            }
            Self::EmptyChannelsDirective => write!(
                f,
                "`# ana-channels:` directive has no channels, or contains a blank entry"
            ),
            Self::DuplicateChannelsDirective => write!(
                f,
                "`# ana-channels:` directive may appear at most once per file"
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! End-to-end tests for [`RequirementsTxt::parse`]: `requirements.txt`
    //! text in, typed `RequirementsTxt` (or an aggregated line-error list)
    //! out.

    use rattler_conda_types::MatchSpec;

    use super::*;

    fn req(spec: &str) -> Requirement {
        Requirement::from_str(spec).unwrap()
    }

    fn matchspec(spec: &str) -> MatchSpec {
        ana_dependency::parse_matchspec(spec).unwrap()
    }

    fn parse_ok(text: &str) -> RequirementsTxt {
        RequirementsTxt::parse(text).unwrap()
    }

    fn parse_err(text: &str) -> Vec<LineError> {
        RequirementsTxt::parse(text).unwrap_err().errors().to_vec()
    }

    /// A [`Dependency::Pep508`]-backed [`RequirementEntry`], for comparing
    /// against ordinary requirement lines.
    fn entry(spec: &str, line: usize) -> RequirementEntry {
        RequirementEntry {
            dependency: Dependency::Pep508(req(spec)),
            line,
        }
    }

    /// A [`Dependency::Matchspec`]-backed [`RequirementEntry`], for
    /// comparing against `# ana-matchspec:` directive lines.
    fn matchspec_entry(spec: &str, line: usize) -> RequirementEntry {
        RequirementEntry {
            dependency: Dependency::Matchspec(Box::new(matchspec(spec))),
            line,
        }
    }

    mod valid_documents {
        use super::*;

        #[test]
        fn empty_file() {
            assert_eq!(parse_ok(""), RequirementsTxt::default());
        }

        #[test]
        fn only_comments_and_blank_lines() {
            assert_eq!(
                parse_ok("# leading comment\n\n   \n# trailing comment\n"),
                RequirementsTxt::default()
            );
        }

        #[test]
        fn simple_requirements() {
            let parsed = parse_ok("foo==1.0\nbar>=2.0,<3.0\n");
            assert_eq!(
                parsed.requirements,
                vec![entry("foo==1.0", 1), entry("bar>=2.0,<3.0", 2)]
            );
        }

        #[test]
        fn requirement_with_extras_and_marker() {
            let parsed = parse_ok("requests[socks]>=2.0.0; sys_platform == \"win32\"\n");
            assert_eq!(
                parsed.requirements,
                vec![entry(
                    "requests[socks]>=2.0.0; sys_platform == \"win32\"",
                    1
                )]
            );
        }

        #[test]
        fn trailing_comment_and_blank_lines_between_requirements() {
            let parsed = parse_ok("foo==1.0  # pinned\n\nbar>=2.0\n");
            assert_eq!(
                parsed.requirements,
                vec![entry("foo==1.0", 1), entry("bar>=2.0", 3)]
            );
        }

        #[test]
        fn unversioned_requirement() {
            let parsed = parse_ok("foo\n");
            assert_eq!(parsed.requirements, vec![entry("foo", 1)]);
        }
    }

    mod matchspec_directives {
        use super::*;

        #[test]
        fn simple_matchspec_directive() {
            let parsed = parse_ok("# ana-matchspec: numpy >=1.26\n");
            assert_eq!(
                parsed.requirements,
                vec![matchspec_entry("numpy >=1.26", 1)]
            );
        }

        #[test]
        fn unversioned_matchspec_directive() {
            let parsed = parse_ok("# ana-matchspec: mkl\n");
            assert_eq!(parsed.requirements, vec![matchspec_entry("mkl", 1)]);
        }

        #[test]
        fn matchspec_and_pep508_lines_merge_in_file_order() {
            let parsed = parse_ok("ruff\n# ana-matchspec: compilers\nnumpy>=1.20\n");
            assert_eq!(
                parsed.requirements,
                vec![
                    entry("ruff", 1),
                    matchspec_entry("compilers", 2),
                    entry("numpy>=1.20", 3),
                ]
            );
        }

        #[test]
        fn matchspec_directive_with_extras() {
            let parsed = parse_ok("# ana-matchspec: numpy[build=*py311*]\n");
            assert_eq!(
                parsed.requirements,
                vec![matchspec_entry("numpy[build=*py311*]", 1)]
            );
        }

        #[test]
        fn empty_matchspec_directive_is_an_error() {
            assert_eq!(
                parse_err("# ana-matchspec:\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::EmptyMatchspecDirective
                }]
            );
        }

        #[test]
        fn whitespace_only_matchspec_directive_is_an_error() {
            assert_eq!(
                parse_err("# ana-matchspec:    \n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::EmptyMatchspecDirective
                }]
            );
        }

        #[test]
        fn invalid_matchspec_syntax_is_reported() {
            let errors = parse_err("# ana-matchspec: this is [ not valid\n");
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].line, 1);
            match &errors[0].kind {
                LineErrorKind::InvalidMatchspec(_) => {}
                other => panic!("expected InvalidMatchspec, got {other:?}"),
            }
        }

        #[test]
        fn explicit_channel_is_accepted() {
            let parsed = parse_ok("# ana-matchspec: conda-forge::numpy\n");
            let RequirementEntry {
                dependency: Dependency::Matchspec(spec),
                line,
            } = &parsed.requirements[0]
            else {
                panic!("expected a matchspec dependency");
            };
            assert_eq!(*line, 1);
            assert!(spec.channel.is_some());
        }

        #[test]
        fn ordinary_comments_are_still_dropped_not_reported() {
            assert_eq!(
                parse_ok("# just a comment\nfoo\n").requirements,
                vec![entry("foo", 2)]
            );
        }
    }

    mod channels_directive {
        use super::*;

        #[test]
        fn single_directive_parses_to_the_list() {
            let parsed = parse_ok("# ana-channels: conda-forge, bioconda\n");
            assert_eq!(
                parsed.channels,
                Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
            );
        }

        #[test]
        fn whitespace_around_entries_is_trimmed() {
            let parsed = parse_ok("#ana-channels:  conda-forge ,bioconda  \n");
            assert_eq!(
                parsed.channels,
                Some(vec!["conda-forge".to_string(), "bioconda".to_string()])
            );
        }

        #[test]
        fn absent_file_has_no_channels() {
            let parsed = parse_ok("foo==1.0\n");
            assert_eq!(parsed.channels, None);
        }

        #[test]
        fn empty_directive_is_an_error() {
            assert_eq!(
                parse_err("# ana-channels:\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::EmptyChannelsDirective
                }]
            );
        }

        #[test]
        fn whitespace_only_directive_is_an_error() {
            assert_eq!(
                parse_err("# ana-channels:    \n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::EmptyChannelsDirective
                }]
            );
        }

        #[test]
        fn blank_entry_in_list_is_an_error() {
            assert_eq!(
                parse_err("# ana-channels: conda-forge, ,bioconda\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::EmptyChannelsDirective
                }]
            );
        }

        /// `entry.trim()` only strips ASCII/Unicode whitespace
        /// (`char::is_whitespace()`) -- a zero-width space (Unicode
        /// category Cf, not whitespace) is not whitespace and survives
        /// verbatim, keeping the entry byte-distinct from the clean name
        /// it visually resembles rather than silently colliding with it.
        #[test]
        fn zero_width_space_in_an_entry_is_preserved_verbatim_not_stripped() {
            let parsed = parse_ok("# ana-channels: conda-forge\u{200b}\n");
            let channels = parsed.channels.unwrap();
            assert_eq!(channels, vec!["conda-forge\u{200b}".to_string()]);
            assert_ne!(channels[0], "conda-forge");
        }

        #[test]
        fn duplicate_directive_is_an_error() {
            let errors =
                parse_err("# ana-channels: conda-forge\nfoo==1.0\n# ana-channels: bioconda\n");
            assert_eq!(
                errors,
                vec![LineError {
                    line: 3,
                    kind: LineErrorKind::DuplicateChannelsDirective
                }]
            );
        }

        #[test]
        fn third_occurrence_is_also_an_error() {
            let errors = parse_err("# ana-channels: a\n# ana-channels: b\n# ana-channels: c\n");
            assert_eq!(
                errors,
                vec![
                    LineError {
                        line: 2,
                        kind: LineErrorKind::DuplicateChannelsDirective
                    },
                    LineError {
                        line: 3,
                        kind: LineErrorKind::DuplicateChannelsDirective
                    },
                ]
            );
        }

        #[test]
        fn directive_interleaved_with_ordinary_and_matchspec_lines_still_parses_those() {
            let parsed =
                parse_ok("# ana-channels: conda-forge\nfoo==1.0\n# ana-matchspec: mkl\nbar>=2.0\n");
            assert_eq!(parsed.channels, Some(vec!["conda-forge".to_string()]));
            assert_eq!(
                parsed.requirements,
                vec![
                    entry("foo==1.0", 2),
                    matchspec_entry("mkl", 3),
                    entry("bar>=2.0", 4)
                ]
            );
        }

        #[test]
        fn directive_does_not_become_a_requirement_entry() {
            let parsed = parse_ok("# ana-channels: conda-forge\n");
            assert_eq!(parsed.requirements, vec![]);
        }
    }

    mod unsupported_directives {
        use super::*;

        #[test]
        fn recursive_requirements_file_is_rejected() {
            assert_eq!(
                parse_err("-r other.txt\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("-r".to_string())
                }]
            );
        }

        #[test]
        fn long_form_requirement_flag_is_rejected() {
            assert_eq!(
                parse_err("--requirement other.txt\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("--requirement".to_string())
                }]
            );
        }

        #[test]
        fn constraints_file_is_rejected() {
            assert_eq!(
                parse_err("-c constraints.txt\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("-c".to_string())
                }]
            );
        }

        #[test]
        fn editable_is_rejected() {
            assert_eq!(
                parse_err("-e .\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("-e".to_string())
                }]
            );
        }

        #[test]
        fn editable_long_form_is_rejected() {
            assert_eq!(
                parse_err("--editable ./local-pkg\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("--editable".to_string())
                }]
            );
        }

        #[test]
        fn index_url_option_is_rejected() {
            assert_eq!(
                parse_err("--index-url https://example.com/simple\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("--index-url".to_string())
                }]
            );
        }

        #[test]
        fn inline_hash_is_rejected() {
            assert_eq!(
                parse_err("foo==1.0 --hash=sha256:abc\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("--hash".to_string())
                }]
            );
        }

        #[test]
        fn continuation_joined_hash_is_rejected() {
            assert_eq!(
                parse_err("foo==1.0 \\\n    --hash=sha256:abc\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("--hash".to_string())
                }]
            );
        }

        #[test]
        fn bare_hash_token_without_equals_is_rejected() {
            assert_eq!(
                parse_err("foo==1.0 --hash sha256:abc\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::UnsupportedDirective("--hash".to_string())
                }]
            );
        }
    }

    mod direct_urls_and_paths {
        use super::*;

        #[test]
        fn name_at_https_url_is_rejected() {
            assert_eq!(
                parse_err("foo @ https://example.com/foo-1.0.whl\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::DirectUrlOrPath
                }]
            );
        }

        #[test]
        fn name_at_git_url_is_rejected() {
            assert_eq!(
                parse_err("foo @ git+https://example.com/foo.git\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::DirectUrlOrPath
                }]
            );
        }

        #[test]
        fn bare_https_url_is_rejected() {
            assert_eq!(
                parse_err("https://example.com/foo-1.0-py3-none-any.whl\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::DirectUrlOrPath
                }]
            );
        }

        #[test]
        fn bare_git_vcs_ref_is_rejected() {
            assert_eq!(
                parse_err("git+https://example.com/foo.git\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::DirectUrlOrPath
                }]
            );
        }

        #[test]
        fn relative_local_path_is_rejected() {
            assert_eq!(
                parse_err("./local-package\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::DirectUrlOrPath
                }]
            );
        }

        #[test]
        fn absolute_local_path_is_rejected() {
            assert_eq!(
                parse_err("/opt/packages/local-package\n"),
                vec![LineError {
                    line: 1,
                    kind: LineErrorKind::DirectUrlOrPath
                }]
            );
        }
    }

    mod requirement_parse_errors {
        use super::*;

        #[test]
        fn invalid_pep508_syntax_reports_the_underlying_message() {
            let errors = parse_err("not a valid==requirement==string\n");
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].line, 1);
            match &errors[0].kind {
                LineErrorKind::InvalidRequirement(_) => {}
                other => panic!("expected InvalidRequirement, got {other:?}"),
            }
        }

        #[test]
        fn every_bad_line_is_collected_not_just_the_first() {
            let errors = parse_err("-r other.txt\nfoo==1.0\n-c constraints.txt\n");
            assert_eq!(
                errors,
                vec![
                    LineError {
                        line: 1,
                        kind: LineErrorKind::UnsupportedDirective("-r".to_string())
                    },
                    LineError {
                        line: 3,
                        kind: LineErrorKind::UnsupportedDirective("-c".to_string())
                    },
                ]
            );
        }

        #[test]
        fn every_bad_line_is_collected_across_both_dependency_kinds() {
            let errors = parse_err("-r other.txt\n# ana-matchspec:\nfoo==1.0\n");
            assert_eq!(
                errors,
                vec![
                    LineError {
                        line: 1,
                        kind: LineErrorKind::UnsupportedDirective("-r".to_string())
                    },
                    LineError {
                        line: 2,
                        kind: LineErrorKind::EmptyMatchspecDirective
                    },
                ]
            );
        }

        #[test]
        fn valid_lines_around_a_bad_one_still_parse() {
            // A rejected line shouldn't stop its neighbors from parsing --
            // only `RequirementsTxtError` (not `RequirementsTxt`) is
            // returned when there's at least one bad line, but that
            // error should carry exactly the one problem, not incidental
            // fallout from lines that were actually fine.
            let errors = parse_err("foo==1.0\n-e .\nbar==2.0\n");
            assert_eq!(
                errors,
                vec![LineError {
                    line: 2,
                    kind: LineErrorKind::UnsupportedDirective("-e".to_string())
                }]
            );
        }
    }

    mod display {
        use super::*;

        #[test]
        fn line_error_display_includes_line_number() {
            let err = LineError {
                line: 5,
                kind: LineErrorKind::UnsupportedDirective("-r".to_string()),
            };
            assert_eq!(err.to_string(), "line 5: `-r` is not supported");
        }

        #[test]
        fn requirements_txt_error_display_lists_every_line() {
            let err = RequirementsTxt::parse("-r a.txt\n-c b.txt\n").unwrap_err();
            let rendered = err.to_string();
            assert!(rendered.contains("line 1:"));
            assert!(rendered.contains("line 2:"));
        }
    }
}
