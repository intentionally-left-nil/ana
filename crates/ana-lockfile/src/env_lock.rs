//! The environment lock file: `<env_path>/ana.lock` -- what's actually
//! materialized in this one environment right now, plus a `dirty` bit
//! marking a possibly-interrupted reconcile.
//!
//! Reuses `crate::lock_file`'s section parse/serialize for the
//! `platforms` part -- same TOML shape as the project's own `ana.lock`,
//! since this file also holds a [`crate::PlatformSection`] -- adding one
//! extra top-level `dirty: bool` key to the same document.
//!
//! Unlike `ana.lock`, this file is never shared across a splice: exactly
//! one process (the one holding this environment's advisory lock) ever
//! reads or writes it, and it covers exactly one platform (the one
//! `env_path` was materialized for), so every write here is a full-
//! document overwrite, never a read-modify-splice.

use std::fs;
use std::path::Path;
use std::str::FromStr;

use rattler_conda_types::Platform;
use toml_edit::{DocumentMut, Item, Table, Value};

use ana_fs_util::write_atomic;

use crate::error::Error;
use crate::lock_file::{
    check_version, parse_section, section_to_item, PlatformSection, LOCK_FILE_VERSION,
};

/// The env lock file's parsed content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvLock {
    /// Set just before a reconcile starts, cleared (together with
    /// rewriting `section` to match) only once that reconcile succeeds --
    /// so a crash mid-install leaves this `true`, and the next `ana run`
    /// wipes `env_path` recursively rather than trusting a prefix that
    /// might be half-installed.
    pub dirty: bool,
    /// The target platform's section as of the last successful
    /// reconcile. `None` for a first install (no env lock yet), a
    /// corrupt/unreadable file, or right after a dirty-triggered wipe.
    pub section: Option<PlatformSection>,
}

impl EnvLock {
    /// Read `env_lock_path`, keeping only `platform`'s section (the file
    /// covers exactly one platform in practice, but reading is scoped
    /// defensively rather than assumed). Missing, unparseable, or a
    /// version this binary doesn't understand all come back as
    /// [`EnvLock::default`] (`{ dirty: false, section: None }`) --
    /// algorithm step 1: this file is local and gitignored, never shared,
    /// so any doubt about its content is safe to treat as "nothing
    /// installed yet," never an error.
    pub fn read(env_lock_path: &Path, platform: Platform) -> Self {
        let Ok(text) = fs::read_to_string(env_lock_path) else {
            return Self::default();
        };
        let Ok(doc) = DocumentMut::from_str(&text) else {
            return Self::default();
        };
        if check_version(&doc).is_err() {
            return Self::default();
        }
        let dirty = doc.get("dirty").and_then(Item::as_bool).unwrap_or(false);
        let section = doc
            .get("platforms")
            .and_then(Item::as_table)
            .and_then(|platforms| platforms.get(platform.as_str()))
            .and_then(|item| parse_section(platform.as_str(), item).ok());
        Self { dirty, section }
    }

    /// Overwrite `env_lock_path` wholesale: `dirty`, plus `section` (if
    /// any) under `platform`'s key. Always a full rewrite, never a
    /// splice -- see this module's docs for why there is no foreign
    /// content to preserve here, unlike the committed `ana.lock`.
    ///
    /// Callers decide whether a failure here is fatal: the pre-install
    /// `dirty = true` write is expected to propagate (via `?`) --
    /// without it landing, a crash during the install that follows can't
    /// be told apart from "never started" -- while the post-install
    /// `{ dirty: false, ... }` write is best-effort (the caller ignores
    /// the `Result`), since losing it only costs one extra dirty-wipe on
    /// the next invocation.
    pub fn write(
        env_lock_path: &Path,
        platform: Platform,
        dirty: bool,
        section: Option<&PlatformSection>,
    ) -> Result<(), Error> {
        let mut doc = DocumentMut::new();
        doc["version"] = Item::Value(Value::Integer(toml_edit::Formatted::new(LOCK_FILE_VERSION)));
        doc["dirty"] = Item::Value(Value::Boolean(toml_edit::Formatted::new(dirty)));
        if let Some(section) = section {
            let mut platforms = Table::new();
            platforms[platform.as_str()] = section_to_item(section);
            doc["platforms"] = Item::Table(platforms);
        }
        write_atomic(env_lock_path, doc.to_string().as_bytes()).map_err(|source| Error::Write {
            path: env_lock_path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use rattler_conda_types::{PackageName, PackageRecord, Version};

    use super::*;
    use crate::lock_file::LockedRequirement;

    const PLATFORM: Platform = Platform::Linux64;

    fn package(name: &str, version: &str) -> rattler_conda_types::RepoDataRecord {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            "py312h1234567_0".to_string(),
        );
        record.subdir = PLATFORM.as_str().to_string();
        let identifier = rattler_conda_types::package::DistArchiveIdentifier::try_from_filename(
            &format!("{name}-{version}-py312h1234567_0.conda"),
        )
        .unwrap();
        rattler_conda_types::RepoDataRecord {
            package_record: record,
            identifier,
            url: url::Url::parse(&format!(
                "https://repo.anaconda.com/pkgs/main/linux-64/{name}-{version}-py312h1234567_0.conda"
            ))
            .unwrap(),
            channel: Some("https://repo.anaconda.com/pkgs/main".to_string()),
        }
    }

    fn section() -> PlatformSection {
        PlatformSection {
            requirements: vec![LockedRequirement {
                matchspec: "numpy >=1.20".to_string(),
                source: "runtime".to_string(),
            }],
            packages: vec![package("numpy", "1.23.5")],
            channels_digest: "deadbeef".to_string(),
        }
    }

    #[test]
    fn missing_file_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");
        assert_eq!(EnvLock::read(&path, PLATFORM), EnvLock::default());
    }

    #[test]
    fn corrupt_file_reads_as_default_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");
        fs::write(&path, "not [toml").unwrap();
        assert_eq!(EnvLock::read(&path, PLATFORM), EnvLock::default());
    }

    #[test]
    fn unsupported_version_reads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");
        fs::write(&path, "version = 999999\n").unwrap();
        assert_eq!(EnvLock::read(&path, PLATFORM), EnvLock::default());
    }

    #[test]
    fn write_then_read_round_trips_dirty_and_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");

        EnvLock::write(&path, PLATFORM, false, Some(&section())).unwrap();
        let read = EnvLock::read(&path, PLATFORM);
        assert!(!read.dirty);
        assert_eq!(read.section, Some(section()));
    }

    #[test]
    fn dirty_with_no_section_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");

        EnvLock::write(&path, PLATFORM, true, None).unwrap();
        let read = EnvLock::read(&path, PLATFORM);
        assert!(read.dirty);
        assert_eq!(read.section, None);
    }

    #[test]
    fn write_is_a_full_overwrite_not_a_splice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");

        EnvLock::write(&path, PLATFORM, true, Some(&section())).unwrap();
        EnvLock::write(&path, PLATFORM, false, None).unwrap();

        let read = EnvLock::read(&path, PLATFORM);
        assert!(!read.dirty);
        assert_eq!(
            read.section, None,
            "the second write replaces the whole document, including the section"
        );
    }

    #[test]
    fn read_ignores_a_different_platforms_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ana.lock");

        EnvLock::write(&path, Platform::OsxArm64, false, Some(&section())).unwrap();
        let read = EnvLock::read(&path, PLATFORM);
        assert_eq!(read.section, None, "env_path is scoped to one platform");
    }
}
