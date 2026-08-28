//! `ana clean`: remove every materialized environment for the project.
//!
//! Two kinds of environments exist (`investigations/env_storage.md`):
//! the default one (`<root>/ana.lock`, `<root>/.env/`) and, for every
//! `--group` selection anyone has ever run, a group environment
//! (`<root>/.ana/<hash>/ana.lock`, `<root>/.ana/<hash>/env/`).
//!
//! - The default environment's `ana.lock` is committed and kept: `clean`
//!   removes only `.env/`, exactly like a dirty-env-lock wipe (see
//!   `ana_lockfile::ensure_current_platform_locked`'s docs) but explicit
//!   and unconditional.
//! - A group environment's `.ana/<hash>/ana.lock` is treated as
//!   ephemeral, disposable state -- unlike the default environment's
//!   lock, nothing under `.ana/<hash>/` is committed, so `clean` removes
//!   the *whole* directory, `ana.lock` included.
//! - `.ana/locks/` (the advisory lock files) is left alone: those are the
//!   flock files themselves, not materialized environment content, and
//!   deleting one out from under a concurrent holder would break mutual
//!   exclusion.
//!

use std::fs;
use std::path::{Path, PathBuf};

use ana_fs_util::remove_dir_all_if_exists;
use ana_lockfile::EnvironmentLock;
use ana_paths::discover_paths;

use crate::Error;

/// One environment directory `ana clean` actually removed (it existed
/// before the call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedEnvironment {
    /// The directory that was removed: `.env` for the default
    /// environment, or `.ana/<hash>` for a group environment.
    pub path: PathBuf,
}

/// What `ana clean` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanOutcome {
    /// Every environment directory that existed and was removed, in the
    /// order they were processed (default environment first, then group
    /// environments in whatever order `.ana/` happened to list them).
    pub removed: Vec<CleanedEnvironment>,
}

/// `ana clean`, with `project_dir` as the project root (the process's
/// working directory, in the binary) -- see the module docs for exactly
/// what is and isn't removed.
///
/// There is deliberately no walk-up to find the root, matching `ana
/// run`/`ana sync`: `project_dir` must be the directory containing
/// `pyproject.toml`.
pub fn clean_command(project_dir: &Path) -> Result<CleanOutcome, Error> {
    if !project_dir.join("pyproject.toml").is_file() {
        return Err(Error::NoProjectRoot);
    }

    let mut removed = Vec::new();

    // The default environment: only `.env/` goes, `ana.lock` stays.
    let default_paths = discover_paths(project_dir, &[]);
    if remove_locked(&default_paths.advisory_lock_path(), &default_paths.env_path)? {
        removed.push(CleanedEnvironment {
            path: default_paths.env_path,
        });
    }

    // Every group environment discovered under `.ana/`: the whole
    // `.ana/<hash>/` directory, `ana.lock` included (see module docs for
    // why this differs from the default environment's treatment).
    let ana_dir = project_dir.join(".ana");
    for hash in list_group_hashes(&ana_dir)? {
        let paths = ana_paths::discover_by_hash(project_dir, &hash);
        let Some(group_dir) = paths.group_dir() else {
            // `discover_by_hash` always sets a lock key, so this is
            // unreachable in practice; skipped rather than unwrapped so a
            // future change to that invariant fails safe instead of
            // panicking on untrusted directory listings.
            continue;
        };
        if remove_locked(&paths.advisory_lock_path(), &group_dir)? {
            removed.push(CleanedEnvironment { path: group_dir });
        }
    }

    Ok(CleanOutcome { removed })
}

/// Every subdirectory of `.ana/` except `locks/` -- each one is a group
/// environment's own directory, named by its selection hash. A missing
/// `.ana/` is not an error: no group environment has ever been
/// materialized.
fn list_group_hashes(ana_dir: &Path) -> Result<Vec<String>, Error> {
    let entries = match fs::read_dir(ana_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::ReadDir {
                path: ana_dir.to_path_buf(),
                source,
            })
        }
    };

    let mut hashes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::ReadDir {
            path: ana_dir.to_path_buf(),
            source,
        })?;
        if entry.file_name() == "locks" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                hashes.push(name.to_string());
            }
        }
    }
    Ok(hashes)
}

/// Acquire `lock_path`'s advisory lock, then remove `target` recursively
/// under it -- so a concurrent `ana run`/`ana sync` against the same
/// environment can never race a clean. Returns whether `target` actually
/// existed (for [`CleanOutcome`]'s reporting).
fn remove_locked(lock_path: &Path, target: &Path) -> Result<bool, Error> {
    let existed = target.exists();
    let mut lock = EnvironmentLock::open(lock_path).map_err(|source| Error::Lock {
        path: lock_path.to_path_buf(),
        source,
    })?;
    let _guard = lock.acquire().map_err(|source| Error::Lock {
        path: lock_path.to_path_buf(),
        source,
    })?;
    remove_dir_all_if_exists(target).map_err(|source| Error::DeleteEnv {
        path: target.to_path_buf(),
        source,
    })?;
    Ok(existed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;

    use super::*;

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
dependencies = ["requests"]

[dependency-groups]
dev = ["ruff"]
"#;

    fn project_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), PYPROJECT).unwrap();
        dir
    }

    #[test]
    fn missing_project_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            clean_command(dir.path()),
            Err(Error::NoProjectRoot)
        ));
    }

    #[test]
    fn removes_default_env_but_keeps_its_lock() {
        let dir = project_root();
        let root = dir.path();
        fs::create_dir_all(root.join(".env/conda-meta")).unwrap();
        fs::write(root.join(".env/ana.lock"), b"dirty = false\n").unwrap();
        fs::write(root.join("ana.lock"), b"version = 1\n").unwrap();

        let outcome = clean_command(root).unwrap();

        assert!(!root.join(".env").exists());
        assert!(root.join("ana.lock").exists());
        assert_eq!(
            outcome.removed,
            vec![CleanedEnvironment {
                path: root.join(".env")
            }]
        );
    }

    #[test]
    fn removes_the_whole_group_environment_directory() {
        let dir = project_root();
        let root = dir.path();
        let hash = "ef260e9a";
        fs::create_dir_all(root.join(".ana").join(hash).join("env/conda-meta")).unwrap();
        fs::write(
            root.join(".ana").join(hash).join("ana.lock"),
            b"version = 1\n",
        )
        .unwrap();

        let outcome = clean_command(root).unwrap();

        assert!(
            !root.join(".ana").join(hash).exists(),
            "the whole .ana/<hash> directory goes, ana.lock included"
        );
        assert_eq!(
            outcome.removed,
            vec![CleanedEnvironment {
                path: root.join(".ana").join(hash)
            }]
        );
    }

    #[test]
    fn leaves_the_advisory_locks_directory_alone() {
        let dir = project_root();
        let root = dir.path();
        fs::create_dir_all(root.join(".ana/locks")).unwrap();
        fs::write(root.join(".ana/locks/default.lock"), b"").unwrap();
        fs::create_dir_all(root.join(".ana/ef260e9a/env")).unwrap();
        fs::write(root.join(".ana/ef260e9a/ana.lock"), b"version = 1\n").unwrap();

        clean_command(root).unwrap();

        assert!(root.join(".ana/locks/default.lock").exists());
        assert!(!root.join(".ana/ef260e9a").exists());
    }

    #[test]
    fn is_a_noop_when_nothing_was_ever_materialized() {
        let dir = project_root();
        let outcome = clean_command(dir.path()).unwrap();
        assert_eq!(outcome, CleanOutcome::default());
    }

    #[test]
    fn cleans_every_group_environment_present() {
        let dir = project_root();
        let root = dir.path();
        for hash in ["ef260e9a", "e62119cb"] {
            fs::create_dir_all(root.join(".ana").join(hash).join("env")).unwrap();
            fs::write(
                root.join(".ana").join(hash).join("ana.lock"),
                b"version = 1\n",
            )
            .unwrap();
        }

        let outcome = clean_command(root).unwrap();

        assert!(!root.join(".ana/ef260e9a").exists());
        assert!(!root.join(".ana/e62119cb").exists());
        assert_eq!(outcome.removed.len(), 2);
    }
}
