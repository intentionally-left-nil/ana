//! The `ana.lock` file: model, TOML parsing, and the
//! re-read/splice/atomic-write sequence used by every mode that resolves.
//! The same section parse/serialize functions are reused by
//! `crate::env_lock` for `<env_path>/ana.lock`'s `platforms` part.
//!
//! Format: one `[platforms.<subdir>]` table per solved platform, each
//! holding only real, resolve-time data -- the canonical matchspecs the
//! platform was solved from (including a `python` entry derived from
//! `requires-python`, if the project declares one -- see `crate::matchspec`)
//! and the full resolved [`PackageRecord`] set. No staleness bookkeeping
//! (hashes) lives here at all -- staleness is a live set-diff against
//! `pyproject.toml`, not a cached digest. One file per environment, so the
//! `(environment, ...)` half of rattler's partition key is the file's
//! location, not a key in it.
//!
//! ```toml
//! version = 1
//!
//! [[platforms.linux-64.requirements]]
//! matchspec = "numpy >=1.20"
//! source = "runtime"
//!
//! [[platforms.linux-64.requirements]]
//! matchspec = "python >=3.9"
//! source = "requires-python"
//!
//! [[platforms.linux-64.packages]]
//! name = "numpy"
//! version = "1.23.5"
//! fn = "numpy-1.23.5-py312h1234567_0.conda"
//! url = "https://repo.anaconda.com/pkgs/main/linux-64/numpy-1.23.5-py312h1234567_0.conda"
//! channel = "https://repo.anaconda.com/pkgs/main"
//! # ... full RepoDataRecord fields (a PackageRecord, plus `fn`/`url`/`channel`)
//! ```
//!
//! Three deliberate parsing decisions:
//!
//! - **The format is versioned.** The top-level `version` integer
//!   ([`LOCK_FILE_VERSION`]) is the escape hatch for future incompatible
//!   schema changes: a file newer than this binary understands is a hard
//!   parse error everywhere (surfacing as `Error::CorruptLock`), never
//!   silently trusted, and a splice never writes into it. Absent reads as
//!   `1`, so files written before versioning existed keep working.
//! - **Unknown platform keys are skipped, not rejected.** A lock written by
//!   a newer `ana` that supports subdirs this one doesn't know still parses;
//!   the unknown section is simply absent from the model. Splicing works on
//!   the raw document, so such sections survive a resolve untouched.
//! - **No semantic-completeness validation beyond shape.** A section that
//!   parses is used as-is, even with empty `requirements`/`packages` (a
//!   legitimately empty environment looks exactly like that). "Requirements
//!   present, packages empty" is *not* a regen trigger: with atomic writes a
//!   half-written section is structurally impossible, so that state only
//!   arises from hand-editing, which is out of scope. Both arrays may
//!   simply be absent when empty -- an empty array-of-tables has no TOML
//!   rendering.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use rattler_conda_types::{Platform, RepoDataRecord};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use ana_fs_util::write_atomic;

use crate::error::Error;

/// The current `ana.lock` format version, written as the top-level
/// `version` key by [`splice_sections`]. Bump this when the schema changes
/// incompatibly; readers reject anything newer (see the module docs'
/// versioning bullet). A file with no `version` key predates versioning
/// and reads as `1`.
pub const LOCK_FILE_VERSION: i64 = 1;

/// One requirement a platform section was solved from: the canonical
/// matchspec string ([`rattler_conda_types::MatchSpec`]'s `Display`), plus
/// where in `pyproject.toml` it came from (`source` -- `"runtime"`,
/// `"group:<name>"`, or `"requires-python"` for the `python` matchspec
/// `requires-python` derives; informational only, never part of the
/// staleness comparison, which is a pure set diff on matchspec strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedRequirement {
    pub matchspec: String,
    pub source: String,
}

/// One platform's section of `ana.lock`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformSection {
    pub requirements: Vec<LockedRequirement>,
    /// Full resolved records, so a future re-solve can feed them back to
    /// the solver as preference hints without re-fetching metadata, and
    /// so an install has a `url` to fetch (or re-verify) each record from
    /// -- a bare `PackageRecord` alone doesn't carry that.
    pub packages: Vec<RepoDataRecord>,
}

impl PlatformSection {
    /// Sort `requirements` (by matchspec string, then source) and
    /// `packages` (by [`RepoDataRecord`]'s `Ord`, which delegates to its
    /// own `package_record`'s `Ord`) into the canonical order every
    /// comparison and write in this crate assumes -- so "same content,
    /// different in-memory order" never looks like a difference.
    /// Idempotent.
    pub fn canonicalize(&mut self) {
        self.requirements
            .sort_by(|a, b| a.matchspec.cmp(&b.matchspec).then(a.source.cmp(&b.source)));
        self.packages.sort();
    }
}

/// A parsed `ana.lock`: every recognizable platform section, keyed by
/// platform. Unknown-platform sections are preserved on disk by
/// [`splice_section`] but absent here (see this module's docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockFile {
    pub platforms: BTreeMap<Platform, PlatformSection>,
}

/// Why a lock file's text failed to parse. Carries enough context to
/// report, but the algorithm itself treats every one of these identically:
/// as a regeneration trigger, never a hard failure.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct LockParseError(String);

/// Enforce the format version on an already-parsed document: absent reads
/// as [`LOCK_FILE_VERSION`] (pre-versioning files), anything newer than
/// this binary understands is rejected so an old `ana` never silently
/// trusts (or splices into) a newer file's schema. `pub(crate)`: reused by
/// `crate::env_lock`, which shares this crate's document-versioning
/// policy for `<env_path>/ana.lock` too.
pub(crate) fn check_version(doc: &DocumentMut) -> Result<(), LockParseError> {
    let Some(item) = doc.get("version") else {
        return Ok(());
    };
    let version = item
        .as_integer()
        .ok_or_else(|| LockParseError("`version` is not an integer".to_string()))?;
    if !(1..=LOCK_FILE_VERSION).contains(&version) {
        return Err(LockParseError(format!(
            "unsupported version {version} (newest supported: {LOCK_FILE_VERSION}); upgrade ana"
        )));
    }
    Ok(())
}

impl LockFile {
    /// Parse lock file text. Unknown platform keys are skipped (see module
    /// docs); structurally wrong content is a [`LockParseError`].
    pub fn parse(text: &str) -> Result<Self, LockParseError> {
        let doc = DocumentMut::from_str(text)
            .map_err(|err| LockParseError(format!("invalid TOML: {err}")))?;
        check_version(&doc)?;

        let mut platforms = BTreeMap::new();
        let Some(platforms_item) = doc.get("platforms") else {
            return Ok(Self { platforms });
        };
        let platforms_table = platforms_item
            .as_table()
            .ok_or_else(|| LockParseError("`platforms` is not a table".to_string()))?;

        for (key, item) in platforms_table.iter() {
            let Ok(platform) = Platform::from_str(key) else {
                continue;
            };
            let section = parse_section(key, item)?;
            platforms.insert(platform, section);
        }
        Ok(Self { platforms })
    }

    /// Read and parse `path`. `Ok(None)` means the file doesn't exist; a
    /// parse failure is `Err` -- callers decide whether that means
    /// "regenerate" (it does, in every current mode) or something else.
    pub fn read(path: &Path) -> Result<Option<Self>, Error> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::Read {
                    path: path.to_path_buf(),
                    source: err,
                });
            }
        };
        Self::parse(&text).map(Some).map_err(|err| Error::Read {
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, err.to_string()),
        })
    }
}

/// Parse only `platform`'s section out of lock file text, for the modes
/// that never look at any other section -- they shouldn't pay to
/// deserialize every foreign platform's package records. A syntactically
/// invalid document is a [`LockParseError`] (callers turn it into
/// [`Error::CorruptLock`]); a section that is structurally wrong comes
/// back as `None`, the same "treat as missing and regenerate" policy the
/// full parse's callers apply, but scoped so one platform's hand-edit
/// damage can't force another platform's section to regenerate.
pub(crate) fn parse_platform_section(
    text: &str,
    platform: Platform,
) -> Result<Option<PlatformSection>, LockParseError> {
    let doc = DocumentMut::from_str(text)
        .map_err(|err| LockParseError(format!("invalid TOML: {err}")))?;
    check_version(&doc)?;
    let section = doc
        .get("platforms")
        .and_then(Item::as_table)
        .and_then(|platforms| platforms.get(platform.as_str()));
    match section {
        None => Ok(None),
        Some(item) => Ok(parse_section(platform.as_str(), item).ok()),
    }
}

/// Parse one `[platforms.<subdir>]` table into a [`PlatformSection`].
/// `pub(crate)`: reused by `crate::env_lock` for the env lock file's own
/// (single) platform section, which is the same shape.
pub(crate) fn parse_section(key: &str, item: &Item) -> Result<PlatformSection, LockParseError> {
    let err = |what: &str| LockParseError(format!("platforms.{key}: {what}"));

    let table = item
        .as_table()
        .ok_or_else(|| err("section is not a table"))?;

    let mut requirements = Vec::new();
    if let Some(item) = table.get("requirements") {
        let entries = item
            .as_array_of_tables()
            .ok_or_else(|| err("`requirements` is not an array of tables"))?;
        for entry in entries {
            let matchspec = entry
                .get("matchspec")
                .and_then(Item::as_str)
                .ok_or_else(|| err("requirement is missing a string `matchspec`"))?;
            let source = entry
                .get("source")
                .and_then(Item::as_str)
                .unwrap_or("runtime");
            requirements.push(LockedRequirement {
                matchspec: matchspec.to_string(),
                source: source.to_string(),
            });
        }
    }

    let mut packages = Vec::new();
    if let Some(item) = table.get("packages") {
        let entries = item
            .as_array_of_tables()
            .ok_or_else(|| err("`packages` is not an array of tables"))?;
        for entry in entries {
            let json = table_to_json(entry);
            let record = serde_json::from_value::<RepoDataRecord>(json).map_err(|serde_err| {
                err(&format!("package record does not deserialize: {serde_err}"))
            })?;
            packages.push(record);
        }
    }

    Ok(PlatformSection {
        requirements,
        packages,
    })
}

/// Re-read `lock_path`, replace only `platform`'s section with `section`,
/// and atomically write the result back. Every other key in the document --
/// other platforms' sections, unknown future keys, comments, formatting --
/// is preserved exactly, which is what makes concurrent resolves for
/// *different* platforms safe to serialize through the advisory lock: the
/// re-read happens here, inside the critical section, immediately before
/// the write, so a section another process wrote while we were solving is
/// spliced *around*, never reverted to our stale in-memory snapshot.
///
/// A syntactically unparseable existing file is [`Error::CorruptLock`],
/// never silently replaced: the file is committed and shared, so
/// discarding it would destroy every other platform's section. Only a
/// *missing* file starts a fresh document.
pub(crate) fn splice_section(
    lock_path: &Path,
    platform: Platform,
    section: &PlatformSection,
) -> Result<(), Error> {
    splice_sections(lock_path, &[(platform, section.clone())])
}

/// [`splice_section`] for several platforms at once: one read, one parse,
/// one atomic write, however many sections are replaced. `check --fix`
/// uses this so P stale platforms don't cost P full-file rewrites.
pub(crate) fn splice_sections(
    lock_path: &Path,
    sections: &[(Platform, PlatformSection)],
) -> Result<(), Error> {
    let mut doc = match fs::read_to_string(lock_path) {
        Ok(text) => {
            let doc = DocumentMut::from_str(&text).map_err(|err| Error::CorruptLock {
                path: lock_path.to_path_buf(),
                reason: err.to_string(),
            })?;
            // Never splice into a file whose schema this binary doesn't
            // understand -- the new section's semantics could differ from
            // what the rest of the file assumes.
            check_version(&doc).map_err(|err| Error::CorruptLock {
                path: lock_path.to_path_buf(),
                reason: err.to_string(),
            })?;
            doc
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => DocumentMut::new(),
        Err(err) => {
            return Err(Error::Read {
                path: lock_path.to_path_buf(),
                source: err,
            });
        }
    };

    // Stamp the format version (before `platforms`, so a fresh file renders
    // it first). An existing file that passed `check_version` either already
    // has the key or predates versioning -- either way, don't touch it.
    if doc.get("version").is_none() {
        doc["version"] = Item::Value(Value::Integer(toml_edit::Formatted::new(LOCK_FILE_VERSION)));
    }
    if doc.get("platforms").and_then(Item::as_table).is_none() {
        doc["platforms"] = Item::Table(Table::new());
    }
    for (platform, section) in sections {
        doc["platforms"][platform.as_str()] = section_to_item(section);
    }

    write_atomic(lock_path, doc.to_string().as_bytes()).map_err(|err| Error::Write {
        path: lock_path.to_path_buf(),
        source: err,
    })
}

/// Build the TOML for one platform section: `requirements` and `packages`
/// arrays of tables (omitted when empty -- an empty array-of-tables has no
/// TOML rendering, and absence parses back to an empty list, so this
/// round-trips). `pub(crate)`: reused by `crate::env_lock`.
pub(crate) fn section_to_item(section: &PlatformSection) -> Item {
    let mut table = Table::new();

    if !section.requirements.is_empty() {
        let mut entries = ArrayOfTables::new();
        for req in &section.requirements {
            let mut entry = Table::new();
            entry["matchspec"] = Item::Value(Value::String(toml_edit::Formatted::new(
                req.matchspec.clone(),
            )));
            entry["source"] =
                Item::Value(Value::String(toml_edit::Formatted::new(req.source.clone())));
            entries.push(entry);
        }
        table["requirements"] = Item::ArrayOfTables(entries);
    }

    if !section.packages.is_empty() {
        let mut entries = ArrayOfTables::new();
        for package in &section.packages {
            entries.push(package_to_table(package));
        }
        table["packages"] = Item::ArrayOfTables(entries);
    }

    Item::Table(table)
}

/// A [`RepoDataRecord`] as a TOML table, via its `Serialize` impl and a
/// JSON bridge: `serde_json` gives us the record's canonical field set
/// (the flattened `PackageRecord` fields plus `fn`/`url`/`channel`) as
/// plain data, which is then transcribed field-by-field. Nested objects
/// become inline tables so each `[[packages]]` entry stays self-contained.
fn package_to_table(package: &RepoDataRecord) -> Table {
    let json = serde_json::to_value(package).unwrap_or(serde_json::Value::Null);
    let mut table = Table::new();
    if let serde_json::Value::Object(map) = json {
        for (key, value) in map {
            if let Some(item) = json_to_item(&value) {
                table[&key] = item;
            }
        }
    }
    table
}

/// One JSON value as a TOML item. `Null` (and numbers not representable as
/// TOML integers/floats) yield `None`, and the caller omits the key --
/// `PackageRecord`'s own serialization already skips `None` fields, so a
/// `Null` here only arises from hand-constructed records.
fn json_to_item(value: &serde_json::Value) -> Option<Item> {
    json_to_value(value).map(Item::Value)
}

fn json_to_value(value: &serde_json::Value) -> Option<Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(Value::Boolean(toml_edit::Formatted::new(*b))),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Integer(toml_edit::Formatted::new(i)))
            } else {
                n.as_f64()
                    .map(|f| Value::Float(toml_edit::Formatted::new(f)))
            }
        }
        serde_json::Value::String(s) => Some(Value::String(toml_edit::Formatted::new(s.clone()))),
        serde_json::Value::Array(values) => {
            let mut array = Array::new();
            for value in values {
                if let Some(value) = json_to_value(value) {
                    array.push(value);
                }
            }
            Some(Value::Array(array))
        }
        serde_json::Value::Object(map) => {
            let mut table = InlineTable::new();
            for (key, value) in map {
                if let Some(value) = json_to_value(value) {
                    table.insert(key, value);
                }
            }
            Some(Value::InlineTable(table))
        }
    }
}

/// A parsed TOML table as a JSON object -- the inverse bridge of
/// [`package_to_table`], feeding `serde_json::from_value::<RepoDataRecord>`.
/// TOML datetimes (which this crate never writes) degrade to their string
/// form.
fn table_to_json(table: &Table) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, item) in table.iter() {
        map.insert(key.to_string(), item_to_json(item));
    }
    serde_json::Value::Object(map)
}

fn item_to_json(item: &Item) -> serde_json::Value {
    match item {
        Item::None => serde_json::Value::Null,
        Item::Value(value) => value_to_json(value),
        Item::Table(table) => table_to_json(table),
        Item::ArrayOfTables(entries) => {
            serde_json::Value::Array(entries.iter().map(table_to_json).collect())
        }
    }
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.value().clone()),
        Value::Integer(i) => serde_json::Value::Number((*i.value()).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f.value())
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Boolean(b) => serde_json::Value::Bool(*b.value()),
        Value::Datetime(dt) => serde_json::Value::String(dt.value().to_string()),
        Value::Array(array) => serde_json::Value::Array(array.iter().map(value_to_json).collect()),
        Value::InlineTable(table) => {
            let mut map = serde_json::Map::new();
            for (key, value) in table.iter() {
                map.insert(key.to_string(), value_to_json(value));
            }
            serde_json::Value::Object(map)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use rattler_conda_types::{PackageName, PackageRecord, Version};

    use super::*;

    fn package(name: &str, version: &str) -> RepoDataRecord {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            "py312h1234567_0".to_string(),
        );
        record.subdir = "linux-64".to_string();
        let identifier = rattler_conda_types::package::DistArchiveIdentifier::try_from_filename(
            &format!("{name}-{version}-py312h1234567_0.conda"),
        )
        .unwrap();
        RepoDataRecord {
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
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy[version='>=1.20']".to_string(),
                    source: "runtime".to_string(),
                },
                LockedRequirement {
                    matchspec: "python >=3.9".to_string(),
                    source: "requires-python".to_string(),
                },
                LockedRequirement {
                    matchspec: "ruff".to_string(),
                    source: "group:dev".to_string(),
                },
            ],
            packages: vec![package("numpy", "1.23.5"), package("ruff", "0.1.0")],
        }
    }

    #[test]
    fn section_round_trips_through_toml() {
        let section = section();
        let item = section_to_item(&section);

        let mut platforms = Table::new();
        platforms["linux-64"] = item;
        let mut doc = DocumentMut::new();
        doc["platforms"] = Item::Table(platforms);

        let parsed = LockFile::parse(&doc.to_string()).unwrap();
        let parsed_section = &parsed.platforms[&Platform::Linux64];
        assert_eq!(parsed_section.requirements, section.requirements);
        assert_eq!(parsed_section.packages, section.packages);
    }

    #[test]
    fn empty_section_round_trips() {
        let section = PlatformSection::default();
        let item = section_to_item(&section);
        let mut platforms = Table::new();
        platforms["osx-arm64"] = item;
        let mut doc = DocumentMut::new();
        doc["platforms"] = Item::Table(platforms);

        let parsed = LockFile::parse(&doc.to_string()).unwrap();
        assert_eq!(
            parsed.platforms[&Platform::OsxArm64],
            PlatformSection::default()
        );
    }

    #[test]
    fn splice_preserves_other_platforms_and_keys() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");

        let existing = r#"# a hand-written comment
[tooling]
note = "leave me alone"

[[platforms.osx-arm64.requirements]]
matchspec = "numpy[version='>=1.20']"
source = "runtime"
"#;
        fs::write(&lock_path, existing).unwrap();

        splice_section(&lock_path, Platform::Linux64, &section()).unwrap();

        let text = fs::read_to_string(&lock_path).unwrap();
        assert!(text.contains("a hand-written comment"));
        assert!(text.contains("leave me alone"));

        let parsed = LockFile::parse(&text).unwrap();
        // The pre-existing osx-arm64 section survived untouched...
        let osx = &parsed.platforms[&Platform::OsxArm64];
        assert_eq!(osx.requirements.len(), 1);
        assert!(osx.packages.is_empty());
        // ... and the new linux-64 section landed whole.
        let linux = &parsed.platforms[&Platform::Linux64];
        assert_eq!(linux, &section());
    }

    #[test]
    fn splice_replaces_only_the_named_platforms_section() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");

        splice_section(&lock_path, Platform::Linux64, &section()).unwrap();
        let mut replacement = section();
        replacement.packages = vec![package("numpy", "1.24.0")];
        splice_section(&lock_path, Platform::Linux64, &replacement).unwrap();

        let parsed = LockFile::read(&lock_path).unwrap().unwrap();
        assert_eq!(parsed.platforms.len(), 1);
        assert_eq!(
            parsed.platforms[&Platform::Linux64].packages[0]
                .package_record
                .version
                .to_string(),
            "1.24.0"
        );
    }

    #[test]
    fn splice_over_unparseable_file_errors_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        fs::write(&lock_path, "this is [not toml").unwrap();

        let result = splice_section(&lock_path, Platform::Linux64, &section());
        assert!(matches!(result, Err(Error::CorruptLock { .. })));
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            "this is [not toml",
            "a corrupt lock must never be silently rewritten"
        );
    }

    #[test]
    fn splice_sections_replaces_many_platforms_in_one_write() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");

        let mut other = section();
        other.packages = vec![package("ruff", "0.2.0")];
        splice_sections(
            &lock_path,
            &[
                (Platform::Linux64, section()),
                (Platform::OsxArm64, other.clone()),
            ],
        )
        .unwrap();

        let parsed = LockFile::read(&lock_path).unwrap().unwrap();
        assert_eq!(parsed.platforms[&Platform::Linux64], section());
        assert_eq!(parsed.platforms[&Platform::OsxArm64], other);
    }

    #[test]
    fn parse_platform_section_ignores_broken_foreign_sections() {
        let text = r#"
[[platforms.linux-64.requirements]]
matchspec = "ruff"
source = "runtime"

[[platforms.osx-arm64.packages]]
name = 42
"#;
        // The osx-arm64 section is semantically broken (a package with an
        // integer name fails the full parse)...
        assert!(LockFile::parse(text).is_err());
        // ...but parsing linux-64 alone neither fails nor sees it.
        let section = parse_platform_section(text, Platform::Linux64)
            .unwrap()
            .unwrap();
        assert_eq!(section.requirements.len(), 1);
        // A broken *target* section reads as absent (regenerate), and a
        // syntactically broken document is an error everywhere.
        assert_eq!(
            parse_platform_section(text, Platform::OsxArm64).unwrap(),
            None
        );
        assert!(parse_platform_section("not [toml", Platform::Linux64).is_err());
    }

    #[test]
    fn unknown_platform_sections_are_skipped_but_preserved() {
        // `plan9-64` is not a `Platform` rattler knows, so the section is
        // skipped in the model -- but must survive splicing on disk.
        let text = r#"
[[platforms.plan9-64.requirements]]
matchspec = "ruff"
source = "runtime"

[[platforms.linux-64.requirements]]
matchspec = "ruff"
source = "runtime"
"#;
        let parsed = LockFile::parse(text).unwrap();
        assert_eq!(parsed.platforms.len(), 1);
        assert!(parsed.platforms.contains_key(&Platform::Linux64));

        // Splicing must not discard the unknown section.
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        fs::write(&lock_path, text).unwrap();
        splice_section(&lock_path, Platform::Osx64, &section()).unwrap();
        let after = fs::read_to_string(&lock_path).unwrap();
        assert!(after.contains("plan9-64"));
    }

    #[test]
    fn canonicalize_sorts_requirements_and_packages() {
        let mut a = section();
        let mut b = section();
        b.requirements.reverse();
        b.packages.reverse();

        a.canonicalize();
        b.canonicalize();
        assert_eq!(
            a, b,
            "same content, different in-memory order, must canonicalize identically"
        );
    }

    #[test]
    fn missing_lock_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(LockFile::read(&dir.path().join("ana.lock"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn unparseable_lock_reads_as_error() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        fs::write(&lock_path, "not [toml").unwrap();
        assert!(LockFile::read(&lock_path).is_err());
    }

    #[test]
    fn parse_accepts_missing_and_current_version() {
        assert!(LockFile::parse("[platforms.linux-64]\n").is_ok());
        assert!(LockFile::parse("version = 1\n\n[platforms.linux-64]\n").is_ok());
    }

    #[test]
    fn parse_rejects_unsupported_version() {
        let newer = "version = 2\n\n[platforms.linux-64]\n";
        let err = LockFile::parse(newer).unwrap_err();
        assert!(err.to_string().contains("unsupported version 2"));

        // Not an integer at all.
        assert!(LockFile::parse("version = \"1\"\n").is_err());
        // Zero/negative are not valid versions either.
        assert!(LockFile::parse("version = 0\n").is_err());

        // The single-section read path enforces it too.
        assert!(parse_platform_section(newer, Platform::Linux64).is_err());
    }

    #[test]
    fn splice_stamps_version_into_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");

        splice_section(&lock_path, Platform::Linux64, &section()).unwrap();

        let text = fs::read_to_string(&lock_path).unwrap();
        assert!(
            text.starts_with(&format!("version = {LOCK_FILE_VERSION}")),
            "version is stamped at the top of a fresh file: {text}"
        );
        assert!(LockFile::parse(&text).is_ok());
    }

    #[test]
    fn splice_adds_version_to_pre_versioning_file() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        fs::write(
            &lock_path,
            "[[platforms.osx-arm64.requirements]]\nmatchspec = \"ruff\"\nsource = \"runtime\"\n",
        )
        .unwrap();

        splice_section(&lock_path, Platform::Linux64, &section()).unwrap();

        let text = fs::read_to_string(&lock_path).unwrap();
        assert!(text.contains(&format!("version = {LOCK_FILE_VERSION}")));
        assert!(text.contains("osx-arm64"), "existing section preserved");
    }

    #[test]
    fn splice_over_unsupported_version_errors_and_preserves_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        let newer = "version = 2\n\n[platforms.linux-64]\n";
        fs::write(&lock_path, newer).unwrap();

        let result = splice_section(&lock_path, Platform::Linux64, &section());
        assert!(matches!(result, Err(Error::CorruptLock { .. })));
        assert_eq!(
            fs::read_to_string(&lock_path).unwrap(),
            newer,
            "a newer-version lock must never be written into"
        );
    }

    #[test]
    fn malformed_sections_are_parse_errors() {
        // `packages` entry that doesn't deserialize as a PackageRecord.
        let text = r#"
[platforms.linux-64]

[[platforms.linux-64.packages]]
name = 42
"#;
        assert!(LockFile::parse(text).is_err());

        // `requirements` entry without a matchspec.
        let text = r#"
[[platforms.linux-64.requirements]]
source = "runtime"
"#;
        assert!(LockFile::parse(text).is_err());
    }
}
