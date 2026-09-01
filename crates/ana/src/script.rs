//! PEP 723 inline script detection for `ana run`: given the CLI's
//! `<primary>` token, decides whether it names an existing `.py` file
//! with a `# /// script ... # ///` metadata block (PEP 723) rather than
//! an ordinary program to exec, and if so builds the
//! [`RequirementSet`] it declares.
//!
//! Detection requires a `.py` extension: without it, a working-directory
//! file that happens to share a name with a program (`pytest`, `black`)
//! would be read as a script, hijacking the exec away from the
//! environment's own program.
//!
//! Detection is otherwise deliberately permissive about failure: any
//! I/O problem reading the candidate path (missing, a directory,
//! unreadable, not UTF-8) falls back to [`DetectedScript::NotAScript`]
//! rather than an error -- `<primary>` was never necessarily meant to be
//! read as a file at all. That's distinct from a real `.py` file with no
//! metadata block at all ([`DetectedScript::MissingMetadata`]), which a
//! caller may want to handle differently (see `ana`'s own `main.rs`).
//! Only a *found* metadata block that fails to parse is a real error: at
//! that point the user has clearly declared PEP 723 metadata, so
//! silently ignoring a mistake in it would run the script without the
//! dependencies/version it asked for.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ana_dependency::Dependency;
use ana_requirements::RequirementSet;
use indexmap::IndexMap;

/// How much of a candidate file's head is read while looking for its
/// metadata block. The block lives in the file's top comment region, so
/// this only needs to bound the read for an arbitrarily large file
/// passed as `<primary>`; a block starting past the window is simply
/// not detected.
const MAX_HEADER_READ: u64 = 64 * 1024;

/// `ana run <script>.py --agent <MODE>`'s own value: whether a `.py`
/// file [`detect_script`] finds to have no PEP 723 metadata is routed
/// to a Kilo session for help adding some at all, and if so, whether
/// that session can ask a live user for permission before editing
/// anything (`Interactive`) or must decide on its own because none is
/// present (`Headless`, for scripted/CI contexts). Doesn't affect
/// whether an *existing* metadata block is recognized -- only what
/// happens when one is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ScriptAssistMode {
    /// Never route to Kilo: a `.py` file with no metadata is treated
    /// exactly as it would be without this feature at all -- as an
    /// ordinary program name, not a script.
    Off,
    /// Route to Kilo non-interactively (`kilo run --auto`): no live
    /// user is asked for permission, so the session must decide for
    /// itself whether/how to add metadata.
    Headless,
    /// Route to Kilo interactively (the default): a live user is asked
    /// for permission before anything is edited.
    #[default]
    Interactive,
}

/// What [`detect_script`] found `candidate` to be.
#[derive(Debug)]
pub enum DetectedScript {
    /// A `.py` file with a PEP 723 `# /// script` metadata block: its
    /// canonicalized path and the [`RequirementSet`] that block
    /// declares -- see [`ensure_python`] for why that set is never
    /// missing an interpreter, even for a script with no dependencies
    /// of its own.
    Found(PathBuf, RequirementSet),
    /// A `.py` file that exists, is a regular file, and was readable,
    /// but declares no `# /// script` block at all -- distinct from
    /// [`NotAScript`](DetectedScript::NotAScript) so a caller can offer
    /// to add one rather than silently falling back to treating
    /// `candidate` as an ordinary program name.
    MissingMetadata(PathBuf),
    /// Not a script at all: the wrong extension, or any I/O problem
    /// reading `candidate` (missing, a directory, unreadable, not
    /// UTF-8) -- see the module docs for why those are folded together
    /// rather than reported as separate cases.
    NotAScript,
}

/// Classifies `candidate` (resolved against `cwd`): a `.py` file with a
/// PEP 723 metadata block, one without, or not a script at all. See
/// [`DetectedScript`]'s own docs for what each case means to a caller.
pub fn detect_script(
    cwd: &Path,
    candidate: &str,
) -> Result<DetectedScript, ana_pep723::Pep723Error> {
    let path = cwd.join(candidate);
    if path.extension().is_none_or(|ext| ext != "py") {
        return Ok(DetectedScript::NotAScript);
    }

    let Ok(file) = fs::File::open(&path) else {
        return Ok(DetectedScript::NotAScript);
    };
    let Ok(metadata) = file.metadata() else {
        return Ok(DetectedScript::NotAScript);
    };
    if !metadata.is_file() {
        return Ok(DetectedScript::NotAScript);
    }
    let mut source = String::new();
    if file
        .take(MAX_HEADER_READ)
        .read_to_string(&mut source)
        .is_err()
    {
        return Ok(DetectedScript::NotAScript);
    }

    // Best-effort: an already-opened, just-read file failing to
    // canonicalize (e.g. removed between the read above and here) falls
    // back to the joined-but-uncanonicalized path rather than losing
    // the script entirely -- the same race would just as likely surface
    // later, reading the lock/env paths this key names.
    let path = path.canonicalize().unwrap_or(path);

    let Some(script) = ana_pep723::parse(&source)? else {
        return Ok(DetectedScript::MissingMetadata(path));
    };

    let has_requires_python = script
        .requires_python
        .as_ref()
        .is_some_and(|specifiers| !specifiers.is_empty());
    let dependencies = ensure_python(script.dependencies, has_requires_python);
    let requirements = RequirementSet::new(
        dependencies,
        IndexMap::new(),
        script.requires_python,
        script.channels,
    );
    Ok(DetectedScript::Found(path, requirements))
}

/// Guarantees at least one `python` requirement is present: unnecessary
/// when `requires_python` carries at least one specifier (matchspec
/// conversion already derives a `python` matchspec from it downstream --
/// an empty `requires-python = ""` derives nothing and is treated like
/// an absent one), and skipped when a `python` dependency is already
/// declared explicitly. Otherwise appends a bare, unconstrained
/// `python` `MatchSpec` -- the same one `ana run -g python` would build
/// from its own bare primary -- rather than a PEP 508 `Requirement`,
/// since `python` is not a real PyPI package the pypi-to-conda mapping
/// would know how to translate.
fn ensure_python(mut dependencies: Vec<Dependency>, requires_python: bool) -> Vec<Dependency> {
    if requires_python {
        return dependencies;
    }
    let already_declared = dependencies
        .iter()
        .any(|dependency| ana_dependency::bare_name(dependency).as_deref() == Some("python"));
    if !already_declared {
        if let Ok(spec) = ana_dependency::parse_matchspec("python") {
            dependencies.push(Dependency::Matchspec(Box::new(spec)));
        }
    }
    dependencies
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        fs::write(dir.join(name), contents).unwrap();
    }

    /// Unwraps a [`DetectedScript::Found`], panicking with the actual
    /// variant otherwise -- every test that expects a real script uses
    /// this rather than repeating the match arms.
    fn found(result: Result<DetectedScript, ana_pep723::Pep723Error>) -> (PathBuf, RequirementSet) {
        match result.unwrap() {
            DetectedScript::Found(path, requirements) => (path, requirements),
            other => panic!("expected DetectedScript::Found, got {other:?}"),
        }
    }

    const SCRIPT: &str = "\
# /// script
# requires-python = \">=3.11\"
# dependencies = [
#   \"requests<3\",
# ]
# ///
print(\"hi\")
";

    #[test]
    fn detects_a_pep_723_script() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "hello.py", SCRIPT);

        let (path, requirements) = found(detect_script(dir.path(), "hello.py"));
        assert_eq!(path, dir.path().join("hello.py").canonicalize().unwrap());
        assert_eq!(requirements.select(&[]).unwrap().len(), 1);
        assert!(requirements.requires_python().is_some());
    }

    #[test]
    fn a_plain_program_name_is_not_a_script() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            detect_script(dir.path(), "pytest").unwrap(),
            DetectedScript::NotAScript
        ));
    }

    #[test]
    fn a_file_with_no_metadata_block_reports_missing_metadata() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plain.py", "print('hi')\n");
        let DetectedScript::MissingMetadata(path) = detect_script(dir.path(), "plain.py").unwrap()
        else {
            panic!("expected DetectedScript::MissingMetadata");
        };
        assert_eq!(path, dir.path().join("plain.py").canonicalize().unwrap());
    }

    #[test]
    fn a_directory_is_not_a_script() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("adir.py")).unwrap();
        assert!(matches!(
            detect_script(dir.path(), "adir.py").unwrap(),
            DetectedScript::NotAScript
        ));
    }

    #[test]
    fn a_non_py_file_with_a_metadata_block_is_not_a_script() {
        // The `.py` gate is what keeps a working-directory file that
        // shares a name with a program from hijacking `ana run <name>`.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pytest", SCRIPT);
        write(dir.path(), "hello.sh", SCRIPT);
        assert!(matches!(
            detect_script(dir.path(), "pytest").unwrap(),
            DetectedScript::NotAScript
        ));
        assert!(matches!(
            detect_script(dir.path(), "hello.sh").unwrap(),
            DetectedScript::NotAScript
        ));
    }

    #[test]
    fn a_block_beyond_the_read_window_reports_missing_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let padding = "# padding\n".repeat((MAX_HEADER_READ as usize / 10) + 1);
        write(dir.path(), "deep.py", &format!("{padding}{SCRIPT}"));
        assert!(matches!(
            detect_script(dir.path(), "deep.py").unwrap(),
            DetectedScript::MissingMetadata(_)
        ));
    }

    #[test]
    fn a_large_file_with_a_block_in_its_head_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let body = "print('x')\n".repeat((MAX_HEADER_READ as usize / 11) + 1);
        write(dir.path(), "big.py", &format!("{SCRIPT}{body}"));
        assert!(matches!(
            detect_script(dir.path(), "big.py").unwrap(),
            DetectedScript::Found(..)
        ));
    }

    #[test]
    fn a_broken_metadata_block_is_a_real_error() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "broken.py",
            "# /// script\n# dependencies = [\"!!! not valid !!!\"]\n# ///\n",
        );
        assert!(detect_script(dir.path(), "broken.py").is_err());
    }

    #[test]
    fn a_script_with_no_dependencies_and_no_requires_python_still_gets_python() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "bare.py",
            "# /// script\n# dependencies = []\n# ///\nprint('hi')\n",
        );
        let (_, requirements) = found(detect_script(dir.path(), "bare.py"));
        let selected = requirements.select(&[]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            ana_dependency::bare_name(selected[0].dependency),
            Some("python".to_string())
        );
    }

    #[test]
    fn requires_python_alone_is_not_duplicated_into_an_extra_python_dependency() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pinned.py", SCRIPT);
        let (_, requirements) = found(detect_script(dir.path(), "pinned.py"));
        // `SCRIPT` declares one dependency (`requests`) and
        // `requires-python`; `ensure_python` must not add a second,
        // separate `python` entry on top of that -- matchspec
        // conversion derives one from `requires_python()` itself.
        assert_eq!(requirements.select(&[]).unwrap().len(), 1);
    }

    #[test]
    fn an_empty_requires_python_still_gets_python() {
        // `requires-python = ""` parses to an empty specifier set, from
        // which matchspec conversion derives no `python` constraint --
        // so `ensure_python` must treat it like an absent one.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "empty_rp.py",
            "# /// script\n# requires-python = \"\"\n# dependencies = []\n# ///\n",
        );
        let (_, requirements) = found(detect_script(dir.path(), "empty_rp.py"));
        let selected = requirements.select(&[]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(
            ana_dependency::bare_name(selected[0].dependency),
            Some("python".to_string())
        );
    }

    #[test]
    fn an_explicit_python_dependency_is_not_duplicated() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "explicit.py",
            "# /// script\n# dependencies = [\"python\"]\n# ///\n",
        );
        let (_, requirements) = found(detect_script(dir.path(), "explicit.py"));
        assert_eq!(requirements.select(&[]).unwrap().len(), 1);
    }

    #[test]
    fn the_returned_path_is_canonicalized() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "hello.py", SCRIPT);
        // Run detection with a relative-looking candidate resolved
        // against `dir` -- the returned path must still be absolute.
        let (path, _) = found(detect_script(dir.path(), "hello.py"));
        assert!(path.is_absolute());
    }

    #[test]
    fn the_missing_metadata_path_is_also_canonicalized() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plain.py", "print('hi')\n");
        let DetectedScript::MissingMetadata(path) = detect_script(dir.path(), "plain.py").unwrap()
        else {
            panic!("expected DetectedScript::MissingMetadata");
        };
        assert!(path.is_absolute());
    }
}
