//! Environments: the `lock_path`/`env_path` pair an invocation resolves
//! to, plus the per-environment advisory lock path -- given an
//! [`EnvironmentLayout`] describing where it's rooted and how its own
//! subdirectory (if any) is named. Deliberately no `selection.toml`
//! sidecar: an [`EnvironmentKey`] is trusted blindly, accepting the
//! theoretical collision risk rather than carrying a verification
//! sidecar.

use std::path::{Path, PathBuf};

use crate::key::EnvironmentKey;

/// Where one environment's paths are rooted, and how its own
/// subdirectory (if any) is named. The *policy* of which constructor an
/// [`EnvironmentKey`] should come from for a given invocation lives above
/// this crate; this type only knows how to turn an already-decided key
/// into paths.
#[derive(Debug, Clone)]
pub enum EnvironmentLayout<'a> {
    /// No `--group`/ad hoc requirements: the project's own files
    /// directly -- `<root>/ana.lock`, `<root>/.env`.
    ProjectDefault { root: &'a Path },
    /// A keyed, project-scoped declaration (e.g. a `--group` selection):
    /// `<root>/.ana/<key>/`.
    ProjectKeyed { root: &'a Path, key: EnvironmentKey },
    /// A keyed declaration with no project root at all (CLI-declared, or
    /// later a script): `<cache_root>/<key>/`.
    Global {
        cache_root: &'a Path,
        key: EnvironmentKey,
    },
}

/// The owned mirror of [`EnvironmentLayout`] that [`EnvironmentPaths`]
/// keeps for itself, so a caller of [`discover`] doesn't have to keep the
/// borrowed layout alive.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Layout {
    ProjectDefault { root: PathBuf },
    ProjectKeyed { root: PathBuf, key: String },
    Global { cache_root: PathBuf, key: String },
}

/// The resolved paths for one environment: what [`discover`] produces.
/// Everything downstream (lockfile generation, environment
/// materialization) starts from here and never re-derives which paths an
/// invocation maps to.
///
/// `lock_path`/`env_path` are readable directly, but construction is
/// [`discover`]'s job alone: the layout is recorded here at construction,
/// not reverse-engineered from `lock_path`'s shape later.
pub struct EnvironmentPaths {
    pub lock_path: PathBuf,
    pub env_path: PathBuf,
    layout: Layout,
}

impl EnvironmentPaths {
    /// Path of this environment's advisory lock file. Every environment
    /// rooted at the same project (or the same global cache) keeps its
    /// lock under one shared `locks/` directory, so a single gitignore
    /// rule covers them all, and locks stay out of `env_path` -- deleting
    /// a lock file breaks mutual exclusion (two processes could hold
    /// flocks on different inodes of the same path).
    pub fn advisory_lock_path(&self) -> PathBuf {
        self.locks_dir().join(format!("{}.lock", self.lock_key()))
    }

    /// Path of this environment's own lock file -- `<env_path>/ana.lock`
    /// -- tracking what's actually materialized in this one environment
    /// right now, plus a `dirty` bit. Distinct from `lock_path` (a
    /// project's committed `ana.lock`, holding every platform's
    /// resolve-time data): this one is local, gitignored, and scoped to
    /// exactly the platform `env_path` was materialized for.
    pub fn env_lock_path(&self) -> PathBuf {
        self.env_path.join("ana.lock")
    }

    /// This environment's own directory, for a keyed layout -- `None` for
    /// [`EnvironmentLayout::ProjectDefault`], whose lock and env are the
    /// project root's own files/subdirectories, not one dedicated
    /// directory that could be removed wholesale.
    ///
    /// A keyed environment's `ana.lock` is treated as ephemeral,
    /// disposable state, so `ana clean` removes this whole directory,
    /// `ana.lock` included, not just `env_path`.
    pub fn group_dir(&self) -> Option<PathBuf> {
        match &self.layout {
            Layout::ProjectDefault { .. } => None,
            Layout::ProjectKeyed { root, key } => Some(root.join(".ana").join(key)),
            Layout::Global { cache_root, key } => Some(cache_root.join(key)),
        }
    }

    /// The directory holding every keyed environment sharing this one's
    /// root (or global cache), `locks/` excluded -- what `ana clean`
    /// enumerates to find every environment it's ever materialized,
    /// without spelling `.ana` itself.
    pub fn keyed_container(&self) -> PathBuf {
        match &self.layout {
            Layout::ProjectDefault { root } | Layout::ProjectKeyed { root, .. } => {
                root.join(".ana")
            }
            Layout::Global { cache_root, .. } => cache_root.clone(),
        }
    }

    /// The directory holding every advisory lock sharing this one's root
    /// (or global cache).
    pub fn locks_dir(&self) -> PathBuf {
        match &self.layout {
            Layout::ProjectDefault { .. } | Layout::ProjectKeyed { .. } => {
                self.keyed_container().join("locks")
            }
            Layout::Global { cache_root, .. } => cache_root.join("locks"),
        }
    }

    /// The advisory lock's own file stem: `"default"` for the
    /// unkeyed layout, the key itself for any keyed one.
    fn lock_key(&self) -> &str {
        match &self.layout {
            Layout::ProjectDefault { .. } => "default",
            Layout::ProjectKeyed { key, .. } | Layout::Global { key, .. } => key,
        }
    }
}

/// Map a layout to its environment's paths. Pure computation -- nothing
/// is read or written; directories are created by the downstream writers
/// (the advisory lock, the lock file splice, the cache) as needed.
pub fn discover(layout: EnvironmentLayout<'_>) -> EnvironmentPaths {
    match layout {
        EnvironmentLayout::ProjectDefault { root } => EnvironmentPaths {
            lock_path: root.join("ana.lock"),
            env_path: root.join(".env"),
            layout: Layout::ProjectDefault {
                root: root.to_path_buf(),
            },
        },
        EnvironmentLayout::ProjectKeyed { root, key } => {
            let dir = root.join(".ana").join(key.as_str());
            EnvironmentPaths {
                lock_path: dir.join("ana.lock"),
                env_path: dir.join("env"),
                layout: Layout::ProjectKeyed {
                    root: root.to_path_buf(),
                    key: key.as_str().to_string(),
                },
            }
        }
        EnvironmentLayout::Global { cache_root, key } => {
            let dir = cache_root.join(key.as_str());
            EnvironmentPaths {
                lock_path: dir.join("ana.lock"),
                env_path: dir.join("env"),
                layout: Layout::Global {
                    cache_root: cache_root.to_path_buf(),
                    key: key.as_str().to_string(),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn key(name: &str) -> EnvironmentKey {
        EnvironmentKey::from_symbolic_names(&[name])
    }

    #[test]
    fn default_layout_is_unkeyed_root_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = discover(EnvironmentLayout::ProjectDefault { root: dir.path() });
        assert_eq!(paths.lock_path, dir.path().join("ana.lock"));
        assert_eq!(paths.env_path, dir.path().join(".env"));
    }

    #[test]
    fn project_keyed_layout_paths() {
        let dir = tempfile::tempdir().unwrap();
        let key = EnvironmentKey::from_symbolic_names(&["doc", "dev"]);
        let paths = discover(EnvironmentLayout::ProjectKeyed {
            root: dir.path(),
            key,
        });
        let expected_dir = dir.path().join(".ana").join("e62119cb");
        assert_eq!(paths.lock_path, expected_dir.join("ana.lock"));
        assert_eq!(paths.env_path, expected_dir.join("env"));
        // Discovery is pure: nothing is created on disk.
        assert!(!dir.path().join(".ana").exists());
    }

    #[test]
    fn global_layout_paths_have_no_ana_segment() {
        let dir = tempfile::tempdir().unwrap();
        let key = EnvironmentKey::from_content(&["numpy"]);
        let paths = discover(EnvironmentLayout::Global {
            cache_root: dir.path(),
            key: key.clone(),
        });
        let expected_dir = dir.path().join(key.as_str());
        assert_eq!(paths.lock_path, expected_dir.join("ana.lock"));
        assert_eq!(paths.env_path, expected_dir.join("env"));
        assert_eq!(
            paths.advisory_lock_path(),
            dir.path()
                .join("locks")
                .join(format!("{}.lock", key.as_str()))
        );
    }

    #[test]
    fn advisory_lock_paths_per_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let default = discover(EnvironmentLayout::ProjectDefault { root });
        assert_eq!(
            default.advisory_lock_path(),
            root.join(".ana/locks/default.lock")
        );

        let keyed = discover(EnvironmentLayout::ProjectKeyed {
            root,
            key: key("dev"),
        });
        assert_eq!(
            keyed.advisory_lock_path(),
            root.join(".ana/locks/ef260e9a.lock")
        );
    }

    #[test]
    fn env_lock_paths_per_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let default = discover(EnvironmentLayout::ProjectDefault { root });
        assert_eq!(default.env_lock_path(), root.join(".env/ana.lock"));

        let keyed = discover(EnvironmentLayout::ProjectKeyed {
            root,
            key: key("dev"),
        });
        assert_eq!(
            keyed.env_lock_path(),
            root.join(".ana/ef260e9a/env/ana.lock")
        );
    }

    /// The layout is recorded at construction, never sniffed from
    /// `lock_path`'s shape: a project root *inside* a directory named
    /// `.ana` must not make the default layout look like a keyed one.
    #[test]
    fn advisory_lock_path_is_deterministic_under_an_ana_named_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".ana").join("myproj");
        std::fs::create_dir_all(&root).unwrap();

        let paths = discover(EnvironmentLayout::ProjectDefault { root: &root });
        assert_eq!(
            paths.advisory_lock_path(),
            root.join(".ana/locks/default.lock")
        );
    }

    #[test]
    fn default_layout_has_no_group_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = discover(EnvironmentLayout::ProjectDefault { root: dir.path() });
        assert_eq!(paths.group_dir(), None);
    }

    #[test]
    fn project_keyed_group_dir_is_its_ana_key_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = discover(EnvironmentLayout::ProjectKeyed {
            root,
            key: key("dev"),
        });
        assert_eq!(paths.group_dir(), Some(root.join(".ana/ef260e9a")));
    }

    #[test]
    fn global_group_dir_has_no_ana_segment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = discover(EnvironmentLayout::Global {
            cache_root: root,
            key: key("dev"),
        });
        assert_eq!(paths.group_dir(), Some(root.join("ef260e9a")));
    }

    #[test]
    fn keyed_container_and_locks_dir_per_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let default = discover(EnvironmentLayout::ProjectDefault { root });
        assert_eq!(default.keyed_container(), root.join(".ana"));
        assert_eq!(default.locks_dir(), root.join(".ana/locks"));

        let keyed = discover(EnvironmentLayout::ProjectKeyed {
            root,
            key: key("dev"),
        });
        assert_eq!(keyed.keyed_container(), root.join(".ana"));
        assert_eq!(keyed.locks_dir(), root.join(".ana/locks"));

        let global = discover(EnvironmentLayout::Global {
            cache_root: root,
            key: key("dev"),
        });
        assert_eq!(global.keyed_container(), root.to_path_buf());
        assert_eq!(global.locks_dir(), root.join("locks"));
    }

    #[test]
    fn project_keyed_from_raw_matches_the_key_it_was_read_from() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let from_groups = discover(EnvironmentLayout::ProjectKeyed {
            root,
            key: EnvironmentKey::from_symbolic_names(&["dev", "doc"]),
        });
        let from_raw = discover(EnvironmentLayout::ProjectKeyed {
            root,
            key: EnvironmentKey::from_raw("e62119cb"),
        });

        assert_eq!(from_raw.lock_path, from_groups.lock_path);
        assert_eq!(from_raw.env_path, from_groups.env_path);
        assert_eq!(
            from_raw.advisory_lock_path(),
            from_groups.advisory_lock_path()
        );
        assert_eq!(from_raw.group_dir(), from_groups.group_dir());
    }
}
