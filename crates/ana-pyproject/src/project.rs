//! The `pyproject.toml` front end: [`Pyproject::parse`] extracts the pieces
//! of a `pyproject.toml` that `ana` consumes -- the project name and its
//! requirements -- rejecting anything outside the supported static
//! PEP 621 / PEP 735 shape, plus ana's own `[tool.ana]`
//! `matchspec-dependencies`/`matchspec-dependency-groups` extension (see
//! [`ProjectRequirements`]'s docs).
//!
//! ## Error reporting: fail fast, except for requirement strings
//!
//! Every structural check here -- `project.name` present and valid,
//! `dynamic` not claiming a static key, no legacy-Poetry tell, each of
//! `dependencies`/`optional-dependencies`/`dependency-groups`/
//! `tool.ana.matchspec-dependencies`/`tool.ana.matchspec-dependency-groups`
//! the right shape with no duplicate names -- returns a single
//! [`InvalidField`] and stops as soon as it finds a problem, via `?`. The
//! one exception: once every section above is shape-valid, the literal
//! PEP 508 requirement strings and conda `MatchSpec` strings inside them
//! are parsed and *every* failure among them is collected into one
//! [`PyprojectError`] instead of stopping at the first. See
//! [`PyprojectError`]'s docs for why requirement/matchspec strings are the
//! one place this aggregates.
//!
//! ## Performance notes
//!
//! The only expensive step here is turning PEP 508 strings into
//! [`Requirement`]s and `MatchSpec` strings into [`MatchSpec`]s --
//! everything else (the TOML walk, the resolution graph) touches at most a
//! few dozen nodes. So:
//!
//! - **One parallel region per string kind, not one per extra/group.**
//!   Every literal PEP 508 string in the document is flattened into a
//!   single `Vec<&str>` before any parsing happens, and parsed with one
//!   `rayon` call; every literal matchspec string gets its own separate
//!   flatten-then-parse pass, since `Requirement::from_str` and
//!   `MatchSpec::from_str` are different functions with different error
//!   types. Entering a `rayon` parallel region has a fixed cost (waking
//!   parked worker threads is a syscall) regardless of how much work is
//!   inside it, so paying that once per document per string kind beats
//!   paying it once per `[dependency-groups]`/
//!   `[tool.ana.matchspec-dependency-groups]` entry.
//! - **No thread pool of our own.** We call `par_iter`/`into_par_iter`
//!   against the process-global `rayon` pool rather than building a
//!   `ThreadPoolBuilder` -- a second pool would mean two sets of OS
//!   threads competing for the same cores.
//! - **Sequential fallback below a size threshold.** Below
//!   [`PARALLEL_PARSE_THRESHOLD`] this skips `rayon` entirely, since for
//!   the common case (a handful of dependencies) the fixed cost of
//!   entering rayon's split/join machinery exceeds just parsing inline.
//!   Both branches produce the same type, so the threshold is a pure
//!   performance knob with zero effect on behavior.
//! - **Errors are the only place we allocate freely.** Building a
//!   `String`/path/`format!` happens only once something is already
//!   wrong; the success path never pays for diagnostics it doesn't need.
//! - **No locks.** Each parallel step is a pure `map` over an
//!   [`IndexedParallelIterator`](rayon::iter::IndexedParallelIterator)
//!   collected into a `Vec` -- rayon pre-sizes the output and each worker
//!   writes into its own disjoint index range, so there's nothing to
//!   contend on and result order matches input order for free. A
//!   sequential pass afterwards fans the results back out into
//!   `runtime`/`extras`/`groups` by walking the same containers in the
//!   same order they were flattened from, with no index bookkeeping
//!   needed. PEP 508 entries are reassembled first, then matchspec
//!   entries -- see [`ProjectRequirements`]'s docs for why that merge
//!   order is what it is.
//! - **Duplicate-name checks probe the table twice, deliberately.**
//!   `extract_extras`/`extract_group_table` (shared by [`extract_groups`]
//!   and [`extract_conda_groups`]) check `contains_key` and then `insert`
//!   separately rather than bucketing every raw key first -- bucketing
//!   only pays off if every problem is collected before being reported,
//!   but here the first duplicate found already stops the walk. Note
//!   that a group name can legitimately appear in *both*
//!   `[dependency-groups]` and `[tool.ana.matchspec-dependency-groups]`
//!   -- that's a merge, not a duplicate; only a repeated key *within the
//!   same table* is rejected, which holds automatically since
//!   `extract_group_table` starts a fresh map on every call.
//! - **Every collection here is pre-sized.** `table.len()` (an O(1)
//!   `toml_edit` call) is already an exact upper bound before either loop
//!   in `extract_extras`/`extract_group_table` starts, and exactly the
//!   final size on the success path, so there's no reason to let
//!   `IndexMap` grow-and-rehash its way there.

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use ana_dependency::MatchspecError;
use indexmap::IndexMap;
use rattler_conda_types::MatchSpec;
use rayon::prelude::*;
use toml_edit::{Document, Item, TableLike};
use uv_normalize::{ExtraName, GroupName, PackageName};
use uv_pep440::VersionSpecifiers;
use uv_pep508::Requirement;

use crate::resolution::{self, Dependency, DependencyGroupSpecifier};

/// Below this many total requirement strings in the document, parse them
/// sequentially instead of handing them to `rayon`. See the module docs.
///
/// A single `Requirement::from_str` call is on the order of hundreds of
/// nanoseconds to low microseconds; waking a parked `rayon` worker thread
/// can be an OS-scheduler round trip an order of magnitude more expensive
/// than that. This is a starting estimate, not a measured one -- retune
/// from a `criterion` benchmark once a real corpus exists.
const PARALLEL_PARSE_THRESHOLD: usize = 64;

/// The parts of a `pyproject.toml` that `ana` consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pyproject {
    /// `[project.name]`, normalized. Required to be present and static --
    /// self-referential-extra resolution needs it.
    pub name: PackageName,
    /// `[project.requires-python]`, parsed. Not a requirement itself -- it
    /// constrains the interpreter the environment is solved around, so the
    /// lock algorithm checks it as its own field rather than folding it
    /// into the requirement set. `None` when the key is absent.
    pub requires_python: Option<VersionSpecifiers>,
    /// Runtime dependencies, extras, and dependency groups.
    pub requirements: ProjectRequirements,
}

impl Pyproject {
    /// Parse `pyproject.toml` source text into a [`Pyproject`].
    ///
    /// The only entrypoint -- callers read the file off disk themselves.
    /// Returns the first structural problem found, or every invalid PEP 508
    /// requirement string once the document's shape checks out -- see the
    /// module docs and [`PyprojectError`] for the two-tier contract.
    pub fn parse(toml: &str) -> Result<Self, PyprojectError> {
        // A syntax error means there's no document to walk at all; there's
        // nothing more specific than "the file" to blame.
        let doc = Document::<&str>::parse(toml)
            .map_err(|err| InvalidField::new("", Some(err.to_string())))?;

        let project = doc
            .get("project")
            .and_then(Item::as_table_like)
            .ok_or_else(|| InvalidField::new("project", None))?;

        // --- Structural checks: each returns (via `?`) on the first
        // problem found, in document order. Reaching the parallel parsing
        // region below means every one of these passed. ---

        let name = extract_name(project)?;
        check_dynamic(project)?;
        check_legacy_poetry(&doc, project)?;
        let requires_python = extract_requires_python(project)?;

        let runtime_raw = extract_dependencies(project)?;
        let extras_raw = extract_extras(project)?;
        let groups_slots = extract_groups(&doc)?;
        let conda_runtime_raw = extract_conda_dependencies(&doc)?;
        let conda_groups_slots = extract_conda_groups(&doc)?;

        // --- Two parallel regions: one for every PEP 508 string in the
        // document, one for every matchspec string. See the module docs
        // for why these are flattened first instead of parsed per-section,
        // and why matchspecs get their own pass rather than sharing the
        // PEP 508 one. This is the one place multiple failures are
        // collected instead of stopping at the first. ---

        let total_raw = runtime_raw.len()
            + extras_raw.values().map(Vec::len).sum::<usize>()
            + groups_slots
                .values()
                .flat_map(|slots| slots.iter())
                .filter(|slot| matches!(slot, GroupSlot::Requirement(..)))
                .count();

        let mut flat: Vec<&str> = Vec::with_capacity(total_raw);
        flat.extend(runtime_raw.iter().map(|&(_, s)| s));
        for raws in extras_raw.values() {
            flat.extend(raws.iter().map(|&(_, s)| s));
        }
        for slots in groups_slots.values() {
            flat.extend(slots.iter().filter_map(|slot| match slot {
                GroupSlot::Requirement(_, s) => Some(*s),
                GroupSlot::Include(_) => None,
            }));
        }

        let parsed: Vec<Result<Requirement, uv_pep508::Pep508Error>> =
            if flat.len() >= PARALLEL_PARSE_THRESHOLD {
                flat.into_par_iter().map(Requirement::from_str).collect()
            } else {
                flat.into_iter().map(Requirement::from_str).collect()
            };
        let mut parsed = parsed.into_iter();

        let total_conda_raw = conda_runtime_raw.len()
            + conda_groups_slots
                .values()
                .flat_map(|slots| slots.iter())
                .filter(|slot| matches!(slot, CondaGroupSlot::Matchspec(..)))
                .count();

        let mut flat_matchspec: Vec<&str> = Vec::with_capacity(total_conda_raw);
        flat_matchspec.extend(conda_runtime_raw.iter().map(|&(_, s)| s));
        for slots in conda_groups_slots.values() {
            flat_matchspec.extend(slots.iter().filter_map(|slot| match slot {
                CondaGroupSlot::Matchspec(_, s) => Some(*s),
                CondaGroupSlot::Include(_) => None,
            }));
        }

        let parsed_matchspec: Vec<Result<MatchSpec, MatchspecError>> =
            if flat_matchspec.len() >= PARALLEL_PARSE_THRESHOLD {
                flat_matchspec
                    .into_par_iter()
                    .map(ana_dependency::parse_matchspec)
                    .collect()
            } else {
                flat_matchspec
                    .into_iter()
                    .map(ana_dependency::parse_matchspec)
                    .collect()
            };
        let mut parsed_matchspec = parsed_matchspec.into_iter();

        // Sequential passes reconnecting each parsed result to where its
        // raw string came from, in the exact order `flat`/`flat_matchspec`
        // were built above, so no index bookkeeping is needed to line
        // either up.

        let mut errors: Vec<InvalidField> = Vec::new();

        // `runtime` merges `[project.dependencies]` (PEP 508) and
        // `[tool.ana.matchspec-dependencies]` (matchspec) into one list --
        // PEP 508 entries first, then matchspec entries, both in file
        // order. See `ProjectRequirements`'s docs for why this merge order
        // was chosen.
        let mut runtime = Vec::with_capacity(runtime_raw.len() + conda_runtime_raw.len());
        for (i, _) in &runtime_raw {
            match next_parsed(&mut parsed, || format!("project.dependencies[{i}]")) {
                Ok(req) => runtime.push(Dependency::Pep508(req)),
                Err(err) => errors.push(err),
            }
        }
        for (i, _) in &conda_runtime_raw {
            match next_parsed_matchspec(&mut parsed_matchspec, || {
                format!("tool.ana.matchspec-dependencies[{i}]")
            }) {
                Ok(spec) => runtime.push(Dependency::Matchspec(Box::new(spec))),
                Err(err) => errors.push(err),
            }
        }

        let mut extras_unresolved: IndexMap<ExtraName, Vec<Requirement>> =
            IndexMap::with_capacity(extras_raw.len());
        for (extra_name, raws) in extras_raw {
            let mut reqs = Vec::with_capacity(raws.len());
            for (i, _) in &raws {
                let path = || format!("project.optional-dependencies.{}[{i}]", extra_name.as_str());
                match next_parsed(&mut parsed, path) {
                    Ok(req) => reqs.push(req),
                    Err(err) => errors.push(err),
                }
            }
            extras_unresolved.insert(extra_name, reqs);
        }

        // `groups_unresolved` merges `[dependency-groups]` (PEP 508) and
        // `[tool.ana.matchspec-dependency-groups]` (matchspec) into one map,
        // keyed by normalized group name: a group present in both tables
        // gets its PEP 508 entries first, then its matchspec entries
        // appended, both in file order; a group present in only one table
        // is unaffected. See `ProjectRequirements`'s docs.
        let mut groups_unresolved: IndexMap<GroupName, Vec<DependencyGroupSpecifier>> =
            IndexMap::with_capacity(groups_slots.len() + conda_groups_slots.len());
        for (group_name, slots) in groups_slots {
            let mut specs = Vec::with_capacity(slots.len());
            for slot in slots {
                match slot {
                    GroupSlot::Include(target) => {
                        specs.push(DependencyGroupSpecifier::IncludeGroup(target));
                    }
                    GroupSlot::Requirement(i, _) => {
                        let path = || format!("dependency-groups.{}[{i}]", group_name.as_str());
                        match next_parsed(&mut parsed, path) {
                            Ok(req) => specs.push(DependencyGroupSpecifier::Dependency(
                                Dependency::Pep508(req),
                            )),
                            Err(err) => errors.push(err),
                        }
                    }
                }
            }
            groups_unresolved.insert(group_name, specs);
        }
        for (group_name, slots) in conda_groups_slots {
            let specs = groups_unresolved.entry(group_name.clone()).or_default();
            for slot in slots {
                match slot {
                    CondaGroupSlot::Include(target) => {
                        specs.push(DependencyGroupSpecifier::IncludeGroup(target));
                    }
                    CondaGroupSlot::Matchspec(i, _) => {
                        let path = || {
                            format!(
                                "tool.ana.matchspec-dependency-groups.{}[{i}]",
                                group_name.as_str()
                            )
                        };
                        match next_parsed_matchspec(&mut parsed_matchspec, path) {
                            Ok(spec) => specs.push(DependencyGroupSpecifier::Dependency(
                                Dependency::Matchspec(Box::new(spec)),
                            )),
                            Err(err) => errors.push(err),
                        }
                    }
                }
            }
        }

        // Every remaining failure at this point is a requirement/matchspec
        // parse failure -- every structural check above already returned
        // early on its own first problem, so `errors` here can only hold
        // entries from the loops directly above. Resolution is only
        // attempted once every one of those has come back clean: running
        // `resolve()` over partial maps (a dropped entry from a bad
        // requirement/matchspec string, say) could produce a misleading
        // "not found" error on top of the real one.
        if !errors.is_empty() {
            return Err(PyprojectError::new(errors));
        }

        // Resolution errors are attributed to a section via
        // `ResolveError::section`, which `resolve()` itself tags at the one
        // place that knows which of its two loops was running -- see that
        // function's docs. No second `resolve()` call needed here just to
        // infer it. `DependencyGroups` now covers both `[dependency-groups]`
        // and `[tool.ana.matchspec-dependency-groups]`, since the two are
        // merged into one graph before `resolve()` ever sees them.
        let resolved = resolution::resolve(
            Some(&name),
            Some(&extras_unresolved),
            Some(&groups_unresolved),
        )
        .map_err(|err| {
            let path = match err.section() {
                resolution::Section::OptionalDependencies => "project.optional-dependencies",
                resolution::Section::DependencyGroups => "dependency-groups",
            };
            PyprojectError::new(vec![InvalidField {
                path: path.to_string(),
                description: Some(err.to_string()),
            }])
        })?;

        Ok(Pyproject {
            name,
            requires_python,
            requirements: ProjectRequirements {
                runtime,
                extras: resolved.optional_dependencies,
                groups: resolved.dependency_groups,
            },
        })
    }
}

/// Pull the next parsed result off the flat cursor, converting it directly
/// into either a `Requirement` or an [`InvalidField`] at `path()`.
///
/// `path` is called at most once, and only once something has actually
/// gone wrong -- the success path (the overwhelming majority of calls)
/// never allocates a path string it doesn't need.
///
/// The cursor running dry -- meaning the number of raw strings collected
/// into `flat` didn't match the number of `Requirement`-shaped slots
/// walked during reassembly, a bug in this module's own bookkeeping rather
/// than a consequence of `pyproject.toml` content -- is handled the same
/// way as any other failure: a reported [`InvalidField`], never a panic.
/// `pyproject.toml` content is untrusted input; this module has no case
/// where panicking on it is acceptable, including one that "shouldn't" be
/// reachable.
fn next_parsed(
    parsed: &mut std::vec::IntoIter<Result<Requirement, uv_pep508::Pep508Error>>,
    path: impl FnOnce() -> String,
) -> Result<Requirement, InvalidField> {
    match parsed.next() {
        Some(Ok(req)) => Ok(req),
        Some(Err(err)) => Err(InvalidField::new(&path(), Some(err.to_string()))),
        None => Err(InvalidField::new(
            &path(),
            Some("internal error: ran out of parsed requirements".to_string()),
        )),
    }
}

/// Pull the next parsed result off the matchspec flat cursor, converting
/// it directly into either a `MatchSpec` or an [`InvalidField`] at
/// `path()`. Mirrors [`next_parsed`], but for the matchspec parse pass --
/// see the module docs for why matchspec strings get their own flatten/
/// parse/reassemble pass rather than sharing the PEP 508 one.
///
/// Parsing/validation itself is [`ana_dependency::parse_matchspec`]'s
/// job; this function only reassembles the flattened results back into
/// field-path errors.
fn next_parsed_matchspec(
    parsed: &mut std::vec::IntoIter<Result<MatchSpec, MatchspecError>>,
    path: impl FnOnce() -> String,
) -> Result<MatchSpec, InvalidField> {
    match parsed.next() {
        Some(Ok(spec)) => Ok(spec),
        Some(Err(err)) => Err(InvalidField::new(&path(), Some(err.to_string()))),
        None => Err(InvalidField::new(
            &path(),
            Some("internal error: ran out of parsed matchspecs".to_string()),
        )),
    }
}

/// `[project.name]`. Required to be present, a string, non-empty, and a
/// normalizable package name.
fn extract_name(project: &dyn TableLike) -> Result<PackageName, InvalidField> {
    let raw = project
        .get("name")
        .and_then(Item::as_str)
        .ok_or_else(|| InvalidField::new("project.name", None))?;
    if raw.is_empty() {
        return Err(InvalidField::new(
            "project.name",
            Some("project name must not be empty".to_string()),
        ));
    }
    PackageName::from_str(raw)
        .map_err(|err| InvalidField::new("project.name", Some(err.to_string())))
}

/// `[project.dynamic]`. Unconditionally rejects `dependencies`/
/// `optional-dependencies`/`requires-python` if listed, even alongside a
/// static value for the same key. `requires-python` is rejected too since
/// it's a lock input: a value we can't read statically can't be checked
/// for staleness.
fn check_dynamic(project: &dyn TableLike) -> Result<(), InvalidField> {
    let Some(item) = project.get("dynamic") else {
        return Ok(());
    };
    let rejected = match item.as_array() {
        Some(arr) => arr.iter().any(|v| match v.as_str() {
            Some(s) => {
                s == "dependencies" || s == "optional-dependencies" || s == "requires-python"
            }
            None => true, // Non-string entry: shape is already wrong.
        }),
        None => true, // `dynamic` itself isn't an array.
    };
    if rejected {
        Err(InvalidField::new("project.dynamic", None))
    } else {
        Ok(())
    }
}

/// `[project.requires-python]`. Missing entirely means `None` (no
/// interpreter constraint), not an error; present-but-not-a-string or
/// present-but-unparseable as a PEP 440 specifier set is.
fn extract_requires_python(
    project: &dyn TableLike,
) -> Result<Option<VersionSpecifiers>, InvalidField> {
    let Some(item) = project.get("requires-python") else {
        return Ok(None);
    };
    let raw = item
        .as_str()
        .ok_or_else(|| InvalidField::new("project.requires-python", None))?;
    VersionSpecifiers::from_str(raw)
        .map(Some)
        .map_err(|err| InvalidField::new("project.requires-python", Some(err.to_string())))
}

/// The legacy-Poetry tell: `[tool.poetry.dependencies]` present without a
/// corresponding `[project.dependencies]`. Must check the *presence* of
/// `[tool.poetry.dependencies]` independent of whether `dependencies` is
/// itself valid or present -- a missing `dependencies` key is otherwise
/// not an error (empty runtime deps), so without this check a
/// pre-PEP-621 Poetry project would silently resolve to zero dependencies
/// instead of being rejected.
fn check_legacy_poetry(doc: &Document<&str>, project: &dyn TableLike) -> Result<(), InvalidField> {
    let has_poetry_dependencies = doc
        .get("tool")
        .and_then(Item::as_table_like)
        .and_then(|tool| tool.get("poetry"))
        .and_then(Item::as_table_like)
        .and_then(|poetry| poetry.get("dependencies"))
        .is_some();
    if has_poetry_dependencies && project.get("dependencies").is_none() {
        Err(InvalidField::new("tool.poetry.dependencies", None))
    } else {
        Ok(())
    }
}

/// `(original array index, raw PEP 508 string)` pairs for one array of
/// literal requirement strings, before parsing -- the index lets a
/// requirement that fails to *parse* later still be blamed on its real
/// position in the file. Named so `extract_dependencies`/`extract_extras`
/// don't each spell out the same three-deep generic by hand (which also
/// trips `clippy::type_complexity` if written inline).
type RawRequirements<'a> = Vec<(usize, &'a str)>;

/// `[project.dependencies]`. Missing entirely means zero runtime
/// dependencies, not an error; present-but-wrong-shape is -- including a
/// single non-string entry, which stops the walk right there rather than
/// collecting every bad entry (only the requirement-*parsing* tier
/// aggregates; see the module docs). Returns `(original array index, raw
/// PEP 508 string)` pairs.
fn extract_dependencies(project: &dyn TableLike) -> Result<RawRequirements<'_>, InvalidField> {
    let Some(item) = project.get("dependencies") else {
        return Ok(Vec::new());
    };
    let Some(arr) = item.as_array() else {
        return Err(InvalidField::new("project.dependencies", None));
    };
    let mut raw = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| InvalidField::new(&format!("project.dependencies[{i}]"), None))?;
        raw.push((i, s));
    }
    Ok(raw)
}

/// `[project.optional-dependencies]`. Same missing-vs-wrong-shape handling
/// as [`extract_dependencies`], plus duplicate-name detection: a raw key
/// that normalizes to an [`ExtraName`] (`GUI`) already seen under a
/// different spelling (`gui`) is rejected rather than silently merged or
/// overwritten.
fn extract_extras(
    project: &dyn TableLike,
) -> Result<IndexMap<ExtraName, RawRequirements<'_>>, InvalidField> {
    let Some(item) = project.get("optional-dependencies") else {
        return Ok(IndexMap::new());
    };
    let Some(table) = item.as_table_like() else {
        return Err(InvalidField::new("project.optional-dependencies", None));
    };

    let mut extras = IndexMap::with_capacity(table.len());
    for (key, value) in table.iter() {
        let extra_name = ExtraName::from_str(key).map_err(|err| {
            InvalidField::new(
                &format!("project.optional-dependencies.{key}"),
                Some(err.to_string()),
            )
        })?;
        if extras.contains_key(&extra_name) {
            return Err(InvalidField::new(
                "project.optional-dependencies",
                Some(format!("duplicate extra name `{}`", extra_name.as_str())),
            ));
        }
        let arr = value.as_array().ok_or_else(|| {
            InvalidField::new(&format!("project.optional-dependencies.{key}"), None)
        })?;
        let mut raws = Vec::with_capacity(arr.len());
        for (i, v) in arr.iter().enumerate() {
            let s = v.as_str().ok_or_else(|| {
                InvalidField::new(&format!("project.optional-dependencies.{key}[{i}]"), None)
            })?;
            raws.push((i, s));
        }
        extras.insert(extra_name, raws);
    }
    Ok(extras)
}

/// One entry in a `[dependency-groups]`-shaped array, before its literal
/// string (if any) has been parsed and before it's been typed as the
/// caller's own slot enum -- the raw shape [`extract_group_table`] walks
/// every such table into, regardless of which table it came from.
enum GroupTableEntry<'a> {
    /// A literal string: `(original array index, raw string)`.
    Literal(usize, &'a str),
    /// `{ include-group = "<name>" }`. No parsing needed, so this is
    /// resolved eagerly during the walk rather than deferred.
    Include(GroupName),
}

fn extract_group_table<'a>(
    table: &'a dyn TableLike,
    table_path: &str,
) -> Result<IndexMap<GroupName, Vec<GroupTableEntry<'a>>>, InvalidField> {
    let mut groups = IndexMap::with_capacity(table.len());
    for (key, value) in table.iter() {
        let group_name = GroupName::from_str(key).map_err(|err| {
            InvalidField::new(&format!("{table_path}.{key}"), Some(err.to_string()))
        })?;
        if groups.contains_key(&group_name) {
            return Err(InvalidField::new(
                table_path,
                Some(format!("duplicate group name `{}`", group_name.as_str())),
            ));
        }
        let arr = value
            .as_array()
            .ok_or_else(|| InvalidField::new(&format!("{table_path}.{key}"), None))?;

        let mut slots = Vec::with_capacity(arr.len());
        for (i, v) in arr.iter().enumerate() {
            let path = || format!("{table_path}.{key}[{i}]");
            if let Some(s) = v.as_str() {
                slots.push(GroupTableEntry::Literal(i, s));
            } else if let Some(t) = v.as_inline_table() {
                if t.len() != 1 {
                    return Err(InvalidField::new(&path(), None));
                }
                match t.iter().next() {
                    Some(("include-group", value)) => {
                        let target_raw = value
                            .as_str()
                            .ok_or_else(|| InvalidField::new(&path(), None))?;
                        let target = GroupName::from_str(target_raw)
                            .map_err(|err| InvalidField::new(&path(), Some(err.to_string())))?;
                        slots.push(GroupTableEntry::Include(target));
                    }
                    _ => return Err(InvalidField::new(&path(), None)),
                }
            } else {
                return Err(InvalidField::new(&path(), None));
            }
        }
        groups.insert(group_name, slots);
    }
    Ok(groups)
}

/// One entry in a `[dependency-groups]` list, before its literal
/// requirement strings have been parsed. Mirrors
/// [`DependencyGroupSpecifier`], but holds a raw `&str` (plus its original
/// array index, for error paths) instead of a parsed [`Requirement`] --
/// parsing happens later, in the single flattened parallel region.
enum GroupSlot<'a> {
    /// A literal PEP 508 requirement string: `(original array index, raw
    /// string)`.
    Requirement(usize, &'a str),
    /// `{ include-group = "<name>" }`. No parsing needed, so this is
    /// resolved eagerly during the walk rather than deferred.
    Include(GroupName),
}

/// `[dependency-groups]`. See [`extract_group_table`] for the shape/error
/// handling this delegates to; this just types the walk's raw
/// [`GroupTableEntry`]s as [`GroupSlot`]s.
fn extract_groups<'a>(
    doc: &'a Document<&str>,
) -> Result<IndexMap<GroupName, Vec<GroupSlot<'a>>>, InvalidField> {
    let Some(item) = doc.get("dependency-groups") else {
        return Ok(IndexMap::new());
    };
    let Some(table) = item.as_table_like() else {
        return Err(InvalidField::new("dependency-groups", None));
    };
    Ok(extract_group_table(table, "dependency-groups")?
        .into_iter()
        .map(|(name, entries)| {
            let slots = entries
                .into_iter()
                .map(|entry| match entry {
                    GroupTableEntry::Literal(i, s) => GroupSlot::Requirement(i, s),
                    GroupTableEntry::Include(target) => GroupSlot::Include(target),
                })
                .collect();
            (name, slots)
        })
        .collect())
}

/// `[tool.ana]`, if present and table-like. `None` covers both "absent"
/// and "present but not a table" (e.g. `tool = "oops"`) -- `[tool.ana]`
/// is a foreign namespace that's otherwise entirely ignored (see the
/// `unrelated_fields_are_ignored` test), so a malformed `tool`/`tool.ana`
/// is silently treated the same as an absent one rather than rejected;
/// only the two specific keys this module actually reads under it --
/// `matchspec-dependencies` and `matchspec-dependency-groups` -- are
/// required to be the right shape once they're looked up.
fn ana_table<'a>(doc: &'a Document<&str>) -> Option<&'a dyn TableLike> {
    doc.get("tool")
        .and_then(Item::as_table_like)
        .and_then(|tool| tool.get("ana"))
        .and_then(Item::as_table_like)
}

/// `[tool.ana.matchspec-dependencies]`. Same missing-vs-wrong-shape
/// handling as [`extract_dependencies`], but for conda `MatchSpec`
/// strings rather than PEP 508 ones. Returns `(original array index, raw
/// matchspec string)` pairs.
fn extract_conda_dependencies<'a>(
    doc: &'a Document<&str>,
) -> Result<RawRequirements<'a>, InvalidField> {
    let Some(ana) = ana_table(doc) else {
        return Ok(Vec::new());
    };
    let Some(item) = ana.get("matchspec-dependencies") else {
        return Ok(Vec::new());
    };
    let Some(arr) = item.as_array() else {
        return Err(InvalidField::new("tool.ana.matchspec-dependencies", None));
    };
    let mut raw = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let s = v.as_str().ok_or_else(|| {
            InvalidField::new(&format!("tool.ana.matchspec-dependencies[{i}]"), None)
        })?;
        raw.push((i, s));
    }
    Ok(raw)
}

/// One entry in a `[tool.ana.matchspec-dependency-groups]` list, before its
/// literal matchspec strings have been parsed. Mirrors [`GroupSlot`], but
/// for matchspec strings rather than PEP 508 ones -- kept as a separate
/// type (rather than a third `GroupSlot` variant) so that
/// [`extract_groups`] and [`extract_conda_groups`] each only ever produce
/// slot variants their own reassembly loop knows how to handle, with no
/// impossible variant either loop would otherwise need to guard against.
enum CondaGroupSlot<'a> {
    /// A literal conda `MatchSpec` string: `(original array index, raw
    /// string)`.
    Matchspec(usize, &'a str),
    /// `{ include-group = "<name>" }`. No parsing needed, so this is
    /// resolved eagerly during the walk rather than deferred. Resolves
    /// against the same merged group graph as a `[dependency-groups]`
    /// `include-group` entry -- see [`ProjectRequirements`]'s docs.
    Include(GroupName),
}

/// `[tool.ana.matchspec-dependency-groups]`. See [`extract_group_table`]
/// for the shape/error handling this delegates to; this just types the
/// walk's raw [`GroupTableEntry`]s as [`CondaGroupSlot`]s.
///
/// A group name here is checked for duplicates only *within this table*
/// -- the same name also appearing in `[dependency-groups]` is not a
/// duplicate, it's how a group's PEP 508 and matchspec entries end up
/// merged; see `Pyproject::parse`. [`extract_group_table`]'s duplicate
/// check is per call (a fresh `groups` map each time), so this holds
/// automatically -- [`extract_groups`]'s call is a separate walk with its
/// own map.
fn extract_conda_groups<'a>(
    doc: &'a Document<&str>,
) -> Result<IndexMap<GroupName, Vec<CondaGroupSlot<'a>>>, InvalidField> {
    let Some(ana) = ana_table(doc) else {
        return Ok(IndexMap::new());
    };
    let Some(item) = ana.get("matchspec-dependency-groups") else {
        return Ok(IndexMap::new());
    };
    let Some(table) = item.as_table_like() else {
        return Err(InvalidField::new(
            "tool.ana.matchspec-dependency-groups",
            None,
        ));
    };

    Ok(
        extract_group_table(table, "tool.ana.matchspec-dependency-groups")?
            .into_iter()
            .map(|(name, entries)| {
                let slots = entries
                    .into_iter()
                    .map(|entry| match entry {
                        GroupTableEntry::Literal(i, s) => CondaGroupSlot::Matchspec(i, s),
                        GroupTableEntry::Include(target) => CondaGroupSlot::Include(target),
                    })
                    .collect();
                (name, slots)
            })
            .collect(),
    )
}

/// A project's dependency declarations: the sources of requirements in a
/// modern `pyproject.toml`, plus ana's own `[tool.ana]` matchspec
/// extension.
///
/// ## The `tool.ana` matchspec extension
///
/// `[tool.ana.matchspec-dependencies]` and
/// `[tool.ana.matchspec-dependency-groups]` declare conda `MatchSpec`
/// requirements alongside `[project.dependencies]` and
/// `[dependency-groups]`'s PEP 508 ones. They're merged into `runtime` and
/// `groups` below rather than kept in separate fields, forming one unified
/// dependency graph: a single `include-group` reference or self-
/// referential-extra check walks one graph, not two, and a group present
/// in both `[dependency-groups]` and
/// `[tool.ana.matchspec-dependency-groups]` is one group with a mixed-type
/// entry list, not two same-named groups a caller has to remember to
/// union themselves.
///
/// The merge order is deterministic and simple: for `runtime`, and for
/// each group, every PEP 508 entry comes first (in file order), then
/// every matchspec entry (in file order) -- no attempt is made to
/// interleave the two by original position, matching this crate's
/// existing "positional, undeduplicated" stance on `include-group`
/// expansion.
///
/// There is deliberately no matchspec equivalent of
/// `[project.optional-dependencies]` (extras) -- see [`Dependency`]'s
/// docs for why a matchspec entry never needs one.
///
/// [`Dependency`]: crate::resolution::Dependency
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRequirements {
    /// `[project.dependencies]` (PEP 508) merged with
    /// `[tool.ana.matchspec-dependencies]` (conda `MatchSpec`) -- the
    /// package's mandatory dependencies. PEP 508 entries first, then
    /// matchspec entries; see this struct's docs for the merge order.
    pub runtime: Vec<Dependency>,
    /// `[project.optional-dependencies]`, keyed by normalized extra name,
    /// with self-referential extras (`myproj[gui]` inside `myproj`'s own
    /// metadata) already expanded.
    pub extras: IndexMap<ExtraName, Vec<Requirement>>,
    /// `[dependency-groups]` (PEP 508) merged with
    /// `[tool.ana.matchspec-dependency-groups]` (conda `MatchSpec`), keyed
    /// by normalized group name, with `{include-group = "..."}` references
    /// already expanded (positionally, undeduplicated, per PEP 735). A
    /// group present in only one of the two tables is unaffected; a group
    /// present in both has its PEP 508 entries first, then its matchspec
    /// entries -- see this struct's docs for the merge order.
    pub groups: IndexMap<GroupName, Vec<Dependency>>,
}

/// Every invalid field found in one `pyproject.toml`. Never constructed
/// with an empty field list -- if nothing is invalid, parsing succeeds.
///
/// Carries either exactly one field, or several, and which one it is
/// tells you what kind of problem was found:
///
/// - **One field** means a structural check failed -- a missing/invalid
///   `project.name`, a rejected `dynamic`, the legacy-Poetry tell, or a
///   wrong-shaped/duplicate-named `dependencies`/`optional-dependencies`/
///   `dependency-groups` section. [`Pyproject::parse`] returns on the
///   first one found, so there is never more than one.
/// - **One or more fields**, every one a PEP 508 requirement *parse*
///   failure, means every structural check above already passed, and
///   every invalid requirement string is collected rather than just the
///   first -- no reason to make someone fix-and-rerun once per bad one.
///
/// These two cases never mix in a single [`PyprojectError`]. Parse
/// failures are listed in flattened document order (`dependencies`, then
/// each extra, then each group), not a global sort by path, since that
/// would mean either giving up the single-parallel-region parse (see the
/// module docs) or paying for errors per-section as they're discovered.
#[derive(Debug)]
pub struct PyprojectError {
    fields: Vec<InvalidField>,
}

impl PyprojectError {
    fn new(fields: Vec<InvalidField>) -> Self {
        debug_assert!(
            !fields.is_empty(),
            "PyprojectError must carry at least one invalid field"
        );
        Self { fields }
    }

    /// All invalid fields, in document order.
    pub fn fields(&self) -> &[InvalidField] {
        &self.fields
    }
}

impl From<InvalidField> for PyprojectError {
    /// Wraps a single structural-check failure. See [`PyprojectError`]'s
    /// docs for why a single field is exactly what every structural check
    /// in this module produces.
    fn from(field: InvalidField) -> Self {
        Self::new(vec![field])
    }
}

impl Display for PyprojectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid pyproject.toml:")?;
        for field in &self.fields {
            writeln!(f, "  {field}")?;
        }
        Ok(())
    }
}

impl std::error::Error for PyprojectError {}

/// A single invalid field: where it is, plus optional detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidField {
    /// Dotted TOML path, with arrays addressed by index --
    /// `project.name`, `project.dependencies[2]`, `dependency-groups.dev[0]`.
    /// The empty path means the document itself (a TOML syntax error).
    /// Intended for human consumption, not machine navigation.
    pub path: String,
    /// Optional detail: the offending value and why it was rejected (PEP 508
    /// parser message, duplicate normalized name, resolution cycle trace).
    /// `None` means a bare "not valid".
    pub description: Option<String>,
}

impl InvalidField {
    fn new(path: &str, description: Option<String>) -> Self {
        Self {
            path: path.to_string(),
            description,
        }
    }
}

impl Display for InvalidField {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let path = if self.path.is_empty() {
            "document"
        } else {
            self.path.as_str()
        };
        match &self.description {
            Some(description) => write!(f, "{path} not valid: {description}"),
            None => write!(f, "{path} not valid"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! End-to-end tests for [`Pyproject::parse`]: TOML text in, typed
    //! `Pyproject` (or an aggregated field-error list) out.

    use std::str::FromStr;

    use super::*;

    fn req(spec: &str) -> Requirement {
        Requirement::from_str(spec).unwrap()
    }

    /// A [`Dependency::Pep508`] wrapping `req(spec)`, for comparing against
    /// `.runtime`/`.groups`, which hold the unified [`Dependency`] type.
    /// `.extras` stays plain `Requirement`-typed (see `ProjectRequirements`'s
    /// docs), so callers comparing against it should keep using `req(...)`.
    fn dep(spec: &str) -> Dependency {
        Dependency::Pep508(req(spec))
    }

    /// A [`Dependency::Matchspec`] wrapping a parsed conda `MatchSpec`
    /// string, for comparing against `.runtime`/`.groups` entries that came
    /// from `[tool.ana.matchspec-dependencies]`/
    /// `[tool.ana.matchspec-dependency-groups]`.
    fn matchspec_dep(spec: &str) -> Dependency {
        Dependency::Matchspec(Box::new(ana_dependency::parse_matchspec(spec).unwrap()))
    }

    fn extra(name: &str) -> ExtraName {
        ExtraName::from_str(name).unwrap()
    }

    fn group(name: &str) -> GroupName {
        GroupName::from_str(name).unwrap()
    }

    fn package(name: &str) -> PackageName {
        PackageName::from_str(name).unwrap()
    }

    fn parse_ok(toml: &str) -> Pyproject {
        Pyproject::parse(toml).unwrap()
    }

    fn parse_err(toml: &str) -> Vec<InvalidField> {
        Pyproject::parse(toml).unwrap_err().fields().to_vec()
    }

    /// An expected `InvalidField` with no description.
    fn invalid(path: &str) -> InvalidField {
        InvalidField {
            path: path.to_string(),
            description: None,
        }
    }

    fn paths(fields: &[InvalidField]) -> Vec<&str> {
        fields.iter().map(|f| f.path.as_str()).collect()
    }

    // -----------------------------------------------------------------------
    // Valid documents
    // -----------------------------------------------------------------------

    #[test]
    fn minimal_project() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests", "click>=8"]
"#,
        );
        assert_eq!(p.name, package("myproj"));
        assert_eq!(
            p.requirements.runtime,
            vec![dep("requests"), dep("click>=8")]
        );
        assert!(p.requirements.extras.is_empty());
        assert!(p.requirements.groups.is_empty());
    }

    /// A trailing comma in a version specifier list (`"foo>=1,<2,"`) used to
    /// be a `uv_pep508` parse error. uv#19806 ("Allow trailing commas in
    /// version specifiers"), picked up by this crate's `uv-pep508` bump to
    /// `0.12.6`, relaxed the grammar to accept exactly one trailing comma in
    /// both the bare and parenthesized specifier-list forms.
    #[test]
    fn trailing_comma_in_version_specifiers_is_now_accepted() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests>=1,<2,"]
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests>=1,<2,")]);
    }

    #[test]
    fn missing_dependencies_key_is_empty_runtime() {
        // A `[project]` table with no `dependencies` key at all means zero
        // runtime dependencies, not an error.
        let p = parse_ok(
            r#"
[project]
name = "myproj"
"#,
        );
        assert_eq!(p.name, package("myproj"));
        assert!(p.requirements.runtime.is_empty());
    }

    #[test]
    fn empty_dependencies() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = []
"#,
        );
        assert!(p.requirements.runtime.is_empty());
    }

    #[test]
    fn full_project() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]

[project.optional-dependencies]
test = ["pytest"]

[dependency-groups]
dev = ["ruff"]
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests")]);
        assert_eq!(p.requirements.extras[&extra("test")], vec![req("pytest")]);
        assert_eq!(p.requirements.groups[&group("dev")], vec![dep("ruff")]);
    }

    #[test]
    fn dynamic_with_unrelated_keys_is_fine() {
        // Only `dependencies`/`optional-dependencies` in `dynamic` are
        // rejection-worthy; `version`/`readme` etc. are ignored.
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dynamic = ["version", "readme"]
"#,
        );
        assert_eq!(p.name, package("myproj"));
    }

    #[test]
    fn unrelated_fields_are_ignored() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
version = "1.0"
readme = "README.md"
dependencies = ["requests"]

[tool.ana]
whatever = true

[tool.setuptools]
packages = ["x"]
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests")]);
        assert_eq!(p.requires_python, None);
    }

    #[test]
    fn requires_python_is_parsed() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
requires-python = ">=3.9"
"#,
        );
        assert_eq!(
            p.requires_python,
            Some(uv_pep440::VersionSpecifiers::from_str(">=3.9").unwrap())
        );
    }

    #[test]
    fn requires_python_wrong_shape_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
requires-python = [">=3.9"]
"#,
        );
        assert_eq!(paths(&fields), vec!["project.requires-python"]);
    }

    #[test]
    fn requires_python_unparseable_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
requires-python = "not a specifier"
"#,
        );
        assert_eq!(paths(&fields), vec!["project.requires-python"]);
    }

    #[test]
    fn dynamic_requires_python_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dynamic = ["requires-python"]
"#,
        );
        assert_eq!(paths(&fields), vec!["project.dynamic"]);
    }

    #[test]
    fn markers_and_extras_on_requirements_are_preserved() {
        // PEP 508 markers are static data -- evaluated downstream at
        // matchspec-generation time, not a parse-stage concern.
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["django>2; os_name != 'nt'", "foo[gui,cli]"]
"#,
        );
        assert_eq!(
            p.requirements.runtime,
            vec![dep("django>2; os_name != 'nt'"), dep("foo[gui,cli]")]
        );
    }

    #[test]
    fn source_order_is_preserved() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["zeta", "alpha", "middle"]
"#,
        );
        assert_eq!(
            p.requirements.runtime,
            vec![dep("zeta"), dep("alpha"), dep("middle")]
        );
    }

    #[test]
    fn poetry_table_with_project_dependencies_is_fine() {
        // Poetry 2.0+ emits a standard `[project]` table; when
        // `[project.dependencies]` exists we don't care the backend
        // happens to be poetry-core.
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]

[tool.poetry.dependencies]
requests = "^2.0"
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests")]);
    }

    #[test]
    fn tool_poetry_without_dependencies_is_fine() {
        // The legacy tell is `[tool.poetry.dependencies]` specifically, not
        // the mere presence of a `[tool.poetry]` table.
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]

[tool.poetry]
name = "myproj"
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests")]);
    }

    #[test]
    fn self_referential_extra_expands() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
gui = ["tkinter"]
cli = ["click"]
all = ["myproj[gui,cli]"]
"#,
        );
        assert_eq!(
            p.requirements.extras[&extra("all")],
            vec![req("tkinter"), req("click")]
        );
    }

    #[test]
    fn self_referential_extra_in_group_expands() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
test = ["pytest"]

[dependency-groups]
dev = ["myproj[test]"]
"#,
        );
        assert_eq!(p.requirements.groups[&group("dev")], vec![dep("pytest")]);
    }

    #[test]
    fn include_group_expands_positionally_without_dedup() {
        // PEP 735: includes expand in place and do not deduplicate -- both
        // `foo` entries pass through; dedup is the solver's problem.
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[dependency-groups]
a = ["foo"]
b = ["bar", {include-group = "a"}, "foo"]
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("b")],
            vec![dep("bar"), dep("foo"), dep("foo")]
        );
    }

    #[test]
    fn include_group_reference_is_normalized() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev-tools = ["ruff"]
all = [{include-group = "dev_tools"}]
"#,
        );
        assert_eq!(p.requirements.groups[&group("all")], vec![dep("ruff")]);
    }

    #[test]
    fn extra_reference_is_normalized() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
group_one = ["anyio"]
all = ["myproj[Group-One]"]
"#,
        );
        assert_eq!(p.requirements.extras[&extra("all")], vec![req("anyio")]);
    }

    #[test]
    fn project_name_is_normalized_for_self_reference() {
        let p = parse_ok(
            r#"
[project]
name = "MyProj"

[project.optional-dependencies]
test = ["pytest"]

[dependency-groups]
dev = ["myproj[test]"]
"#,
        );
        assert_eq!(p.name, package("myproj"));
        assert_eq!(p.requirements.groups[&group("dev")], vec![dep("pytest")]);
    }

    #[test]
    fn runtime_self_reference_passes_through_unexpanded() {
        // `resolve()` only expands inside extras/groups; a self-reference
        // in `[project.dependencies]` stays a literal requirement on the
        // project.
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["myproj[test]"]

[project.optional-dependencies]
test = ["pytest"]
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("myproj[test]")]);
        assert_eq!(p.requirements.extras[&extra("test")], vec![req("pytest")]);
    }

    #[test]
    fn extra_and_group_with_same_name_are_independent() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
dev = ["pytest"]

[dependency-groups]
dev = ["ruff"]
"#,
        );
        assert_eq!(p.requirements.extras[&extra("dev")], vec![req("pytest")]);
        assert_eq!(p.requirements.groups[&group("dev")], vec![dep("ruff")]);
    }

    #[test]
    fn empty_group_is_fine() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = []
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("dev")],
            Vec::<Dependency>::new()
        );
    }

    // -----------------------------------------------------------------------
    // `tool.ana` matchspec extension
    // -----------------------------------------------------------------------

    #[test]
    fn matchspec_dependencies_are_parsed() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]

[tool.ana]
matchspec-dependencies = ["numpy >=1.26", "mkl"]
"#,
        );
        assert_eq!(
            p.requirements.runtime,
            vec![
                dep("requests"),
                matchspec_dep("numpy >=1.26"),
                matchspec_dep("mkl")
            ]
        );
    }

    #[test]
    fn missing_matchspec_dependencies_key_is_empty() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]

[tool.ana]
whatever = true
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests")]);
    }

    #[test]
    fn missing_tool_ana_table_is_empty() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]
"#,
        );
        assert_eq!(p.requirements.runtime, vec![dep("requests")]);
    }

    #[test]
    fn matchspec_dependency_groups_are_parsed() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
build = ["compilers", "cmake"]
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("build")],
            vec![matchspec_dep("compilers"), matchspec_dep("cmake")]
        );
    }

    /// A group present in both `[dependency-groups]` and
    /// `[tool.ana.matchspec-dependency-groups]` merges into one group: PEP
    /// 508 entries first, then matchspec entries.
    #[test]
    fn same_named_groups_merge_pep508_then_matchspec() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = ["ruff", "pytest"]

[tool.ana.matchspec-dependency-groups]
dev = ["compilers", "cmake"]
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("dev")],
            vec![
                dep("ruff"),
                dep("pytest"),
                matchspec_dep("compilers"),
                matchspec_dep("cmake"),
            ]
        );
    }

    /// A group present only in `[tool.ana.matchspec-dependency-groups]`
    /// (no corresponding `[dependency-groups]` entry) is a conda-only
    /// group, appended after every PEP 508-declared group.
    #[test]
    fn matchspec_only_group_is_independent() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = ["ruff"]

[tool.ana.matchspec-dependency-groups]
gpu = ["cudatoolkit=11.8"]
"#,
        );
        assert_eq!(p.requirements.groups[&group("dev")], vec![dep("ruff")]);
        assert_eq!(
            p.requirements.groups[&group("gpu")],
            vec![matchspec_dep("cudatoolkit=11.8")]
        );
    }

    /// `{include-group = "..."}` inside `[tool.ana.matchspec-dependency-groups]`
    /// resolves against the same merged graph as a `[dependency-groups]`
    /// include -- here, referencing a matchspec-only group.
    #[test]
    fn matchspec_include_group_resolves_against_merged_graph() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
build = ["compilers"]
dev = ["mamba", {include-group = "build"}]
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("dev")],
            vec![matchspec_dep("mamba"), matchspec_dep("compilers")]
        );
    }

    /// A `[dependency-groups]` `include-group` can reference a group that
    /// only exists in `[tool.ana.matchspec-dependency-groups]`, and vice
    /// versa -- there is exactly one graph, so there is no "wrong side" to
    /// include from.
    #[test]
    fn include_group_crosses_between_pep508_and_matchspec_tables() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = ["ruff", {include-group = "build"}]

[tool.ana.matchspec-dependency-groups]
build = ["compilers"]
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("dev")],
            vec![dep("ruff"), matchspec_dep("compilers")]
        );
    }

    /// A matchspec entry never triggers self-referential-extra expansion,
    /// even if it names the project itself with bracket `extras=[...]`
    /// syntax -- see `Dependency`'s docs for why. It's always pushed
    /// through unchanged.
    #[test]
    fn matchspec_self_reference_passes_through_unexpanded() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
dev = ["myproj[extras=[test]]"]
"#,
        );
        assert_eq!(
            p.requirements.groups[&group("dev")],
            vec![matchspec_dep("myproj[extras=[test]]")]
        );
    }

    #[test]
    fn matchspec_dependencies_not_an_array_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = "numpy"
"#,
        );
        assert_eq!(fields, vec![invalid("tool.ana.matchspec-dependencies")]);
    }

    #[test]
    fn matchspec_dependencies_non_string_entry_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = [123]
"#,
        );
        assert_eq!(fields, vec![invalid("tool.ana.matchspec-dependencies[0]")]);
    }

    #[test]
    fn matchspec_dependency_groups_not_a_table_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependency-groups = ["nope"]
"#,
        );
        assert_eq!(
            fields,
            vec![invalid("tool.ana.matchspec-dependency-groups")]
        );
    }

    #[test]
    fn duplicate_matchspec_group_names_after_normalization_are_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
dev-tools = ["a"]
dev_tools = ["b"]
"#,
        );
        assert_eq!(
            fields,
            vec![InvalidField {
                path: "tool.ana.matchspec-dependency-groups".to_string(),
                description: Some("duplicate group name `dev-tools`".to_string()),
            }]
        );
    }

    #[test]
    fn invalid_matchspec_string_is_reported() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = ["this is not [ a valid matchspec"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "tool.ana.matchspec-dependencies[0]");
        assert!(fields[0].description.is_some());
    }

    /// `[tool.ana.matchspec-dependencies]` mirrors `[project.dependencies]`
    /// -- name/version/build/extras, nothing more. A PEP 508 requirement
    /// string has no way to name a conda channel, so a matchspec entry
    /// that sets one (`channel::name` syntax) would be a strictly more
    /// powerful, unvalidated capability smuggled in under the guise of
    /// mirroring the PEP 508 table: it could redirect a locked/solved
    /// dependency to fetch from a channel the rest of the project never
    /// declared trust in. Rejected instead.
    #[test]
    fn matchspec_dependency_with_explicit_channel_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = ["conda-forge::numpy"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "tool.ana.matchspec-dependencies[0]");
        assert!(fields[0].description.is_some());
    }

    /// Same rationale as `matchspec_dependency_with_explicit_channel_is_rejected`,
    /// for a matchspec that pins an exact package URL instead of a
    /// channel -- an even more direct way to name an arbitrary fetch
    /// location, and equally inexpressible in PEP 508.
    #[test]
    fn matchspec_dependency_with_explicit_url_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana]
matchspec-dependencies = [
    "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.26.0-py311h64a7726_0.conda",
]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "tool.ana.matchspec-dependencies[0]");
        assert!(fields[0].description.is_some());
    }

    /// The channel/url rejection applies identically to
    /// `[tool.ana.matchspec-dependency-groups]` -- it's enforced in
    /// `next_parsed_matchspec`, shared by both extraction paths, not
    /// re-implemented per table.
    #[test]
    fn matchspec_group_dependency_with_explicit_channel_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
build = ["conda-forge::compilers"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields[0].path,
            "tool.ana.matchspec-dependency-groups.build[0]"
        );
        assert!(fields[0].description.is_some());
    }

    /// PEP 508 and matchspec parse failures both surface in the same
    /// aggregated [`PyprojectError`], since both come from the same
    /// "parse every literal string, collect every failure" tier -- see
    /// the module docs.
    #[test]
    fn pep508_and_matchspec_errors_are_both_aggregated() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = ["this is not pep 508 either!!"]

[tool.ana]
matchspec-dependencies = ["this is not [ a valid matchspec"]
"#,
        );
        assert_eq!(
            paths(&fields),
            vec![
                "project.dependencies[0]",
                "tool.ana.matchspec-dependencies[0]",
            ]
        );
    }

    #[test]
    fn invalid_matchspec_in_group_is_reported() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
dev = ["this is not [ a valid matchspec"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields[0].path,
            "tool.ana.matchspec-dependency-groups.dev[0]"
        );
        assert!(fields[0].description.is_some());
    }

    #[test]
    fn matchspec_group_include_missing_target_is_rejected() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.ana.matchspec-dependency-groups]
dev = [{include-group = "nonexistent"}]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
    }

    // -----------------------------------------------------------------------
    // Structural errors
    // -----------------------------------------------------------------------

    #[test]
    fn missing_project_table() {
        let fields = parse_err(
            r#"
[build-system]
requires = ["setuptools"]
"#,
        );
        assert_eq!(fields, vec![invalid("project")]);
    }

    #[test]
    fn empty_document() {
        assert_eq!(parse_err(""), vec![invalid("project")]);
    }

    #[test]
    fn project_not_a_table() {
        let fields = parse_err(r#"project = "hello""#);
        assert_eq!(fields, vec![invalid("project")]);
    }

    #[test]
    fn missing_name() {
        let fields = parse_err(
            r#"
[project]
dependencies = ["requests"]
"#,
        );
        assert_eq!(fields, vec![invalid("project.name")]);
    }

    #[test]
    fn name_wrong_type() {
        let fields = parse_err(
            r#"
[project]
name = 42
"#,
        );
        assert_eq!(fields, vec![invalid("project.name")]);
    }

    #[test]
    fn name_not_a_valid_package_name() {
        let fields = parse_err(
            r#"
[project]
name = "not a valid name!"
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.name");
        assert!(fields[0].description.is_some());
    }

    #[test]
    fn name_empty() {
        let fields = parse_err(
            r#"
[project]
name = ""
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.name");
    }

    #[test]
    fn dynamic_dependencies() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dynamic = ["dependencies"]
"#,
        );
        assert_eq!(fields, vec![invalid("project.dynamic")]);
    }

    #[test]
    fn dynamic_optional_dependencies() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dynamic = ["optional-dependencies"]
"#,
        );
        assert_eq!(fields, vec![invalid("project.dynamic")]);
    }

    #[test]
    fn dynamic_dependencies_rejected_even_with_static_value() {
        // The spec allows static + dynamic simultaneously (backend
        // appends) -- we reject unconditionally, since we can't tell a
        // complete list from a prefix.
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = ["requests"]
dynamic = ["dependencies"]
"#,
        );
        assert_eq!(fields, vec![invalid("project.dynamic")]);
    }

    #[test]
    fn dynamic_wrong_type() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dynamic = "dependencies"
"#,
        );
        assert_eq!(fields, vec![invalid("project.dynamic")]);
    }

    #[test]
    fn dynamic_non_string_entry() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dynamic = [42]
"#,
        );
        assert_eq!(fields, vec![invalid("project.dynamic")]);
    }

    #[test]
    fn poetry_dependencies_without_project_dependencies() {
        // Legacy Poetry 1.x tell: `[tool.poetry.dependencies]` with no
        // `[project.dependencies]`. Must error even though a missing
        // `dependencies` key is otherwise fine.
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[tool.poetry.dependencies]
python = "^3.9"
requests = "^2.0"
"#,
        );
        assert_eq!(fields, vec![invalid("tool.poetry.dependencies")]);
    }

    #[test]
    fn toml_syntax_error() {
        let fields = parse_err("[project\nname = \"myproj\"");
        assert_eq!(fields.len(), 1);
        assert!(fields[0].path.is_empty());
        assert!(fields[0].description.is_some());
    }

    // -----------------------------------------------------------------------
    // Wrong types in dependency lists
    // -----------------------------------------------------------------------

    #[test]
    fn dependencies_not_an_array() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = "requests"
"#,
        );
        assert_eq!(fields, vec![invalid("project.dependencies")]);
    }

    #[test]
    fn dependencies_non_string_entry() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = [42]
"#,
        );
        assert_eq!(fields, vec![invalid("project.dependencies[0]")]);
    }

    #[test]
    fn dependencies_table_entry() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = [{name = "requests"}]
"#,
        );
        assert_eq!(fields, vec![invalid("project.dependencies[0]")]);
    }

    #[test]
    fn optional_dependencies_not_a_table() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
optional-dependencies = ["pytest"]
"#,
        );
        assert_eq!(fields, vec![invalid("project.optional-dependencies")]);
    }

    #[test]
    fn extra_not_an_array() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
test = "pytest"
"#,
        );
        assert_eq!(fields, vec![invalid("project.optional-dependencies.test")]);
    }

    #[test]
    fn extra_table_value() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
test = {x = 1}
"#,
        );
        assert_eq!(fields, vec![invalid("project.optional-dependencies.test")]);
    }

    #[test]
    fn extra_non_string_entry() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
test = ["pytest", 42]
"#,
        );
        assert_eq!(
            fields,
            vec![invalid("project.optional-dependencies.test[1]")]
        );
    }

    #[test]
    fn dependency_groups_not_a_table() {
        // `dependency-groups` is a top-level table (PEP 735), not nested
        // under `[project]` -- it must appear before the `[project]`
        // header (or after, via its own `[dependency-groups]` header) to
        // actually land at the document root.
        let fields = parse_err(
            r#"
dependency-groups = ["pytest"]

[project]
name = "myproj"
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups")]);
    }

    #[test]
    fn group_not_an_array() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = "pytest"
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev")]);
    }

    #[test]
    fn group_table_value() {
        // A group is a *list* of specifiers; a bare `{include-group = ...}`
        // table as the group value itself is not legal.
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = {include-group = "base"}
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev")]);
    }

    #[test]
    fn group_entry_int() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [42]
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev[0]")]);
    }

    #[test]
    fn group_entry_nested_array() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [["pytest"]]
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev[0]")]);
    }

    #[test]
    fn group_entry_include_group_wrong_type() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [{include-group = 42}]
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev[0]")]);
    }

    #[test]
    fn group_entry_table_with_extra_keys() {
        // PEP 735: tools SHOULD error on unrecognized data in dependency
        // groups rather than silently skip it.
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [{include-group = "base", foo = 1}]
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev[0]")]);
    }

    #[test]
    fn group_entry_table_with_wrong_key() {
        // `include_group` (underscore) is not the PEP 735 key.
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [{include_group = "base"}]
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev[0]")]);
    }

    #[test]
    fn group_entry_empty_table() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [{}]
"#,
        );
        assert_eq!(fields, vec![invalid("dependency-groups.dev[0]")]);
    }

    // -----------------------------------------------------------------------
    // Requirement parse errors
    // -----------------------------------------------------------------------

    #[test]
    fn invalid_requirement_in_dependencies() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = ["requests >< 1.0"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.dependencies[0]");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("requests >< 1.0"));
    }

    /// An extra name ending in a bare separator (`-`, `_`, or `.` with no
    /// alphanumeric character after it) used to make `uv_pep508`'s
    /// `Requirement::from_str` *panic* -- not return an `Err` -- under this
    /// crate's old `uv-pep508` `0.9.7` pin: the extra-name scanner accepted
    /// the trailing separator and then assumed `ExtraName` construction
    /// from the scanned text could not fail. That's a real violation of
    /// this module's contract that untrusted `pyproject.toml` content
    /// should never panic, only report an [`InvalidField`].
    ///
    /// Fixed by the `uv-pep508` bump to `0.12.6`: uv#19779 validates the
    /// extra name ends in an alphanumeric character before constructing
    /// `ExtraName`, turning this into an ordinary parse error. Covers all
    /// three separators (`-`, `_`, `.`), which shared the identical panic
    /// path.
    #[test]
    fn invalid_requirement_extra_with_trailing_separator_no_longer_panics() {
        for (separator, dependency) in [
            ('-', "requests[bar-]"),
            ('_', "requests[bar_]"),
            ('.', "requests[bar.]"),
        ] {
            let fields = parse_err(&format!(
                r#"
[project]
name = "myproj"
dependencies = ["{dependency}"]
"#,
            ));
            assert_eq!(fields.len(), 1, "separator {separator:?}: {fields:?}");
            assert_eq!(fields[0].path, "project.dependencies[0]");
            assert!(
                fields[0]
                    .description
                    .as_deref()
                    .unwrap()
                    .contains("alphanumeric"),
                "separator {separator:?}: {fields:?}"
            );
        }
    }

    /// A reversed-operand compatible-release marker against a pure string
    /// field (`"posix" ~= os_name`) used to make `Requirement::from_str`
    /// panic the same way as
    /// [`invalid_requirement_extra_with_trailing_separator_no_longer_panics`]
    /// above -- a marker-algebra `unreachable!()` in `uv_pep508` `0.9.7`,
    /// fixed by uv#19782 in the same bump to `0.12.6`. Unlike the
    /// trailing-separator case, the fixed behavior is *not* a parse error:
    /// the marker is silently treated as `MarkerTree::TRUE`, so the
    /// document parses successfully with the marker simply absent.
    #[test]
    fn requirement_with_reversed_compatible_release_string_marker_no_longer_panics() {
        let p = parse_ok(
            r#"
[project]
name = "myproj"
dependencies = ["requests>=2.0.0; \"posix\" ~= os_name"]
"#,
        );
        assert_eq!(p.requirements.runtime.len(), 1);
        match &p.requirements.runtime[0] {
            Dependency::Pep508(req) => assert!(req.marker.is_true()),
            Dependency::Matchspec(_) => panic!("expected a Pep508 dependency"),
        }
    }

    #[test]
    fn invalid_requirement_in_extra() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
test = ["foo["]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.optional-dependencies.test[0]");
        assert!(fields[0].description.as_deref().unwrap().contains("foo["));
    }

    #[test]
    fn invalid_requirement_in_group() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = ["== 1.0"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups.dev[0]");
        assert!(fields[0].description.as_deref().unwrap().contains("== 1.0"));
    }

    #[test]
    fn multiple_invalid_requirements_all_reported() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = ["requests >< 1.0", "flask", "foo["]

[project.optional-dependencies]
test = ["== 1.0"]

[dependency-groups]
dev = ["pytest", "bar)"]
"#,
        );
        assert_eq!(
            paths(&fields),
            [
                "project.dependencies[0]",
                "project.dependencies[2]",
                "project.optional-dependencies.test[0]",
                "dependency-groups.dev[1]",
            ]
        );
        assert!(fields.iter().all(|f| f.description.is_some()));
    }

    // -----------------------------------------------------------------------
    // Duplicate names after normalization
    // -----------------------------------------------------------------------

    #[test]
    fn duplicate_group_names_after_normalization() {
        // PEP 735: duplicate group names after normalization are an error,
        // not "last one wins".
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev-tools = ["ruff"]
dev_tools = ["mypy"]
DEV_TOOLS = ["pytest"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("dev-tools"));
    }

    #[test]
    fn duplicate_extra_names_after_normalization() {
        // PEP 621 doesn't mandate it, but silently merging/overwriting is
        // worse than erroring -- symmetric with the group rule.
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
GUI = ["tkinter"]
gui = ["pyqt"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.optional-dependencies");
        assert!(fields[0].description.as_deref().unwrap().contains("gui"));
    }

    // -----------------------------------------------------------------------
    // Resolution errors (cycles, missing references)
    // -----------------------------------------------------------------------

    #[test]
    fn include_group_cycle() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
a = [{include-group = "b"}]
b = [{include-group = "a"}]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("Cycles are not supported"));
    }

    #[test]
    fn include_group_self_cycle() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [{include-group = "dev"}]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("Cycles are not supported"));
    }

    #[test]
    fn include_group_long_acyclic_chain_is_rejected_not_crashed() {
        // Regression test for a stack-overflow DoS: a long but fully
        // acyclic `include-group` chain (every name distinct, so the cycle
        // check never fires) used to recurse with no bound until the
        // process aborted. `resolution::MAX_RESOLUTION_DEPTH` now bounds
        // this; 200 links is well past any plausible legitimate nesting
        // without this test needing to know the exact limit.
        const CHAIN_LEN: usize = 200;
        let mut toml = String::from("[project]\nname = \"myproj\"\n\n[dependency-groups]\n");
        for i in 0..CHAIN_LEN {
            if i + 1 < CHAIN_LEN {
                toml += &format!("g{i} = [{{include-group = \"g{}\"}}]\n", i + 1);
            } else {
                toml += &format!("g{i} = [\"leaf\"]\n");
            }
        }
        let fields = parse_err(&toml);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("maximum reference depth"));
    }

    #[test]
    fn include_group_missing_target() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = [{include-group = "nonexistent"}]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("Failed to find dependency group `nonexistent`"));
    }

    #[test]
    fn self_referential_extra_cycle() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
alpha = ["myproj[iota]"]
iota = ["myproj[alpha]"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.optional-dependencies");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("Cycles are not supported"));
    }

    #[test]
    fn self_referential_extra_missing_target() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[project.optional-dependencies]
all = ["myproj[nope]"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "project.optional-dependencies");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("Failed to find optional dependency `nope`"));
    }

    #[test]
    fn group_referencing_missing_extra() {
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
dev = ["myproj[nope]"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups");
        assert!(fields[0]
            .description
            .as_deref()
            .unwrap()
            .contains("Failed to find optional dependency `nope`"));
    }

    #[test]
    fn resolution_error_discarded_when_field_errors_exist() {
        // A bad requirement string means the group maps are partial;
        // running resolution on them could report misleading "not found"
        // errors, so only the field error comes back. This proves that
        // when resolution *would also fail* (the b<->c cycle here), that
        // failure never leaks into the output. It doesn't prove resolution
        // wasn't attempted at all -- see
        // `resolution_never_run_when_field_errors_exist_even_if_it_would_succeed`
        // below for that.
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
a = ["foo >< 1.0"]
b = [{include-group = "c"}]
c = [{include-group = "b"}]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups.a[0]");
    }

    #[test]
    fn resolution_never_run_when_field_errors_exist_even_if_it_would_succeed() {
        // Unlike the test above, `dev` here is entirely independent of `a`
        // and has no cycle or missing reference -- resolving it in
        // isolation would succeed outright. This pins down that resolution
        // is genuinely skipped (not just run and its successful result
        // discarded) when a field error already exists: no trace of `dev`
        // comes back alongside the one real problem (`a`'s bad requirement
        // string).
        let fields = parse_err(
            r#"
[project]
name = "myproj"

[dependency-groups]
a = ["foo >< 1.0"]
dev = ["pytest"]
"#,
        );
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].path, "dependency-groups.a[0]");
    }

    // -----------------------------------------------------------------------
    // Fail-fast across independent structural checks
    // -----------------------------------------------------------------------

    #[test]
    fn first_structural_error_short_circuits_the_rest() {
        // `project.name` is checked first (phase order: name, dynamic,
        // legacy Poetry, dependencies, optional-dependencies,
        // dependency-groups). This document also has a duplicate extra
        // name and a dependency string that would fail to parse, but
        // neither is ever reached: `Pyproject::parse` returns on the first
        // structural problem found.
        let fields = parse_err(
            r#"
[project]
name = 42
dependencies = ["requests >< 1.0"]

[project.optional-dependencies]
GUI = ["tkinter"]
gui = ["pyqt"]
"#,
        );
        assert_eq!(fields, vec![invalid("project.name")]);
    }

    #[test]
    fn structural_error_short_circuits_before_requirement_parsing() {
        // `project.name` is valid here, so the walk gets further than the
        // previous test -- but `optional-dependencies.test` being the
        // wrong shape is a structural problem found before `dependencies`
        // is ever handed to the PEP 508 parser. Asserting there's exactly
        // one field, and that it's the shape error, proves parsing was
        // skipped rather than merely "also correct" (if it ran,
        // `requests >< 1.0` would fail too).
        let fields = parse_err(
            r#"
[project]
name = "myproj"
dependencies = ["requests >< 1.0"]

[project.optional-dependencies]
test = "pytest"
"#,
        );
        assert_eq!(fields, vec![invalid("project.optional-dependencies.test")]);
    }

    // -----------------------------------------------------------------------
    // Aggregation within the requirement-parsing tier
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_lists_every_field() {
        // Once every structural check has passed, multiple *requirement
        // string* parse failures across different sections do all get
        // collected into one `PyprojectError` -- this is the one place the
        // module aggregates rather than stopping at the first. See
        // `multiple_invalid_requirements_all_reported` for the same
        // property asserted on `.fields()` directly.
        let err = Pyproject::parse(
            r#"
[project]
name = "myproj"
dependencies = ["requests >< 1.0", "flask"]

[dependency-groups]
dev = ["== 1.0"]
"#,
        )
        .unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("project.dependencies[0] not valid"));
        assert!(rendered.contains("dependency-groups.dev[0] not valid"));
    }
}
