//! Environments: the `lock_path`/`env_path` pair an invocation's
//! `--group` flags map to, plus the per-environment advisory lock path.
//! Deliberately no `selection.toml` sidecar: the 8-hex-character hash is
//! trusted blindly, accepting the theoretical collision risk rather than
//! carrying a verification sidecar.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uv_normalize::GroupName;

/// The resolved paths for one environment: what [`discover_paths`]
/// produces. Everything downstream (lockfile generation, environment
/// materialization) starts from here and never re-derives which paths a
/// `--group` selection maps to.
///
/// `lock_path`/`env_path` are readable directly, but construction is
/// [`discover_paths`]' job alone: the advisory-lock key is recorded here
/// at construction (not reverse-engineered from `lock_path`'s shape
/// later), so every path of an environment stays consistent no matter
/// what the project root happens to be named.
pub struct EnvironmentPaths {
    pub lock_path: PathBuf,
    pub env_path: PathBuf,
    /// The project root `discover_paths` resolved.
    root: PathBuf,
    /// The advisory-lock key: `Some(hash)` for a group environment,
    /// `None` for the default one.
    lock_key: Option<String>,
}

impl EnvironmentPaths {
    /// Path of this environment's advisory lock file:
    /// `<root>/.ana/locks/default.lock` for the default environment
    /// (`<root>/ana.lock`), or `<root>/.ana/locks/<hash>.lock` for a group
    /// environment (`<root>/.ana/<hash>/ana.lock`). Pure computation from
    /// the root and key recorded at construction. Keeping every
    /// environment's lock under one `.ana/locks/` directory means a
    /// single gitignore rule covers them all, and keeps them out of both
    /// the project root and `env_path` -- environment recreation may
    /// delete `env_path`, and deleting a lock file breaks mutual
    /// exclusion (two processes could hold flocks on different inodes of
    /// the same path).
    pub fn advisory_lock_path(&self) -> PathBuf {
        let key = self.lock_key.as_deref().unwrap_or("default");
        self.root
            .join(".ana")
            .join("locks")
            .join(format!("{key}.lock"))
    }

    /// Path of this environment's own lock file --
    /// `<env_path>/ana.lock` -- tracking what's actually materialized in
    /// this one environment right now, plus a `dirty` bit. Distinct from
    /// `lock_path` (the project's committed `ana.lock`, holding every
    /// platform's resolve-time data): this one is local, gitignored (it
    /// lives inside `env_path`, already covered by that ignore rule), and
    /// scoped to exactly the platform `env_path` was materialized for.
    pub fn env_lock_path(&self) -> PathBuf {
        self.env_path.join("ana.lock")
    }
}

/// The hash for an environment: SHA-256 over the normalized, sorted,
/// deduplicated, comma-joined group names, truncated to 8 hex characters.
/// `GroupName` is already normalized at parse time, so only the
/// sort/dedupe/join happens here.
pub fn environment_hash(groups: &[GroupName]) -> String {
    let signature = normalized_signature(groups);
    let digest = Sha256::digest(signature.as_bytes());
    let mut hex = String::with_capacity(8);
    for byte in &digest[..4] {
        use std::fmt::Write as _;
        // Writing two hex chars into a String never fails.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Map a group selection to its environment's paths. Pure computation --
/// nothing is read or written; directories are created by the downstream
/// writers (the advisory lock, the lock file splice, the cache) as
/// needed.
pub fn discover_paths(root: &Path, groups: &[GroupName]) -> EnvironmentPaths {
    if groups.is_empty() {
        return EnvironmentPaths {
            lock_path: root.join("ana.lock"),
            env_path: root.join(".env"),
            root: root.to_path_buf(),
            lock_key: None,
        };
    }
    let hash = environment_hash(groups);
    let dir = root.join(".ana").join(&hash);
    EnvironmentPaths {
        lock_path: dir.join("ana.lock"),
        env_path: dir.join("env"),
        root: root.to_path_buf(),
        lock_key: Some(hash),
    }
}

/// The normalized, sorted, deduplicated, comma-joined signature of a
/// group selection -- the string the environment hash is taken over.
fn normalized_signature(groups: &[GroupName]) -> String {
    let mut names: Vec<&str> = groups.iter().map(|group| group.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    names.join(",")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::str::FromStr;

    use super::*;

    fn groups(names: &[&str]) -> Vec<GroupName> {
        names
            .iter()
            .map(|name| GroupName::from_str(name).unwrap())
            .collect()
    }

    #[test]
    fn hash_matches_documented_vectors() {
        // Worked examples, pinned so the implementation can't silently
        // drift.
        assert_eq!(environment_hash(&groups(&["dev"])), "ef260e9a");
        assert_eq!(environment_hash(&groups(&["dev", "doc"])), "e62119cb");
        assert_eq!(environment_hash(&groups(&["doc", "other"])), "4a091557");
    }

    #[test]
    fn hash_is_order_and_repeat_invariant() {
        assert_eq!(
            environment_hash(&groups(&["doc", "dev"])),
            environment_hash(&groups(&["dev", "doc"])),
        );
        assert_eq!(
            environment_hash(&groups(&["dev", "dev"])),
            environment_hash(&groups(&["dev"])),
        );
    }

    #[test]
    fn default_environment_is_unhashed_root_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = discover_paths(dir.path(), &[]);
        assert_eq!(paths.lock_path, dir.path().join("ana.lock"));
        assert_eq!(paths.env_path, dir.path().join(".env"));
    }

    #[test]
    fn group_environment_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = discover_paths(dir.path(), &groups(&["doc", "dev"]));
        let expected_dir = dir.path().join(".ana").join("e62119cb");
        assert_eq!(paths.lock_path, expected_dir.join("ana.lock"));
        assert_eq!(paths.env_path, expected_dir.join("env"));
        // Discovery is pure: nothing is created on disk.
        assert!(!dir.path().join(".ana").exists());
    }

    #[test]
    fn advisory_lock_paths_per_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let default = discover_paths(root, &[]);
        assert_eq!(
            default.advisory_lock_path(),
            root.join(".ana/locks/default.lock")
        );

        let hashed = discover_paths(root, &groups(&["dev"]));
        assert_eq!(
            hashed.advisory_lock_path(),
            root.join(".ana/locks/ef260e9a.lock")
        );
    }

    #[test]
    fn env_lock_paths_per_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let default = discover_paths(root, &[]);
        assert_eq!(default.env_lock_path(), root.join(".env/ana.lock"));

        let hashed = discover_paths(root, &groups(&["dev"]));
        assert_eq!(
            hashed.env_lock_path(),
            root.join(".ana/ef260e9a/env/ana.lock")
        );
    }

    /// The lock key is recorded at construction, never sniffed from
    /// `lock_path`'s shape: a project root *inside* a directory named
    /// `.ana` must not make the default environment look like a group
    /// environment (which would put its lock at `<parent>/locks/<dir>.lock`,
    /// outside the project root).
    #[test]
    fn advisory_lock_path_is_deterministic_under_an_ana_named_parent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".ana").join("myproj");
        std::fs::create_dir_all(&root).unwrap();

        let paths = discover_paths(&root, &[]);
        assert_eq!(
            paths.advisory_lock_path(),
            root.join(".ana/locks/default.lock")
        );
    }
}
