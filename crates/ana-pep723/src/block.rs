//! Locating a PEP 723 metadata block's raw content within a Python
//! source file, per the algorithm PEP 723 itself specifies: a
//! `# /// <type>` header line, one or more `#`/`# `-prefixed content
//! lines, and a closing `# ///` line, each matched exactly (comment
//! markers included).

/// A second `# /// script` block was found; PEP 723 permits at most one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MultipleBlocks;

/// The un-prefixed content of the first `# /// script ... # ///` block
/// found in `source`, or `None` if none exists. An unterminated or
/// malformed block (a header with no matching close, or a content line
/// that isn't `#`/`# `-prefixed) is not a match at all, per PEP 723 --
/// not an error -- so scanning simply continues past its header line
/// looking for a real one.
///
/// A block of any *other* type (`# /// pyproject`, say) is recognized
/// and skipped over too, so its own `# ///` close is never mistaken for
/// closing an unrelated, unterminated `script` block that happens to
/// precede it. `Err` only for a second `script` block.
pub(crate) fn extract_script_block(source: &str) -> Result<Option<String>, MultipleBlocks> {
    let lines: Vec<&str> = source.lines().collect();
    let mut found: Option<String> = None;
    let mut i = 0;
    while i < lines.len() {
        let Some(block_type) = header_type(lines[i]) else {
            i += 1;
            continue;
        };
        let Some((content, next)) = read_block_content(&lines, i + 1) else {
            i += 1;
            continue;
        };
        if block_type == "script" {
            if found.is_some() {
                return Err(MultipleBlocks);
            }
            found = Some(content);
        }
        i = next;
    }
    Ok(found)
}

/// `line`'s block type, if it is a well-formed `# /// <type>` header:
/// `# /// ` followed by one or more of `[a-zA-Z0-9-]`, with nothing else
/// on the line.
fn header_type(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("# /// ")?;
    (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some(rest)
}

/// `line` is a valid metadata content line: exactly `#`, or `# `
/// followed by anything.
fn is_content_line(line: &str) -> bool {
    line == "#" || line.starts_with("# ")
}

/// Reads content lines starting at `lines[start]` (just after a header
/// line) up to and including a closing `# ///` line. Returns the
/// content with each line's `#`/`# ` marker stripped (each followed by
/// `\n`) and the index just past the close, or `None` if the content
/// stops matching before a close is found -- per PEP 723, at least one
/// content line is required, so a header immediately followed by
/// `# ///` is not a match either.
fn read_block_content(lines: &[&str], start: usize) -> Option<(String, usize)> {
    let mut content_lines: Vec<&str> = Vec::new();
    let mut j = start;
    while j < lines.len() {
        if lines[j] == "# ///" {
            if content_lines.is_empty() {
                return None;
            }
            let mut content = String::new();
            for line in &content_lines {
                content.push_str(line.strip_prefix("# ").unwrap_or(&line[1..]));
                content.push('\n');
            }
            return Some((content, j + 1));
        }
        if is_content_line(lines[j]) {
            content_lines.push(lines[j]);
            j += 1;
        } else {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn no_block_is_none() {
        assert_eq!(extract_script_block("print('hi')\n").unwrap(), None);
    }

    #[test]
    fn finds_a_simple_block() {
        let source = "\
# /// script
# dependencies = [\"requests\"]
# ///
print('hi')
";
        let content = extract_script_block(source).unwrap().unwrap();
        assert_eq!(content, "dependencies = [\"requests\"]\n");
    }

    #[test]
    fn a_bare_hash_content_line_becomes_a_blank_line() {
        let source = "\
# /// script
# dependencies = []
#
# [tool.ana]
# ///
";
        let content = extract_script_block(source).unwrap().unwrap();
        assert_eq!(content, "dependencies = []\n\n[tool.ana]\n");
    }

    #[test]
    fn a_non_script_block_type_is_skipped() {
        let source = "\
# /// pyproject
# [tool.foo]
# ///
# /// script
# dependencies = []
# ///
";
        let content = extract_script_block(source).unwrap().unwrap();
        assert_eq!(content, "dependencies = []\n");
    }

    #[test]
    fn a_second_script_block_is_an_error() {
        let source = "\
# /// script
# dependencies = []
# ///
# /// script
# dependencies = [\"requests\"]
# ///
";
        assert_eq!(extract_script_block(source), Err(MultipleBlocks));
    }

    #[test]
    fn an_unterminated_block_is_not_a_match() {
        let source = "\
# /// script
# dependencies = []
print('no close, falls through to plain code')
";
        assert_eq!(extract_script_block(source).unwrap(), None);
    }

    #[test]
    fn an_unterminated_block_does_not_hide_a_real_one_later() {
        let source = "\
# /// script
# dependencies = []
print('this one never closes')
# /// script
# dependencies = [\"requests\"]
# ///
";
        let content = extract_script_block(source).unwrap().unwrap();
        assert_eq!(content, "dependencies = [\"requests\"]\n");
    }

    #[test]
    fn an_empty_block_is_not_a_match() {
        // PEP 723's own regex requires at least one content line.
        let source = "\
# /// script
# ///
";
        assert_eq!(extract_script_block(source).unwrap(), None);
    }

    #[test]
    fn a_header_with_invalid_type_characters_is_ignored() {
        assert_eq!(
            extract_script_block("# /// not a valid type!\n# x = 1\n# ///\n").unwrap(),
            None
        );
    }

    #[test]
    fn the_closing_line_must_match_exactly() {
        let source = "\
# /// script
# dependencies = []
# /// (trailing text disqualifies this as a close)
";
        assert_eq!(extract_script_block(source).unwrap(), None);
    }
}
