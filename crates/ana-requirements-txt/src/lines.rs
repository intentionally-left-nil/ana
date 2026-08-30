//! Turns raw `requirements.txt` text into logical lines: comments
//! stripped, backslash-continued physical lines joined, blank lines
//! dropped, and `# ana-matchspec: <spec>`/`# ana-channels: <list>`
//! directive comments recognized as their own kind of line rather than
//! discarded as ordinary comments. This is a pure text-shape
//! transformation -- it has no idea what a valid requirement, matchspec,
//! or channel list looks like.
//!
//! ## The `# ana-matchspec: <spec>` directive
//!
//! Conda `MatchSpec` syntax isn't valid PEP 508, so it's declared via a
//! whole-line comment: after stripping leading whitespace, `#`, and
//! more whitespace, the line must start with the literal
//! `ana-matchspec:` (case-sensitive). It's recognized only when it
//! isn't the continuation of an already-open `\`-joined line.
//!
//! ## The `# ana-channels: <list>` directive
//!
//! A file-level channel override, recognized the same way as
//! `# ana-matchspec:` but with the literal `ana-channels:`. This module
//! only recognizes the line shape; validating how many times it may
//! appear, and parsing its comma-separated value, is `document.rs`'s
//! job.
//!
//! ## Comment stripping
//!
//! A `#` starts a comment -- to the end of its physical line -- when it
//! is the first character of the line, or preceded by whitespace. A `#`
//! glued directly onto a non-whitespace character (as in a URL fragment,
//! `https://example.com/x#frag`) is left alone. This runs before
//! continuation-joining, matching pip's semantics: a trailing `# comment`
//! after a `\` continuation is stripped before the next line is appended.
//!
//! ## Continuation joining
//!
//! A (comment-stripped, right-trimmed) physical line ending in `\` has
//! that backslash removed and the next physical line appended directly,
//! with no separator inserted -- so whitespace already present before
//! the `\` (`foo==1.0 \` continued by `    --hash=...`) is exactly what
//! ends up between the two halves. A trailing `\` on the file's last
//! physical line is dropped the same way, since there's no next line to
//! join.

use std::borrow::Cow;

/// The directive name recognized after `#` (plus surrounding
/// whitespace) at the start of a physical line.
const MATCHSPEC_DIRECTIVE: &str = "ana-matchspec:";

/// The file-level channel override directive name, recognized the same
/// way as [`MATCHSPEC_DIRECTIVE`].
const CHANNELS_DIRECTIVE: &str = "ana-channels:";

/// One logical (post-join, post-comment, non-blank, trimmed) line,
/// tagged with the physical line number it started on.
///
/// `text` borrows from the input whenever a logical line is exactly one
/// physical line -- the common case, and the only shape an
/// `# ana-matchspec:`/`# ana-channels:` directive can have. Only a
/// `\`-continuation chain, which must join non-adjacent slices together,
/// needs an owned buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LogicalLine<'a> {
    /// An ordinary logical line, expected to be a PEP 508 requirement.
    /// May have been joined from several `\`-continued physical lines.
    Requirement { line: usize, text: Cow<'a, str> },
    /// An `# ana-matchspec: <spec>` directive line. `text` is whatever
    /// followed the directive name, trimmed; it is not validated as a
    /// matchspec string here.
    Matchspec { line: usize, text: Cow<'a, str> },
    /// An `# ana-channels: <list>` directive line. `text` is whatever
    /// followed the directive name, trimmed; it is not split/validated
    /// as a channel list here.
    Channels { line: usize, text: Cow<'a, str> },
}

/// Extracts every logical line from `text`, in order. Blank lines and
/// comment-only lines are silently dropped.
pub(crate) fn logical_lines(text: &str) -> Vec<LogicalLine<'_>> {
    let mut result = Vec::new();
    // The logical line currently being accumulated across `\`-continued
    // lines, and the physical line it started on; `None` between
    // logical lines. A directive is only recognized while this is `None`.
    let mut pending: Option<(usize, String)> = None;

    for (index, raw) in text.lines().enumerate() {
        let physical_line = index + 1;

        if pending.is_none() {
            if let Some(spec_text) = match_directive(raw, MATCHSPEC_DIRECTIVE) {
                result.push(LogicalLine::Matchspec {
                    line: physical_line,
                    text: Cow::Borrowed(spec_text),
                });
                continue;
            }
            if let Some(channels_text) = match_directive(raw, CHANNELS_DIRECTIVE) {
                result.push(LogicalLine::Channels {
                    line: physical_line,
                    text: Cow::Borrowed(channels_text),
                });
                continue;
            }
        }

        let stripped = strip_comment(raw);
        let right_trimmed = stripped.trim_end();
        let continued = right_trimmed.strip_suffix('\\');
        let chunk = continued.unwrap_or(stripped);

        if pending.is_none() && continued.is_none() {
            push_borrowed_if_nonblank(&mut result, physical_line, chunk);
            continue;
        }

        match &mut pending {
            Some((_, buffer)) => buffer.push_str(chunk),
            None => pending = Some((physical_line, chunk.to_string())),
        }

        if continued.is_none() {
            if let Some((start, buffer)) = pending.take() {
                push_owned_if_nonblank(&mut result, start, buffer);
            }
        }
    }

    // A trailing `\` on the last physical line leaves `pending`
    // populated with no further line to join -- finalize it here.
    if let Some((start, buffer)) = pending {
        push_owned_if_nonblank(&mut result, start, buffer);
    }

    result
}

/// Trims and, unless that leaves nothing behind, pushes one finalized,
/// borrowed `LogicalLine::Requirement` -- the zero-allocation path for a
/// logical line that was exactly one physical line with no
/// `\`-continuation.
fn push_borrowed_if_nonblank<'a>(result: &mut Vec<LogicalLine<'a>>, start: usize, chunk: &'a str) {
    let trimmed = chunk.trim();
    if !trimmed.is_empty() {
        result.push(LogicalLine::Requirement {
            line: start,
            text: Cow::Borrowed(trimmed),
        });
    }
}

/// Trims in place and, unless that leaves nothing behind, pushes one
/// finalized, owned `LogicalLine::Requirement` -- the path for a
/// logical line that was `\`-continued across multiple physical lines,
/// which already required an owned buffer to join non-adjacent slices.
fn push_owned_if_nonblank(result: &mut Vec<LogicalLine<'_>>, start: usize, mut buffer: String) {
    let trimmed_end = buffer.trim_end().len();
    buffer.truncate(trimmed_end);
    let leading_ws = buffer.len() - buffer.trim_start().len();
    if leading_ws > 0 {
        buffer.drain(..leading_ws);
    }
    if !buffer.is_empty() {
        result.push(LogicalLine::Requirement {
            line: start,
            text: Cow::Owned(buffer),
        });
    }
}

/// If `raw` (a single, not-yet-joined physical line) is a `#
/// <directive><spec>` directive comment for the given directive name
/// (e.g. [`MATCHSPEC_DIRECTIVE`]/[`CHANNELS_DIRECTIVE`]), returns
/// whatever follows the directive name, trimmed (possibly empty).
fn match_directive<'a>(raw: &'a str, directive: &str) -> Option<&'a str> {
    let after_hash = raw.trim_start().strip_prefix('#')?;
    let rest = after_hash.trim_start().strip_prefix(directive)?;
    Some(rest.trim())
}

/// Strips a trailing comment from one physical line: everything from a
/// `#` that starts the line or is preceded by whitespace, to the end of
/// the line. A `#` immediately after a non-whitespace character is left
/// in place.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut prev_is_space = true; // start-of-line counts as "preceded by whitespace"
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && prev_is_space {
            return &line[..i];
        }
        prev_is_space = b == b' ' || b == b'\t';
    }
    line
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Every `LogicalLine::Requirement` in `text`, as `(line, text)`
    /// pairs -- for tests that don't care about `# ana-matchspec:`/
    /// `# ana-channels:` directives. Panics if `text` produces any
    /// `LogicalLine::Matchspec`/`LogicalLine::Channels`.
    fn extract(text: &str) -> Vec<(usize, String)> {
        logical_lines(text)
            .into_iter()
            .map(|l| match l {
                LogicalLine::Requirement { line, text } => (line, text.into_owned()),
                LogicalLine::Matchspec { line, text } => {
                    panic!("unexpected matchspec directive at line {line}: {text:?}")
                }
                LogicalLine::Channels { line, text } => {
                    panic!("unexpected channels directive at line {line}: {text:?}")
                }
            })
            .collect()
    }

    #[test]
    fn strip_comment_at_line_start() {
        assert_eq!(strip_comment("# a comment"), "");
    }

    #[test]
    fn strip_comment_after_whitespace() {
        assert_eq!(strip_comment("foo==1.0 # trailing"), "foo==1.0 ");
    }

    #[test]
    fn strip_comment_leaves_url_fragment_alone() {
        assert_eq!(
            strip_comment("foo @ https://example.com/x#frag"),
            "foo @ https://example.com/x#frag"
        );
    }

    #[test]
    fn empty_and_whitespace_only_input_yields_nothing() {
        assert_eq!(extract(""), vec![]);
        assert_eq!(extract("\n\n   \n\t\n"), vec![]);
    }

    #[test]
    fn simple_lines_are_independent() {
        assert_eq!(
            extract("foo==1.0\nbar>=2.0\n"),
            vec![(1, "foo==1.0".to_string()), (2, "bar>=2.0".to_string())]
        );
    }

    #[test]
    fn blank_and_comment_only_lines_are_dropped_but_numbering_survives() {
        assert_eq!(
            extract("foo==1.0\n\n# just a comment\n\nbar>=2.0\n"),
            vec![(1, "foo==1.0".to_string()), (5, "bar>=2.0".to_string())]
        );
    }

    #[test]
    fn trailing_comment_is_stripped_before_trim() {
        assert_eq!(
            extract("foo==1.0   # pinned for compat\n"),
            vec![(1, "foo==1.0".to_string())]
        );
    }

    #[test]
    fn continuation_joins_without_inserting_a_separator() {
        assert_eq!(
            extract("foo==1.0 \\\n    --hash=sha256:abc\n"),
            vec![(1, "foo==1.0     --hash=sha256:abc".to_string())]
        );
    }

    #[test]
    fn continuation_reports_the_starting_line_number() {
        assert_eq!(
            extract("bar>=1.0\nfoo==1.0 \\\n    --hash=sha256:abc\nbaz\n"),
            vec![
                (1, "bar>=1.0".to_string()),
                (2, "foo==1.0     --hash=sha256:abc".to_string()),
                (4, "baz".to_string()),
            ]
        );
    }

    #[test]
    fn comment_after_a_continuation_backslash_does_not_defeat_it() {
        assert_eq!(
            extract("foo==1.0 \\ # explains the continuation\nbar>=1.0\n"),
            // Comment stripping runs before the backslash check, so the
            // trailing `\` is still seen and the join still happens.
            vec![(1, "foo==1.0 bar>=1.0".to_string())]
        );
    }

    #[test]
    fn trailing_backslash_at_end_of_file_is_dropped() {
        assert_eq!(extract("foo==1.0 \\"), vec![(1, "foo==1.0".to_string())]);
    }

    #[test]
    fn multi_line_continuation_chain() {
        assert_eq!(
            extract("foo==1.0 \\\n    --hash=sha256:aaa \\\n    --hash=sha256:bbb\n"),
            vec![(
                1,
                "foo==1.0     --hash=sha256:aaa     --hash=sha256:bbb".to_string()
            )]
        );
    }

    mod matchspec_directive {
        use super::*;

        fn all_lines(text: &str) -> Vec<LogicalLine<'_>> {
            logical_lines(text)
        }

        #[test]
        fn recognized_on_its_own_line() {
            assert_eq!(
                all_lines("# ana-matchspec: numpy >=1.26\n"),
                vec![LogicalLine::Matchspec {
                    line: 1,
                    text: "numpy >=1.26".into()
                }]
            );
        }

        #[test]
        fn tolerates_extra_whitespace_around_hash_and_colon() {
            assert_eq!(
                all_lines("   #   ana-matchspec:    mkl   \n"),
                vec![LogicalLine::Matchspec {
                    line: 1,
                    text: "mkl".into()
                }]
            );
        }

        #[test]
        fn works_with_no_space_after_hash() {
            assert_eq!(
                all_lines("#ana-matchspec:compilers\n"),
                vec![LogicalLine::Matchspec {
                    line: 1,
                    text: "compilers".into()
                }]
            );
        }

        #[test]
        fn empty_directive_text_is_preserved_as_empty_not_dropped() {
            assert_eq!(
                all_lines("# ana-matchspec:\n"),
                vec![LogicalLine::Matchspec {
                    line: 1,
                    text: "".into()
                }]
            );
        }

        #[test]
        fn ordinary_comments_are_unaffected() {
            assert_eq!(all_lines("# just a comment\n"), vec![]);
            assert_eq!(all_lines("# ana-matchspec-like-but-not-quite\n"), vec![]);
        }

        #[test]
        fn mixes_with_requirement_lines_in_file_order() {
            assert_eq!(
                all_lines("foo==1.0\n# ana-matchspec: mkl\nbar>=2.0\n"),
                vec![
                    LogicalLine::Requirement {
                        line: 1,
                        text: "foo==1.0".into()
                    },
                    LogicalLine::Matchspec {
                        line: 2,
                        text: "mkl".into()
                    },
                    LogicalLine::Requirement {
                        line: 3,
                        text: "bar>=2.0".into()
                    },
                ]
            );
        }

        #[test]
        fn not_recognized_mid_continuation() {
            assert_eq!(
                all_lines("foo==1.0 \\\n# ana-matchspec: mkl\n"),
                vec![LogicalLine::Requirement {
                    line: 1,
                    text: "foo==1.0".into()
                }]
            );
        }
    }

    mod channels_directive {
        use super::*;

        fn all_lines(text: &str) -> Vec<LogicalLine<'_>> {
            logical_lines(text)
        }

        #[test]
        fn recognized_on_its_own_line() {
            assert_eq!(
                all_lines("# ana-channels: conda-forge, bioconda\n"),
                vec![LogicalLine::Channels {
                    line: 1,
                    text: "conda-forge, bioconda".into()
                }]
            );
        }

        #[test]
        fn tolerates_extra_whitespace_around_hash_and_colon() {
            assert_eq!(
                all_lines("   #   ana-channels:    conda-forge   \n"),
                vec![LogicalLine::Channels {
                    line: 1,
                    text: "conda-forge".into()
                }]
            );
        }

        #[test]
        fn works_with_no_space_after_hash() {
            assert_eq!(
                all_lines("#ana-channels:conda-forge\n"),
                vec![LogicalLine::Channels {
                    line: 1,
                    text: "conda-forge".into()
                }]
            );
        }

        #[test]
        fn empty_directive_text_is_preserved_as_empty_not_dropped() {
            assert_eq!(
                all_lines("# ana-channels:\n"),
                vec![LogicalLine::Channels {
                    line: 1,
                    text: "".into()
                }]
            );
        }

        #[test]
        fn ordinary_comments_are_unaffected() {
            assert_eq!(all_lines("# just a comment\n"), vec![]);
            assert_eq!(all_lines("# ana-channels-like-but-not-quite\n"), vec![]);
        }

        #[test]
        fn mixes_with_requirement_and_matchspec_lines_in_file_order() {
            assert_eq!(
                all_lines("# ana-channels: conda-forge\nnumpy>=1.20\n# ana-matchspec: mkl\n"),
                vec![
                    LogicalLine::Channels {
                        line: 1,
                        text: "conda-forge".into()
                    },
                    LogicalLine::Requirement {
                        line: 2,
                        text: "numpy>=1.20".into()
                    },
                    LogicalLine::Matchspec {
                        line: 3,
                        text: "mkl".into()
                    },
                ]
            );
        }

        #[test]
        fn not_recognized_mid_continuation() {
            assert_eq!(
                all_lines("foo==1.0 \\\n# ana-channels: conda-forge\n"),
                vec![LogicalLine::Requirement {
                    line: 1,
                    text: "foo==1.0".into()
                }]
            );
        }
    }
}
