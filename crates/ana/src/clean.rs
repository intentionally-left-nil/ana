//! `ana clean`: remove every materialized environment for the project;
//! `ana clean --global`: remove every ad hoc (`ana run -g`) environment
//! in the global cache instead.
//!
//! Three kinds of environments exist (`investigations/env_storage.md`):
//! the project's default one (`<root>/ana.lock`, `<root>/.env/`), a
//! project's group environment for every `--group` selection anyone has
//! ever run (`<root>/.ana/<key>/ana.lock`, `<root>/.ana/<key>/env/`),
//! and an ad hoc, project-less environment for every distinct `ana run
//! -g`/`-i` invocation (`<cache_root>/<key>/ana.lock`,
//! `<cache_root>/<key>/env/`).
//!
//! - The default environment's `ana.lock` is committed and kept: `clean`
//!   removes only `.env/`.
//! - A group or ad hoc environment's `ana.lock` is not committed, so
//!   `clean`/`clean --global` removes the *whole* directory, `ana.lock`
//!   included.
//! - `locks/` (the advisory lock files, under `.ana/` for a project or
//!   directly under the global cache root) is left alone: deleting one
//!   out from under a concurrent holder would break mutual exclusion.

use std::fs;
use std::path::{Path, PathBuf};

use ana_fs_util::remove_dir_all_if_exists;
use ana_lockfile::EnvironmentLock;
use ana_paths::{discover, EnvironmentKey, EnvironmentLayout};

use crate::Error;

/// One environment directory `ana clean`/`ana clean --global` actually
/// removed (it existed before the call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedEnvironment {
    /// The directory that was removed: `.env` for the default
    /// environment, `.ana/<key>` for a group environment, or
    /// `<cache_root>/<key>` for an ad hoc one.
    pub path: PathBuf,
}

/// What `ana clean`/`ana clean --global` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanOutcome {
    /// Every environment directory that existed and was removed, in the
    /// order they were processed (default environment first, then every
    /// keyed environment in whatever order its container happened to
    /// list them).
    pub removed: Vec<CleanedEnvironment>,
}

/// `ana clean`, with `project_dir` as the project root (the process's
/// working directory, in the binary). See the module docs for exactly
/// what is and isn't removed.
///
/// There is deliberately no walk-up to find the root, matching `ana
/// run`/`ana sync`: `project_dir` must directly contain a
/// `pyproject.toml` or `requirements.txt`.
pub fn clean_command(project_dir: &Path) -> Result<CleanOutcome, Error> {
    if !ana_environment::project_file_exists(project_dir) {
        return Err(Error::Environment(ana_environment::Error::NoProjectFile {
            path: project_dir.to_path_buf(),
        }));
    }

    let mut removed = Vec::new();

    let default_paths = discover(EnvironmentLayout::ProjectDefault { root: project_dir });
    let keyed_container = default_paths.keyed_container();
    if remove_locked(&default_paths.advisory_lock_path(), &default_paths.env_path)? {
        removed.push(CleanedEnvironment {
            path: default_paths.env_path,
        });
    }

    for key in list_keyed_entries(&keyed_container)? {
        let paths = discover(EnvironmentLayout::ProjectKeyed {
            root: project_dir,
            key: EnvironmentKey::from_raw(key),
        });
        let Some(group_dir) = paths.group_dir() else {
            // A `ProjectKeyed` layout always has a group dir, so this is
            // unreachable in practice; skipped rather than unwrapped so
            // a future change to that invariant fails safe instead of
            // panicking on untrusted directory listings.
            continue;
        };
        if remove_locked(&paths.advisory_lock_path(), &group_dir)? {
            removed.push(CleanedEnvironment { path: group_dir });
        }
    }

    Ok(CleanOutcome { removed })
}

/// `ana clean --global`: remove every ad hoc (`ana run -g`/`-i`)
/// environment under `cache_root`, leaving `locks/` alone. Unlike
/// [`clean_command`], there is no project-file precondition -- an ad hoc
/// environment has no project of its own -- and the current project's
/// environments (if the caller happens to be run from inside one) are
/// never touched.
pub fn clean_global_command(cache_root: &Path) -> Result<CleanOutcome, Error> {
    let mut removed = Vec::new();

    for key in list_keyed_entries(cache_root)? {
        let paths = discover(EnvironmentLayout::Global {
            cache_root,
            key: EnvironmentKey::from_raw(key),
        });
        let Some(dir) = paths.group_dir() else {
            // A `Global` layout always has a group dir; see
            // `clean_command`'s identical guard for why this is skipped
            // rather than unwrapped.
            continue;
        };
        if remove_locked(&paths.advisory_lock_path(), &dir)? {
            removed.push(CleanedEnvironment { path: dir });
        }
    }

    Ok(CleanOutcome { removed })
}

/// Every subdirectory of `container` except `locks/` -- each one is a
/// keyed environment's own directory, named by its key (a project's
/// `.ana/`, or the global cache root). A missing `container` is not an
/// error: no keyed environment has ever been materialized there.
fn list_keyed_entries(container: &Path) -> Result<Vec<String>, Error> {
    let entries = match fs::read_dir(container) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::ReadDir {
                path: container.to_path_buf(),
                source,
            })
        }
    };

    let mut keys = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::ReadDir {
            path: container.to_path_buf(),
            source,
        })?;
        if entry.file_name() == "locks" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                keys.push(name.to_string());
            }
        }
    }
    Ok(keys)
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
            Err(Error::Environment(
                ana_environment::Error::NoProjectFile { .. }
            ))
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

    #[test]
    fn global_removes_every_ad_hoc_environment_directory() {
        let cache = tempfile::tempdir().unwrap();
        let cache_root = cache.path();
        let key = "a".repeat(64);
        fs::create_dir_all(cache_root.join(&key).join("env")).unwrap();
        fs::write(cache_root.join(&key).join("ana.lock"), b"version = 1\n").unwrap();

        let outcome = clean_global_command(cache_root).unwrap();

        assert!(!cache_root.join(&key).exists());
        assert_eq!(
            outcome.removed,
            vec![CleanedEnvironment {
                path: cache_root.join(&key)
            }]
        );
    }

    #[test]
    fn global_leaves_the_advisory_locks_directory_alone() {
        let cache = tempfile::tempdir().unwrap();
        let cache_root = cache.path();
        fs::create_dir_all(cache_root.join("locks")).unwrap();
        fs::write(cache_root.join("locks/default.lock"), b"").unwrap();
        let key = "b".repeat(64);
        fs::create_dir_all(cache_root.join(&key).join("env")).unwrap();
        fs::write(cache_root.join(&key).join("ana.lock"), b"version = 1\n").unwrap();

        clean_global_command(cache_root).unwrap();

        assert!(cache_root.join("locks/default.lock").exists());
        assert!(!cache_root.join(&key).exists());
    }

    #[test]
    fn global_is_a_noop_on_an_empty_cache() {
        let cache = tempfile::tempdir().unwrap();
        let outcome = clean_global_command(cache.path()).unwrap();
        assert_eq!(outcome, CleanOutcome::default());
    }

    #[test]
    fn global_does_not_require_a_project_file() {
        // Unlike `clean_command`, `cache.path()` here is neither a
        // project root nor does it need to be one.
        let cache = tempfile::tempdir().unwrap();
        assert!(!ana_environment::project_file_exists(cache.path()));
        assert!(clean_global_command(cache.path()).is_ok());
    }

    #[test]
    fn global_does_not_touch_the_current_projects_environments() {
        let dir = project_root();
        let root = dir.path();
        fs::create_dir_all(root.join(".env/conda-meta")).unwrap();
        fs::write(root.join("ana.lock"), b"version = 1\n").unwrap();
        let cache = tempfile::tempdir().unwrap();

        clean_global_command(cache.path()).unwrap();

        assert!(root.join(".env").exists());
        assert!(root.join("ana.lock").exists());
    }
}
