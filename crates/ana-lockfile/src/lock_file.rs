//! The `ana.lock` file: model, TOML parsing, and the
//! re-read/splice/atomic-write sequence used by every mode that resolves.
//!
//! Format (per `investigations/lock_generation_algorithm.md`, "Decision:
//! `ana.lock` partitions by `(environment, platform)`"): one `[platforms.
//! <subdir>]` table per solved platform, each holding only real,
//! resolve-time data -- the canonical matchspecs the platform was solved
//! from, `requires_python`, and the full resolved [`PackageRecord`] set.
//! No staleness bookkeeping (hashes) lives here; that's the cache file's
//! job (`crate::cache`). One file per bucket, so the `(environment, ...)`
//! half of rattler's partition key is the file's location, not a key in
//! it.
//!
//! ```toml
//! [platforms.linux-64]
//! requires_python = ">=3.9"
//!
//! [[platforms.linux-64.requirements]]
//! matchspec = "numpy >=1.20"
//! source = "runtime"
//!
//! [[platforms.linux-64.packages]]
//! name = "numpy"
//! version = "1.23.5"
//! # ... full PackageRecord fields
//! ```
//!
//! Two deliberate parsing decisions:
//!
//! - **Unknown platform keys are skipped, not rejected.** A lock written by
//!   a newer `ana` that supports subdirs this one doesn't know still parses;
//!   the unknown section is simply absent from the model. Splicing works on
//!   the raw document, so such sections survive a resolve untouched.
//! - **No semantic-completeness validation beyond shape.** A section that
//!   parses is used as-is, even with empty `requirements`/`packages` (a
//!   legitimately empty environment looks exactly like that). The
//!   investigation's open TODO about "requirements present, packages empty"
//!   is resolved here as *not* a regen trigger: with atomic writes a
//!   half-written section is structurally impossible, so that state only
//!   arises from hand-editing, which is explicitly out of scope
//!   (`sync_algorithm.md`). Both arrays may simply be absent when empty --
//!   an empty array-of-tables has no TOML rendering.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use rattler_conda_types::{PackageRecord, Platform};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::error::Error;
use crate::fs_util::write_atomic;
use crate::hash::sha256_hex;

/// One requirement a platform section was solved from: the canonical
/// matchspec string ([`rattler_conda_types::MatchSpec`]'s `Display`), plus
/// where in `pyproject.toml` it came from (`source` -- `"runtime"` or
/// `"group:<name>"`; informational only, never part of the stage-2
/// comparison, which is a pure set diff on matchspec strings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedRequirement {
    pub matchspec: String,
    pub source: String,
}

/// One platform's section of `ana.lock`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlatformSection {
    /// Canonical `Display` of the `pyproject.toml` `requires-python`
    /// specifier set at solve time, `None` if the project doesn't declare
    /// one. Its own field, not folded into `requirements` -- a
    /// `requires-python` edit must invalidate the section even though it
    /// isn't an entry in `[project.dependencies]`.
    pub requires_python: Option<String>,
    pub requirements: Vec<LockedRequirement>,
    /// Full resolved records (`lock_file.md`'s Property 2), so a future
    /// re-solve can feed them back to the solver as preference hints
    /// without re-fetching metadata.
    pub packages: Vec<PackageRecord>,
}

impl PlatformSection {
    /// SHA-256 of the canonical serialization of this section -- the
    /// `ana_lock_hash` half of the stage-1 cache. Hashes the *parsed*
    /// section (requirements sorted by matchspec string, packages sorted by
    /// [`PackageRecord`]'s `Ord`, object keys sorted recursively), never
    /// raw file bytes, so serializer or formatting drift elsewhere in the
    /// file doesn't cause spurious stage-1 misses.
    pub fn hash(&self) -> String {
        let mut canonical = String::new();
        canonical.push_str("requires_python\0");
        canonical.push_str(self.requires_python.as_deref().unwrap_or(""));
        canonical.push('\0');

        let mut requirements: Vec<&LockedRequirement> = self.requirements.iter().collect();
        requirements.sort_by(|a, b| a.matchspec.cmp(&b.matchspec).then(a.source.cmp(&b.source)));
        for req in requirements {
            canonical.push_str(&req.matchspec);
            canonical.push('\0');
            canonical.push_str(&req.source);
            canonical.push('\0');
        }

        let mut packages: Vec<&PackageRecord> = self.packages.iter().collect();
        packages.sort();
        for package in packages {
            // `PackageRecord`'s `Serialize` is total (plain data, no
            // fallible custom impls), so a failure here is unreachable in
            // practice; degrade to an empty object rather than panic.
            let json = serde_json::to_value(package).unwrap_or(serde_json::Value::Null);
            write_canonical_json(&mut canonical, &json);
            canonical.push('\0');
        }

        sha256_hex(canonical.as_bytes())
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

impl LockFile {
    /// Parse lock file text. Unknown platform keys are skipped (see module
    /// docs); structurally wrong content is a [`LockParseError`].
    pub fn parse(text: &str) -> Result<Self, LockParseError> {
        let doc = DocumentMut::from_str(text)
            .map_err(|err| LockParseError(format!("invalid TOML: {err}")))?;

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

/// Parse one `[platforms.<subdir>]` table into a [`PlatformSection`].
fn parse_section(key: &str, item: &Item) -> Result<PlatformSection, LockParseError> {
    let err = |what: &str| LockParseError(format!("platforms.{key}: {what}"));

    let table = item
        .as_table()
        .ok_or_else(|| err("section is not a table"))?;

    let requires_python = match table.get("requires_python") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| err("`requires_python` is not a string"))?
                .to_string(),
        ),
    };

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
            let record = serde_json::from_value::<PackageRecord>(json).map_err(|serde_err| {
                err(&format!("package record does not deserialize: {serde_err}"))
            })?;
            packages.push(record);
        }
    }

    Ok(PlatformSection {
        requires_python,
        requirements,
        packages,
    })
}

/// Re-read `lock_path`, replace only `platform`'s section with `section`,
/// and atomically write the result back. Every other key in the document --
/// other platforms' sections, unknown future keys, comments, formatting --
/// is preserved exactly, which is what makes concurrent resolves for
/// *different* platforms safe to serialize through the bucket lock: the
/// re-read happens here, inside the critical section, immediately before
/// the write, so a section another process wrote while we were solving is
/// spliced *around*, never reverted to our stale in-memory snapshot.
///
/// An unparseable existing file is replaced by a fresh document containing
/// only the new section: unparseable content is unrecoverable anyway, and
/// every mode that reaches this function has already decided to regenerate.
pub(crate) fn splice_section(
    lock_path: &Path,
    platform: Platform,
    section: &PlatformSection,
) -> Result<(), Error> {
    let mut doc = match fs::read_to_string(lock_path) {
        Ok(text) => DocumentMut::from_str(&text).unwrap_or_default(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => DocumentMut::new(),
        Err(err) => {
            return Err(Error::Read {
                path: lock_path.to_path_buf(),
                source: err,
            });
        }
    };

    if doc.get("platforms").and_then(Item::as_table).is_none() {
        doc["platforms"] = Item::Table(Table::new());
    }
    doc["platforms"][platform.as_str()] = section_to_item(section);

    write_atomic(lock_path, doc.to_string().as_bytes()).map_err(|err| Error::Write {
        path: lock_path.to_path_buf(),
        source: err,
    })
}

/// Build the TOML for one platform section: an optional `requires_python`
/// scalar, then `requirements` and `packages` arrays of tables (omitted
/// when empty -- an empty array-of-tables has no TOML rendering, and
/// absence parses back to an empty list, so this round-trips).
fn section_to_item(section: &PlatformSection) -> Item {
    let mut table = Table::new();
    if let Some(requires_python) = &section.requires_python {
        table["requires_python"] = Item::Value(Value::String(toml_edit::Formatted::new(
            requires_python.clone(),
        )));
    }

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

/// A [`PackageRecord`] as a TOML table, via its `Serialize` impl and a
/// JSON bridge: `serde_json` gives us the record's canonical field set
/// (alphabetical, `None`s omitted -- that's what `rattler_macros`'
/// `#[sorted]`/`#[skip_serializing_none]` produce) as plain data, which is
/// then transcribed field-by-field. Nested objects become inline tables so
/// each `[[packages]]` entry stays self-contained.
fn package_to_table(package: &PackageRecord) -> Table {
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
/// [`package_to_table`], feeding `serde_json::from_value::<PackageRecord>`.
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

/// Append `value` to `out` in a canonical JSON form: object keys sorted
/// recursively, no whitespace. Deterministic regardless of which map
/// implementation `serde_json` was compiled with, which is what makes it
/// safe to hash.
fn write_canonical_json(out: &mut String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => out.push_str("null"),
        serde_json::Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        serde_json::Value::Number(n) => out.push_str(&n.to_string()),
        serde_json::Value::String(s) => {
            // `serde_json::to_string` on a string is JSON string escaping;
            // infallible for plain data.
            out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()));
        }
        serde_json::Value::Array(values) => {
            out.push('[');
            for (i, value) in values.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical_json(out, value);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string());
                let _ = write!(out, "{key}:");
                write_canonical_json(out, value);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use rattler_conda_types::{PackageName, Version};

    use super::*;

    fn package(name: &str, version: &str) -> PackageRecord {
        let mut record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            "py312h1234567_0".to_string(),
        );
        record.subdir = "linux-64".to_string();
        record
    }

    fn section() -> PlatformSection {
        PlatformSection {
            requires_python: Some(">=3.9".to_string()),
            requirements: vec![
                LockedRequirement {
                    matchspec: "numpy[version='>=1.20']".to_string(),
                    source: "runtime".to_string(),
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
        assert_eq!(parsed_section.requires_python, section.requires_python);
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

[platforms.osx-arm64]
requires_python = ">=3.10"

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
        assert_eq!(osx.requires_python.as_deref(), Some(">=3.10"));
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
                .version
                .to_string(),
            "1.24.0"
        );
    }

    #[test]
    fn splice_over_unparseable_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        fs::write(&lock_path, "this is [not toml").unwrap();

        splice_section(&lock_path, Platform::Linux64, &section()).unwrap();
        let parsed = LockFile::read(&lock_path).unwrap().unwrap();
        assert_eq!(parsed.platforms[&Platform::Linux64], section());
    }

    #[test]
    fn unknown_platform_sections_are_skipped_but_preserved() {
        // `plan9-64` is not a `Platform` rattler knows, so the section is
        // skipped in the model -- but must survive splicing on disk.
        let text = r#"
[platforms.plan9-64]
requires_python = ">=3.9"

[platforms.linux-64]
requires_python = ">=3.9"
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
    fn section_hash_is_stable_across_ordering_and_formatting() {
        let a = section();
        // Same content, different in-memory ordering.
        let mut b = section();
        b.requirements.reverse();
        b.packages.reverse();
        assert_eq!(a.hash(), b.hash());

        // And a serialization round-trip (which may reorder TOML keys)
        // doesn't change it either.
        let item = section_to_item(&a);
        let mut platforms = Table::new();
        platforms["linux-64"] = item;
        let mut doc = DocumentMut::new();
        doc["platforms"] = Item::Table(platforms);
        let parsed = LockFile::parse(&doc.to_string()).unwrap();
        assert_eq!(a.hash(), parsed.platforms[&Platform::Linux64].hash());
    }

    #[test]
    fn section_hash_changes_with_content() {
        let a = section();
        let mut b = section();
        b.requirements[0].matchspec = "numpy[version='>=1.21']".to_string();
        assert_ne!(a.hash(), b.hash());

        let mut c = section();
        c.requires_python = Some(">=3.10".to_string());
        assert_ne!(a.hash(), c.hash());
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
