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
//! unreadable, not UTF-8) falls back to `Ok(None)` rather than an
//! error -- `<primary>` was never necessarily meant to be read as a
//! file at all. Only a *found* metadata block that fails to parse is a
//! real error: at that point the user has clearly declared PEP 723
//! metadata, so silently ignoring a mistake in it would run the script
//! without the dependencies/version it asked for.

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

/// If `candidate` (resolved against `cwd`) is a `.py` file with a PEP
/// 723 `# /// script` metadata block, its canonicalized path and the
/// [`RequirementSet`] that block declares -- see [`ensure_python`] for
/// why that set is never missing an interpreter, even for a script with
/// no dependencies of its own. `Ok(None)` when `candidate` isn't a
/// script at all; see the module docs for why that covers every I/O
/// failure too, not just "no such file."
pub fn detect_script(
    cwd: &Path,
    candidate: &str,
) -> Result<Option<(PathBuf, RequirementSet)>, ana_pep723::Pep723Error> {
    let path = cwd.join(candidate);
    if path.extension().is_none_or(|ext| ext != "py") {
        return Ok(None);
    }

    let Ok(file) = fs::File::open(&path) else {
        return Ok(None);
    };
    let Ok(metadata) = file.metadata() else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut source = String::new();
    if file
        .take(MAX_HEADER_READ)
        .read_to_string(&mut source)
        .is_err()
    {
        return Ok(None);
    }

    let Some(script) = ana_pep723::parse(&source)? else {
        return Ok(None);
    };

    // Best-effort: an already-opened, just-read file failing to
    // canonicalize (e.g. removed between the read above and here) falls
    // back to the joined-but-uncanonicalized path rather than losing
    // the script entirely -- the same race would just as likely surface
    // later, reading the lock/env paths this key names.
    let path = path.canonicalize().unwrap_or(path);

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
    Ok(Some((path, requirements)))
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

        let (path, requirements) = detect_script(dir.path(), "hello.py").unwrap().unwrap();
        assert_eq!(path, dir.path().join("hello.py").canonicalize().unwrap());
        assert_eq!(requirements.select(&[]).unwrap().len(), 1);
        assert!(requirements.requires_python().is_some());
    }

    #[test]
    fn a_plain_program_name_is_not_a_script() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_script(dir.path(), "pytest").unwrap().is_none());
    }

    #[test]
    fn a_file_with_no_metadata_block_is_not_a_script() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "plain.py", "print('hi')\n");
        assert!(detect_script(dir.path(), "plain.py").unwrap().is_none());
    }

    #[test]
    fn a_directory_is_not_a_script() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("adir.py")).unwrap();
        assert!(detect_script(dir.path(), "adir.py").unwrap().is_none());
    }

    #[test]
    fn a_non_py_file_with_a_metadata_block_is_not_a_script() {
        // The `.py` gate is what keeps a working-directory file that
        // shares a name with a program from hijacking `ana run <name>`.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pytest", SCRIPT);
        write(dir.path(), "hello.sh", SCRIPT);
        assert!(detect_script(dir.path(), "pytest").unwrap().is_none());
        assert!(detect_script(dir.path(), "hello.sh").unwrap().is_none());
    }

    #[test]
    fn a_block_beyond_the_read_window_is_not_detected() {
        let dir = tempfile::tempdir().unwrap();
        let padding = "# padding\n".repeat((MAX_HEADER_READ as usize / 10) + 1);
        write(dir.path(), "deep.py", &format!("{padding}{SCRIPT}"));
        assert!(detect_script(dir.path(), "deep.py").unwrap().is_none());
    }

    #[test]
    fn a_large_file_with_a_block_in_its_head_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let body = "print('x')\n".repeat((MAX_HEADER_READ as usize / 11) + 1);
        write(dir.path(), "big.py", &format!("{SCRIPT}{body}"));
        assert!(detect_script(dir.path(), "big.py").unwrap().is_some());
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
        let (_, requirements) = detect_script(dir.path(), "bare.py").unwrap().unwrap();
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
        let (_, requirements) = detect_script(dir.path(), "pinned.py").unwrap().unwrap();
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
        let (_, requirements) = detect_script(dir.path(), "empty_rp.py").unwrap().unwrap();
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
        let (_, requirements) = detect_script(dir.path(), "explicit.py").unwrap().unwrap();
        assert_eq!(requirements.select(&[]).unwrap().len(), 1);
    }

    #[test]
    fn the_returned_path_is_canonicalized() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "hello.py", SCRIPT);
        // Run detection with a relative-looking candidate resolved
        // against `dir` -- the returned path must still be absolute.
        let (path, _) = detect_script(dir.path(), "hello.py").unwrap().unwrap();
        assert!(path.is_absolute());
    }
}
