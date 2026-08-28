//! Resolve [PEP 621](https://peps.python.org/pep-0621/)
//! `[project.optional-dependencies]` and
//! [PEP 735](https://peps.python.org/pep-0735/) `[dependency-groups]` --
//! merged with ana's own `[tool.ana.matchspec-dependency-groups]`
//! extension, see [`Dependency`] -- into flat, self-reference- and
//! include-free lists.
//!
//! ## Provenance
//!
//! This module is adapted from
//! [`resolution.rs`](https://github.com/PyO3/pyproject-toml-rs/blob/02d274155edf0faf08f8600f0048199067fec26d/src/resolution.rs)
//! in [`pyproject-toml-rs`](https://github.com/PyO3/pyproject-toml-rs)
//! (the `pyproject-toml` crate on crates.io), version 0.13.7, commit
//! [`02d274155edf0faf08f8600f0048199067fec26d`](https://github.com/PyO3/pyproject-toml-rs/commit/02d274155edf0faf08f8600f0048199067fec26d).
//!
//! ```text
//! MIT License
//!
//! Copyright (c) 2021-present PyO3 Project and Contributors
//! ```
//!
//! See the crate's `LICENSE` file for the full text, and its `README.md`
//! for what was changed relative to the original and why.

use std::fmt::{self, Display, Formatter};

use indexmap::IndexMap;
use rattler_conda_types::MatchSpec;
use thiserror::Error;
use uv_normalize::{ExtraName, GroupName, PackageName};
use uv_pep508::Requirement;

/// One dependency in the unified graph [`resolve`] walks: either a PEP 508
/// requirement string (from `[project.dependencies]`,
/// `[project.optional-dependencies]`, or `[dependency-groups]`) or a conda
/// `MatchSpec` string (from `[tool.ana.matchspec-dependencies]` or
/// `[tool.ana.matchspec-dependency-groups]`).
///
/// `[dependency-groups]` and `[tool.ana.matchspec-dependency-groups]`
/// entries sharing the same normalized group name are merged into one
/// group before resolution -- see `crate::project`'s extraction functions
/// -- so a single group's list, and therefore a single `include-group`
/// reference, may hold a mix of both variants.
///
/// Self-referential-extra expansion (a `Requirement` whose name matches
/// the project's own name, expanding into that extra's own entries) only
/// ever applies to the [`Dependency::Pep508`] variant, and there is no
/// conda-side equivalent by design: ana has no
/// `[tool.ana.optional-dependencies]` table, so there's nothing for a
/// matchspec entry to expand *into* even if one referenced the project by
/// name. `MatchSpec` does have its own bracket `extras=[...]` syntax, but
/// that names optional *conda package build features* for the solver --
/// an unrelated, solver-facing concept -- not a `pyproject.toml` extras
/// table lookup. A `Dependency::Matchspec` entry is therefore always
/// pushed through unchanged, regardless of its name or extras.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Dependency {
    /// A literal PEP 508 requirement string.
    Pep508(Requirement),
    /// A literal conda `MatchSpec` string, boxed so this variant doesn't
    /// dwarf [`Dependency::Pep508`]'s size (a `MatchSpec` is
    /// considerably larger than a `Requirement`). Cross-group
    /// duplication (an `include-group` reference pulling another
    /// group's entries into this one) is rare enough that a real clone
    /// here -- same as [`Dependency::Pep508`] already pays -- is
    /// cheaper overall than refcounting every ordinary,
    /// never-duplicated entry.
    Matchspec(Box<MatchSpec>),
}

/// A single entry in a `[dependency-groups]` or
/// `[tool.ana.matchspec-dependency-groups]` list: either a literal
/// dependency, or `{ include-group = "<name>" }`, a reference to another
/// group's entries.
///
/// `include_group` is a typed [`GroupName`], not a raw `String` -- callers
/// building this value are expected to have already normalized it, like
/// every other group/extra name in this module.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DependencyGroupSpecifier {
    /// A literal PEP 508 requirement string or conda `MatchSpec` string.
    Dependency(Dependency),
    /// `{ include-group = "<name>" }` -- pull in another group's entries.
    IncludeGroup(GroupName),
}

/// `[project.optional-dependencies]` and `[dependency-groups]`
/// (merged with `[tool.ana.matchspec-dependency-groups]`), resolved into
/// flat lists that are not self-referential and contain no
/// `include-group` references.
///
/// Resolution is memoized here: [`resolve`] fills this map in as it goes
/// and checks it before resolving a given group/extra again, so a group
/// referenced from multiple places is only walked once.
///
/// `project.name` is required to resolve self-referential optional
/// dependencies (see [`resolve`]'s `project_name` parameter). Order and
/// deduplication of the resulting lists are not guaranteed.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ResolvedDependencies {
    /// Each extra's fully expanded requirement list, keyed by normalized
    /// extra name. Extras are PEP 508-only -- there is no conda-extras
    /// equivalent -- so this stays `Requirement`-typed rather than
    /// [`Dependency`].
    pub optional_dependencies: IndexMap<ExtraName, Vec<Requirement>>,
    /// Each dependency group's fully expanded dependency list, keyed by
    /// normalized group name. May hold a mix of [`Dependency::Pep508`]
    /// and [`Dependency::Matchspec`] entries.
    pub dependency_groups: IndexMap<GroupName, Vec<Dependency>>,
}

/// An error resolving `[project.optional-dependencies]` or
/// `[dependency-groups]`.
///
/// Carries which top-level section the failure was discovered under (see
/// [`Section`]) alongside the underlying [`ResolveErrorKind`], so callers
/// (like [`crate::Pyproject::parse`]) can read [`ResolveError::section`]
/// directly instead of re-running [`resolve`] to infer it.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct ResolveError {
    kind: ResolveErrorKind,
    section: Section,
}

impl ResolveError {
    fn new(kind: ResolveErrorKind, section: Section) -> Self {
        Self { kind, section }
    }

    /// Which top-level section of `pyproject.toml` this failure belongs
    /// to: `[project.optional-dependencies]` or `[dependency-groups]`.
    ///
    /// This is the section [`resolve`] was walking when the failure
    /// surfaced, which need not match what [`ResolveErrorKind`]'s variant
    /// name suggests -- e.g. an `include-group` entry referencing a missing
    /// *extra* is `OptionalDependencyNotFound` discovered while walking
    /// [`Section::DependencyGroups`].
    pub fn section(&self) -> Section {
        self.section
    }
}

/// The two top-level `pyproject.toml` sections [`resolve`] walks, in the
/// order it walks them. See [`ResolveError::section`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// `[project.optional-dependencies]`.
    OptionalDependencies,
    /// `[dependency-groups]`.
    DependencyGroups,
}

#[derive(Debug, Error)]
enum ResolveErrorKind {
    #[error("Failed to find optional dependency `{name}` included by {included_by}")]
    OptionalDependencyNotFound { name: ExtraName, included_by: Item },
    #[error("Failed to find dependency group `{name}` included by {included_by}")]
    DependencyGroupNotFound { name: GroupName, included_by: Item },
    #[error("Cycles are not supported: {0}")]
    DependencyGroupCycle(Cycle),
    #[error("Internal error: no parent tracked while resolving unresolvable reference `{0}`")]
    MissingParent(Item),
    #[error("maximum reference depth ({limit}) exceeded while resolving: {chain}")]
    MaxDepthExceeded { limit: usize, chain: Chain },
}

/// Maximum `include-group`/self-referential-extra reference depth
/// [`resolve`] will follow before giving up, checked in
/// [`resolve_optional_dependency`] and [`resolve_dependency_group`].
///
/// The cycle check below only catches a name that *repeats*; a long chain
/// of never-repeating names would otherwise recurse once per link with no
/// bound at all, and since both recursive functions use plain native
/// recursion, an unbounded chain from an untrusted `pyproject.toml` would
/// be a stack overflow (an unrecoverable abort, not a catchable error).
///
/// `include-group`/self-referential extras are meant for breadth (one
/// umbrella group pulling together a handful of leaf groups), not depth --
/// even an unusually layered project is unlikely to chain more than 4-5
/// deep. This constant leaves comfortable room above that without coming
/// close to risking the stack itself.
const MAX_RESOLUTION_DEPTH: usize = 10;

/// A cycle in the `include-group`/self-referential-extra recursion.
#[derive(Debug)]
pub struct Cycle(Vec<Item>);

/// Display a cycle, e.g., `extra:a -> group:b -> extra:a`.
impl Display for Cycle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let Some((first, rest)) = self.0.split_first() else {
            return Ok(());
        };
        write!(f, "{first}")?;
        for item in rest {
            write!(f, " -> {item}")?;
        }
        write!(f, " -> {first}")?;
        Ok(())
    }
}

/// A non-cyclic `include-group`/self-referential-extra reference chain, for
/// the [`ResolveErrorKind::MaxDepthExceeded`] message. Rendered like
/// [`Cycle`] but without the closing `-> first`, since hitting the depth
/// limit doesn't mean the chain loops back to its start.
#[derive(Debug)]
pub struct Chain(Vec<Item>);

impl Display for Chain {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut items = self.0.iter();
        if let Some(first) = items.next() {
            write!(f, "{first}")?;
        }
        for item in items {
            write!(f, " -> {item}")?;
        }
        Ok(())
    }
}

/// A reference to either an optional dependency (extra) or a dependency
/// group, used to report where an unresolvable reference was included from
/// and to detect cycles.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Item {
    /// An extra referenced via `name[extra]` extras syntax.
    Extra(ExtraName),
    /// A dependency group referenced via `{ include-group = "name" }`.
    Group(GroupName),
}

impl Display for Item {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Item::Extra(extra) => write!(f, "extra:{extra}"),
            Item::Group(group) => write!(f, "group:{group}"),
        }
    }
}

/// Resolve `[project.optional-dependencies]` and `[dependency-groups]` into
/// flat, self-reference- and include-free lists of requirements.
///
/// `project_name`, if given, enables resolving self-referential extras --
/// `myproj[test]` appearing inside `myproj`'s own optional-dependencies or
/// dependency-groups expands into that extra's own entries rather than
/// being treated as a literal dependency on the project itself.
///
/// Returns an error if a reference (an extra bracket or an
/// `include-group`) points at a name that doesn't exist, if following
/// references recurses into a cycle, or if a non-cyclic reference chain
/// runs deeper than [`MAX_RESOLUTION_DEPTH`]. The returned
/// [`ResolveError::section`] reports which of the two loops below was
/// running when the failure was found.
pub fn resolve(
    project_name: Option<&PackageName>,
    optional_dependencies: Option<&IndexMap<ExtraName, Vec<Requirement>>>,
    dependency_groups: Option<&IndexMap<GroupName, Vec<DependencyGroupSpecifier>>>,
) -> Result<ResolvedDependencies, ResolveError> {
    let mut resolved = ResolvedDependencies::default();

    // Resolve optional dependencies, which may only reference optional dependencies.
    if let Some(optional_dependencies) = optional_dependencies {
        for extra in optional_dependencies.keys() {
            resolve_optional_dependency(
                extra,
                optional_dependencies,
                &mut resolved,
                &mut Vec::new(),
                project_name,
            )
            .map_err(|kind| ResolveError::new(kind, Section::OptionalDependencies))?;
        }
    }

    // Resolve dependency groups, which may reference dependency groups and optional
    // dependencies. `empty_extras` is hoisted out of the loop below so a project with no
    // optional dependencies doesn't allocate a fresh empty map on every iteration.
    let empty_extras = IndexMap::new();
    if let Some(dependency_groups) = dependency_groups {
        for group in dependency_groups.keys() {
            resolve_dependency_group(
                group,
                optional_dependencies.unwrap_or(&empty_extras),
                dependency_groups,
                &mut resolved,
                &mut Vec::new(),
                project_name,
            )
            .map_err(|kind| ResolveError::new(kind, Section::DependencyGroups))?;
        }
    }

    Ok(resolved)
}

/// Resolves a single optional dependency (extra), writing its fully
/// expanded requirement list into `resolved.optional_dependencies` rather
/// than returning it -- callers read it back out of `resolved`, so an
/// already-resolved extra costs a map lookup, not a clone.
fn resolve_optional_dependency(
    extra: &ExtraName,
    optional_dependencies: &IndexMap<ExtraName, Vec<Requirement>>,
    resolved: &mut ResolvedDependencies,
    parents: &mut Vec<Item>,
    project_name: Option<&PackageName>,
) -> Result<(), ResolveErrorKind> {
    if resolved.optional_dependencies.contains_key(extra) {
        return Ok(());
    }

    // `extra` is already a normalized `ExtraName`, so this is a direct map lookup
    // rather than a string re-normalization on every call.
    let Some(unresolved_requirements) = optional_dependencies.get(extra) else {
        let parent = parents
            .last()
            .cloned()
            .ok_or_else(|| ResolveErrorKind::MissingParent(Item::Extra(extra.clone())))?;
        return Err(ResolveErrorKind::OptionalDependencyNotFound {
            name: extra.clone(),
            included_by: parent,
        });
    };

    // Check for cycles.
    let item = Item::Extra(extra.clone());
    if parents.contains(&item) {
        return Err(ResolveErrorKind::DependencyGroupCycle(Cycle(
            parents.clone(),
        )));
    }
    // Depth limit for non-cyclic chains (see `MAX_RESOLUTION_DEPTH`) -- nothing in
    // `parents` repeats here, or the cycle check above would have fired, so this is
    // the only thing bounding recursion depth.
    if parents.len() >= MAX_RESOLUTION_DEPTH {
        return Err(ResolveErrorKind::MaxDepthExceeded {
            limit: MAX_RESOLUTION_DEPTH,
            chain: Chain(parents.clone()),
        });
    }
    parents.push(item);

    // Recurse into references and fold their resolved requirements into ours.
    let mut resolved_requirements = Vec::with_capacity(unresolved_requirements.len());
    for unresolved_requirement in unresolved_requirements {
        if project_name.is_some_and(|project_name| *project_name == unresolved_requirement.name) {
            // Each extra bracket entry refers to a different optional dependency.
            for extra in &unresolved_requirement.extras {
                resolve_optional_dependency(
                    extra,
                    optional_dependencies,
                    resolved,
                    parents,
                    project_name,
                )?;
                // The call above guarantees `extra` is now in
                // `resolved.optional_dependencies`; read it back instead of
                // threading it through a return value, so a name referenced
                // from several places is only cloned when actually needed.
                resolved_requirements.extend(resolved.optional_dependencies[extra].iter().cloned());
            }
        } else {
            resolved_requirements.push(unresolved_requirement.clone());
        }
    }
    resolved
        .optional_dependencies
        .insert(extra.clone(), resolved_requirements);
    parents.pop();
    Ok(())
}

/// Resolves a single dependency group, writing its fully expanded
/// requirement list into `resolved.dependency_groups` -- see
/// [`resolve_optional_dependency`]'s docs for why this returns `()` rather
/// than the resolved list itself.
fn resolve_dependency_group(
    dep_group: &GroupName,
    optional_dependencies: &IndexMap<ExtraName, Vec<Requirement>>,
    dependency_groups: &IndexMap<GroupName, Vec<DependencyGroupSpecifier>>,
    resolved: &mut ResolvedDependencies,
    parents: &mut Vec<Item>,
    project_name: Option<&PackageName>,
) -> Result<(), ResolveErrorKind> {
    if resolved.dependency_groups.contains_key(dep_group) {
        return Ok(());
    }

    let Some(unresolved_requirements) = dependency_groups.get(dep_group) else {
        let parent = parents
            .last()
            .cloned()
            .ok_or_else(|| ResolveErrorKind::MissingParent(Item::Group(dep_group.clone())))?;
        return Err(ResolveErrorKind::DependencyGroupNotFound {
            name: dep_group.clone(),
            included_by: parent,
        });
    };

    // Check for cycles.
    let item = Item::Group(dep_group.clone());
    if parents.contains(&item) {
        return Err(ResolveErrorKind::DependencyGroupCycle(Cycle(
            parents.clone(),
        )));
    }
    // See `MAX_RESOLUTION_DEPTH`'s docs: bounds a long, non-repeating
    // `include-group` chain that the cycle check above can't catch.
    if parents.len() >= MAX_RESOLUTION_DEPTH {
        return Err(ResolveErrorKind::MaxDepthExceeded {
            limit: MAX_RESOLUTION_DEPTH,
            chain: Chain(parents.clone()),
        });
    }
    parents.push(item);

    // Perform recursion, as required, on the dependency group's specifiers.
    let mut resolved_requirements = Vec::with_capacity(unresolved_requirements.len());
    for unresolved_requirement in unresolved_requirements {
        match unresolved_requirement {
            // A PEP 508 requirement whose name matches the project's own
            // name is a self-referential extra (`myproj[test]` inside
            // myproj's own dependency-groups) and expands to that extra's
            // entries. There is no matchspec equivalent: ana has no
            // `[tool.ana.optional-dependencies]` table to expand into, so
            // a `Matchspec` is always pushed through unchanged regardless
            // of its name/extras -- see `Dependency`'s docs.
            DependencyGroupSpecifier::Dependency(Dependency::Pep508(spec)) => {
                if project_name.is_some_and(|project_name| *project_name == spec.name) {
                    for extra in &spec.extras {
                        resolve_optional_dependency(
                            extra,
                            optional_dependencies,
                            resolved,
                            parents,
                            project_name,
                        )?;
                        // See `resolve_optional_dependency`'s matching
                        // comment: read the now-memoized value back out
                        // instead of threading it through a return value.
                        resolved_requirements.extend(
                            resolved.optional_dependencies[extra]
                                .iter()
                                .cloned()
                                .map(Dependency::Pep508),
                        );
                    }
                } else {
                    resolved_requirements.push(Dependency::Pep508(spec.clone()));
                }
            }
            DependencyGroupSpecifier::Dependency(Dependency::Matchspec(spec)) => {
                resolved_requirements.push(Dependency::Matchspec(spec.clone()));
            }
            DependencyGroupSpecifier::IncludeGroup(include_group) => {
                resolve_dependency_group(
                    include_group,
                    optional_dependencies,
                    dependency_groups,
                    resolved,
                    parents,
                    project_name,
                )?;
                // Same reasoning as the `Pep508` arm above.
                resolved_requirements
                    .extend(resolved.dependency_groups[include_group].iter().cloned());
            }
        }
    }
    resolved
        .dependency_groups
        .insert(dep_group.clone(), resolved_requirements);
    parents.pop();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::str::FromStr;

    use indexmap::indexmap;

    use super::*;

    fn extra(name: &str) -> ExtraName {
        ExtraName::from_str(name).unwrap()
    }

    fn group(name: &str) -> GroupName {
        GroupName::from_str(name).unwrap()
    }

    fn req(spec: &str) -> Requirement {
        Requirement::from_str(spec).unwrap()
    }

    /// A [`Dependency::Pep508`] wrapping `req(spec)`, for building/
    /// comparing against `dependency_groups`, which is `Dependency`-typed.
    /// `optional_dependencies` stays plain `Requirement`-typed (see
    /// [`ResolvedDependencies`]'s docs), so tests exercising it keep using
    /// `req(...)` directly.
    fn dep(spec: &str) -> Dependency {
        Dependency::Pep508(req(spec))
    }

    // Ported from `parse_pyproject_toml_optional_dependencies_resolve`.
    #[test]
    fn optional_dependencies_resolve() {
        let optional_dependencies = indexmap! {
            extra("alpha") => vec![req("beta"), req("gamma"), req("delta")],
            extra("epsilon") => vec![req("eta<2.0"), req("theta==2024.09.01")],
            extra("iota") => vec![req("spam[alpha]")],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap();

        assert_eq!(
            resolved.optional_dependencies[&extra("iota")],
            vec![req("beta"), req("gamma"), req("delta")]
        );
    }

    // Ported from `parse_pyproject_toml_optional_dependencies_cycle`.
    #[test]
    fn optional_dependencies_cycle() {
        let optional_dependencies = indexmap! {
            extra("alpha") => vec![req("spam[iota]")],
            extra("iota") => vec![req("spam[alpha]")],
        };
        let err = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Cycles are not supported: extra:alpha -> extra:iota -> extra:alpha"
        );
    }

    // Ported from `parse_pyproject_toml_optional_dependencies_missing_include`.
    #[test]
    fn optional_dependencies_missing_include() {
        let optional_dependencies = indexmap! {
            extra("iota") => vec![req("spam[alpha]")],
        };
        let err = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Failed to find optional dependency `alpha` included by extra:iota"
        );
    }

    // Ported from `parse_pyproject_toml_optional_dependencies_missing_top_level`.
    #[test]
    fn optional_dependencies_missing_top_level() {
        let optional_dependencies = indexmap! {
            extra("alpha") => vec![req("beta")],
        };
        let mut resolved = ResolvedDependencies::default();
        let err = resolve_optional_dependency(
            &extra("foo"),
            &optional_dependencies,
            &mut resolved,
            &mut vec![Item::Extra(extra("bar"))],
            Some(&PackageName::from_str("spam").unwrap()),
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "Failed to find optional dependency `foo` included by extra:bar"
        );
    }

    // Ported from `parse_pyproject_toml_dependency_groups_resolve`.
    #[test]
    fn dependency_groups_resolve() {
        let dependency_groups = indexmap! {
            group("alpha") => vec![
                DependencyGroupSpecifier::Dependency(dep("beta")),
                DependencyGroupSpecifier::Dependency(dep("gamma")),
                DependencyGroupSpecifier::Dependency(dep("delta")),
            ],
            group("epsilon") => vec![
                DependencyGroupSpecifier::Dependency(dep("eta<2.0")),
                DependencyGroupSpecifier::Dependency(dep("theta==2024.09.01")),
            ],
            group("iota") => vec![DependencyGroupSpecifier::IncludeGroup(group("alpha"))],
        };
        let resolved = resolve(None, None, Some(&dependency_groups)).unwrap();

        assert_eq!(
            resolved.dependency_groups[&group("iota")],
            vec![dep("beta"), dep("gamma"), dep("delta")]
        );
    }

    // Ported from `parse_pyproject_toml_dependency_groups_cycle`.
    #[test]
    fn dependency_groups_cycle() {
        let dependency_groups = indexmap! {
            group("alpha") => vec![DependencyGroupSpecifier::IncludeGroup(group("iota"))],
            group("iota") => vec![DependencyGroupSpecifier::IncludeGroup(group("alpha"))],
        };
        let err = resolve(None, None, Some(&dependency_groups)).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Cycles are not supported: group:alpha -> group:iota -> group:alpha"
        );
    }

    // Ported from `parse_pyproject_toml_dependency_groups_missing_include`.
    #[test]
    fn dependency_groups_missing_include() {
        let dependency_groups = indexmap! {
            group("iota") => vec![DependencyGroupSpecifier::IncludeGroup(group("alpha"))],
        };
        let err = resolve(None, None, Some(&dependency_groups)).unwrap_err();

        assert_eq!(
            err.to_string(),
            "Failed to find dependency group `alpha` included by group:iota"
        );
    }

    // Ported from `parse_pyproject_toml_dependency_groups_with_optional_dependencies`.
    #[test]
    fn dependency_groups_with_optional_dependencies() {
        let optional_dependencies = indexmap! {
            extra("test") => vec![req("pytest")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Dependency(dep("spam[test]"))],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap();

        assert_eq!(
            resolved.dependency_groups[&group("dev")],
            vec![dep("pytest")]
        );
    }

    // Ported from `name_collision`: an extra and a group with the same name are
    // independent namespaces.
    #[test]
    fn extra_and_group_same_name_are_independent() {
        let optional_dependencies = indexmap! {
            extra("dev") => vec![req("pytest")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Dependency(dep("ruff"))],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap();

        assert_eq!(
            resolved.optional_dependencies[&extra("dev")],
            vec![req("pytest")]
        );
        assert_eq!(resolved.dependency_groups[&group("dev")], vec![dep("ruff")]);
    }

    // Ported from `optional_dependencies_are_not_dependency_groups`.
    #[test]
    fn optional_dependencies_are_not_dependency_groups() {
        let optional_dependencies = indexmap! {
            extra("test") => vec![req("pytest")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Dependency(dep("spam[test]"))],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap();

        assert!(resolved.optional_dependencies.contains_key(&extra("test")));
        assert!(!resolved.dependency_groups.contains_key(&group("test")));
        assert!(resolved.dependency_groups.contains_key(&group("dev")));
    }

    // Ported from `mixed_resolution`.
    #[test]
    fn mixed_resolution() {
        let optional_dependencies = indexmap! {
            extra("test") => vec![req("pytest")],
            extra("numpy") => vec![req("numpy")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Dependency(dep("spam[test]"))],
            group("test") => vec![DependencyGroupSpecifier::Dependency(dep("spam[numpy]"))],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap();

        assert_eq!(
            resolved.dependency_groups[&group("dev")],
            vec![dep("pytest")]
        );
        assert_eq!(
            resolved.dependency_groups[&group("test")],
            vec![dep("numpy")]
        );
    }

    // Ported from `optional_dependencies_with_underscores`. Unlike upstream, this needs
    // no special-cased normalized comparison to pass: `group_one` and `group-one` (and
    // `group_two`/`group-two`) are already the same `ExtraName` value once constructed,
    // so the lookup in `resolve_optional_dependency` is a plain, direct map lookup. See
    // the crate README's "Changes from upstream" section.
    #[test]
    fn optional_dependencies_with_underscores() {
        let optional_dependencies = indexmap! {
            extra("all") => vec![req("foo[group-one]"), req("foo[group_two]")],
            extra("group_one") => vec![req("anyio>=4.9.0")],
            extra("group-two") => vec![req("trio>=0.31.0")],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("foo").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap();

        assert_eq!(
            resolved.optional_dependencies[&extra("all")],
            vec![req("anyio>=4.9.0"), req("trio>=0.31.0")]
        );
    }

    // -----------------------------------------------------------------------
    // Error attribution (`ResolveError::section`)
    // -----------------------------------------------------------------------

    #[test]
    fn error_in_optional_dependencies_is_attributed_there() {
        // A failure found while the extras loop is running must be attributed
        // to `Section::OptionalDependencies`.
        let optional_dependencies = indexmap! {
            extra("iota") => vec![req("spam[alpha]")],
        };
        let err = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap_err();

        assert_eq!(err.section(), Section::OptionalDependencies);
    }

    #[test]
    fn error_in_dependency_groups_is_attributed_there() {
        // Symmetric with the extras case above, for a failure in the groups loop.
        let dependency_groups = indexmap! {
            group("iota") => vec![DependencyGroupSpecifier::IncludeGroup(group("alpha"))],
        };
        let err = resolve(None, None, Some(&dependency_groups)).unwrap_err();

        assert_eq!(err.section(), Section::DependencyGroups);
    }

    #[test]
    fn error_in_extra_referenced_only_from_a_group_is_attributed_to_groups() {
        // `all` isn't a top-level extra, so the extras loop never looks at it --
        // this only fails because `dev` (a group) references `spam[all]`. That
        // makes it an `OptionalDependencyNotFound` (the variant says "optional
        // dependency"), but it's still attributed to `Section::DependencyGroups`,
        // since that's the loop that was running when it was discovered.
        let optional_dependencies = indexmap! {
            extra("test") => vec![req("pytest")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Dependency(dep("spam[all]"))],
        };
        let err = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap_err();

        assert_eq!(err.section(), Section::DependencyGroups);
        assert_eq!(
            err.to_string(),
            "Failed to find optional dependency `all` included by group:dev"
        );
    }

    #[test]
    fn error_in_extras_takes_priority_when_both_sections_are_present() {
        // `resolve()` walks extras to completion before looking at groups, so
        // when both sections have a failure, the extras failure wins.
        let optional_dependencies = indexmap! {
            extra("broken") => vec![req("spam[missing]")],
        };
        let dependency_groups = indexmap! {
            group("also-broken") => vec![DependencyGroupSpecifier::IncludeGroup(group("nope"))],
        };
        let err = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap_err();

        assert_eq!(err.section(), Section::OptionalDependencies);
        assert_eq!(
            err.to_string(),
            "Failed to find optional dependency `missing` included by extra:broken"
        );
    }

    // -----------------------------------------------------------------------
    // Maximum resolution depth
    // -----------------------------------------------------------------------

    /// A chain of `len` dependency groups, `g0` through `g{len-1}`, each
    /// including the next via `include-group`; the last one is a plain
    /// requirement rather than another include, so the chain terminates on
    /// its own instead of erroring on a missing final reference.
    fn dependency_group_chain(len: usize) -> IndexMap<GroupName, Vec<DependencyGroupSpecifier>> {
        (0..len)
            .map(|i| {
                let specifier = if i + 1 < len {
                    DependencyGroupSpecifier::IncludeGroup(group(&format!("g{}", i + 1)))
                } else {
                    DependencyGroupSpecifier::Dependency(dep("leaf"))
                };
                (group(&format!("g{i}")), vec![specifier])
            })
            .collect()
    }

    /// Same shape as [`dependency_group_chain`], but as self-referential
    /// optional-dependency extras (`e0` through `e{len-1}`) on a project
    /// named `spam`, instead of `include-group` references.
    fn optional_dependency_chain(len: usize) -> IndexMap<ExtraName, Vec<Requirement>> {
        (0..len)
            .map(|i| {
                let requirement = if i + 1 < len {
                    req(&format!("spam[e{}]", i + 1))
                } else {
                    req("leaf")
                };
                (extra(&format!("e{i}")), vec![requirement])
            })
            .collect()
    }

    #[test]
    fn dependency_group_chain_within_limit_resolves() {
        let dependency_groups = dependency_group_chain(MAX_RESOLUTION_DEPTH);
        let resolved = resolve(None, None, Some(&dependency_groups)).unwrap();
        assert_eq!(resolved.dependency_groups[&group("g0")], vec![dep("leaf")]);
    }

    #[test]
    fn dependency_group_chain_exceeding_limit_is_rejected() {
        // One link longer than the "within limit" test above, still acyclic
        // (every group name is distinct), so the cycle check can't catch it.
        let dependency_groups = dependency_group_chain(MAX_RESOLUTION_DEPTH + 1);
        let err = resolve(None, None, Some(&dependency_groups)).unwrap_err();
        assert!(err.to_string().starts_with(&format!(
            "maximum reference depth ({MAX_RESOLUTION_DEPTH}) exceeded"
        )));
    }

    #[test]
    fn optional_dependency_chain_within_limit_resolves() {
        let optional_dependencies = optional_dependency_chain(MAX_RESOLUTION_DEPTH);
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap();
        assert_eq!(
            resolved.optional_dependencies[&extra("e0")],
            vec![req("leaf")]
        );
    }

    #[test]
    fn optional_dependency_chain_exceeding_limit_is_rejected() {
        // Self-referential extras recurse through a separate function
        // (`resolve_optional_dependency`) from `include-group`, so this
        // needs its own depth-limit test.
        let optional_dependencies = optional_dependency_chain(MAX_RESOLUTION_DEPTH + 1);
        let err = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            None,
        )
        .unwrap_err();
        assert!(err.to_string().starts_with(&format!(
            "maximum reference depth ({MAX_RESOLUTION_DEPTH}) exceeded"
        )));
    }
}
