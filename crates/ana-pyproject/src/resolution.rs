//! Resolve [PEP 621](https://peps.python.org/pep-0621/)
//! `[project.optional-dependencies]` and
//! [PEP 735](https://peps.python.org/pep-0735/) `[dependency-groups]` into
//! flat, self-reference- and include-free lists of requirements.
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
use thiserror::Error;
use uv_normalize::{ExtraName, GroupName, PackageName};
use uv_pep508::Requirement;

/// A single entry in a `[dependency-groups]` list, per PEP 735: either a
/// literal PEP 508 requirement string, or `{ include-group = "<name>" }`,
/// a reference to another group's entries.
///
/// Unlike upstream's `DependencyGroupSpecifier`, `include_group` is a typed
/// [`GroupName`] here rather than a raw `String` -- whatever builds this
/// value (a future `toml_edit`-based TOML walk) is expected to have already
/// normalized it, the same way every other group/extra name in this module
/// is typed rather than stringly.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DependencyGroupSpecifier {
    /// A literal PEP 508 requirement string.
    Requirement(Requirement),
    /// `{ include-group = "<name>" }` -- pull in another group's entries.
    IncludeGroup(GroupName),
}

/// `[project.optional-dependencies]` and `[dependency-groups]`, resolved
/// into flat lists of requirements that are not self-referential and
/// contain no `include-group` references.
///
/// Resolution is memoized here: [`resolve`] fills this map in as it goes,
/// and checks it before resolving a given group/extra again, so a group
/// referenced by several other groups' `include-group` entries is only
/// walked once. This is the "preserve the parsed data for reuse" property
/// carried over unchanged from upstream -- see the crate `README.md`.
///
/// Note that `project.name` is required to resolve self-referential
/// optional dependencies (see [`resolve`]'s `project_name` parameter).
///
/// This makes no guarantee about the order of items and whether duplicates
/// are removed or not -- same as upstream.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct ResolvedDependencies {
    /// Each extra's fully expanded requirement list, keyed by normalized
    /// extra name.
    pub optional_dependencies: IndexMap<ExtraName, Vec<Requirement>>,
    /// Each dependency group's fully expanded requirement list, keyed by
    /// normalized group name.
    pub dependency_groups: IndexMap<GroupName, Vec<Requirement>>,
}

/// An error resolving `[project.optional-dependencies]` or
/// `[dependency-groups]`.
///
/// Carries which top-level section the failure was discovered under (see
/// [`Section`]) alongside the underlying [`ResolveErrorKind`] -- callers
/// that need to blame a specific part of `pyproject.toml` (like
/// [`crate::Pyproject::parse`]) can read [`ResolveError::section`] directly
/// instead of re-running [`resolve`] with `dependency_groups: None` to
/// infer it from whether that second call happens to succeed. Only
/// [`resolve`] itself constructs this type, at the one place (its own two
/// top-level loops) that genuinely knows which section is being walked --
/// see [`resolve`]'s docs.
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
    /// surfaced, not necessarily the section [`ResolveErrorKind`]'s own
    /// variant names might suggest -- a `{ include-group = ... }` entry
    /// that references a missing *extra* (not another group) is a
    /// [`ResolveErrorKind::OptionalDependencyNotFound`] discovered while
    /// walking [`Section::DependencyGroups`], for example.
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
/// of never-repeating names (`a` includes `b` includes `c` ... thousands of
/// links deep) trips no cycle and would otherwise recurse once per link
/// with no bound at all. Both recursive functions here are plain native
/// recursion (no trampoline, no explicit stack), so an unbounded chain is a
/// stack overflow -- an unrecoverable process abort, not a catchable error
/// -- once the source is an untrusted `pyproject.toml` rather than a
/// hand-written test fixture.
///
/// `include-group`/self-referential extras exist to let one umbrella group
/// pull together a handful of purpose-named leaf groups (breadth --
/// `all = [{include-group="a"}, {include-group="b"}, ...]`), not to
/// express a long linear chain (depth). PEP 735's own canonical example
/// (`typing-test` including `typing` and `test`) only goes one level deep,
/// and even an unusually layered hand-authored project (base -> lint/
/// typing/test/docs -> ci -> dev) is unlikely to exceed 4-5. This constant
/// is set well above that with room to spare, while still being nowhere
/// near deep enough to risk the stack itself.
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
/// the [`ResolveErrorKind::MaxDepthExceeded`] message. Rendered the same
/// way as [`Cycle`] minus the closing `-> first` -- hitting the depth limit
/// doesn't imply the chain ever comes back around to its start.
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
/// references would recurse into a cycle, or if a non-cyclic reference
/// chain runs deeper than [`MAX_RESOLUTION_DEPTH`]. The returned
/// [`ResolveError::section`] tells you which of the two loops below --
/// `[project.optional-dependencies]` or `[dependency-groups]` -- was
/// running when the failure was found; that tagging happens right here,
/// at the one place that actually knows which loop is executing, rather
/// than being inferred after the fact by callers re-running [`resolve`]
/// with a section omitted to see if that changes the outcome.
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
    // dependencies. Hoisted out of the loop below, unlike upstream, which allocates a
    // fresh empty `IndexMap` on every iteration when there are no optional dependencies
    // at all -- same fallback behavior, one allocation instead of N.
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
/// expanded requirement list into `resolved.optional_dependencies` --
/// callers that need the value (rather than just the side effect of it
/// being memoized) read it back out of `resolved` themselves, so an
/// already-resolved extra costs a map lookup, not a clone: see the
/// [`ResolvedDependencies`] docs on memoization for why this can be called
/// on the same `extra` more than once.
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

    // `extra` is already a normalized `ExtraName`, so this is a direct map lookup --
    // upstream instead re-normalizes every key on every call to compare a raw `String`
    // key against a raw `String` reference, because its map is keyed by unnormalized
    // `String`. See the crate README's "Changes from upstream" section.
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
    // Check for a reference chain that's gone on too long to be legitimate
    // -- see `MAX_RESOLUTION_DEPTH`'s docs. This is not a cycle (nothing in
    // `parents` repeats, or the check above would have already fired), so
    // it needs its own bound: nothing else here limits recursion depth.
    if parents.len() >= MAX_RESOLUTION_DEPTH {
        return Err(ResolveErrorKind::MaxDepthExceeded {
            limit: MAX_RESOLUTION_DEPTH,
            chain: Chain(parents.clone()),
        });
    }
    parents.push(item);

    // Recurse into references, and add their resolved requirements to our own requirements.
    let mut resolved_requirements = Vec::with_capacity(unresolved_requirements.len());
    for unresolved_requirement in unresolved_requirements {
        if project_name.is_some_and(|project_name| *project_name == unresolved_requirement.name) {
            // Resolve each extra individually, as each refers to a different optional
            // dependency entry.
            for extra in &unresolved_requirement.extras {
                resolve_optional_dependency(
                    extra,
                    optional_dependencies,
                    resolved,
                    parents,
                    project_name,
                )?;
                // `resolve_optional_dependency` just proved this entry is in
                // `resolved.optional_dependencies` (either it inserted it
                // just now, or it already existed) -- read it back rather
                // than threading it through a return value, so a name
                // referenced from several places is only ever cloned at
                // the point something actually needs to own a copy.
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
            DependencyGroupSpecifier::Requirement(spec) => {
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
                        resolved_requirements
                            .extend(resolved.optional_dependencies[extra].iter().cloned());
                    }
                } else {
                    resolved_requirements.push(spec.clone());
                }
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
                // Same reasoning as the `Requirement` arm above.
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
                DependencyGroupSpecifier::Requirement(req("beta")),
                DependencyGroupSpecifier::Requirement(req("gamma")),
                DependencyGroupSpecifier::Requirement(req("delta")),
            ],
            group("epsilon") => vec![
                DependencyGroupSpecifier::Requirement(req("eta<2.0")),
                DependencyGroupSpecifier::Requirement(req("theta==2024.09.01")),
            ],
            group("iota") => vec![DependencyGroupSpecifier::IncludeGroup(group("alpha"))],
        };
        let resolved = resolve(None, None, Some(&dependency_groups)).unwrap();

        assert_eq!(
            resolved.dependency_groups[&group("iota")],
            vec![req("beta"), req("gamma"), req("delta")]
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
            group("dev") => vec![DependencyGroupSpecifier::Requirement(req("spam[test]"))],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap();

        assert_eq!(
            resolved.dependency_groups[&group("dev")],
            vec![req("pytest")]
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
            group("dev") => vec![DependencyGroupSpecifier::Requirement(req("ruff"))],
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
        assert_eq!(resolved.dependency_groups[&group("dev")], vec![req("ruff")]);
    }

    // Ported from `optional_dependencies_are_not_dependency_groups`.
    #[test]
    fn optional_dependencies_are_not_dependency_groups() {
        let optional_dependencies = indexmap! {
            extra("test") => vec![req("pytest")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Requirement(req("spam[test]"))],
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
            group("dev") => vec![DependencyGroupSpecifier::Requirement(req("spam[test]"))],
            group("test") => vec![DependencyGroupSpecifier::Requirement(req("spam[numpy]"))],
        };
        let resolved = resolve(
            Some(&PackageName::from_str("spam").unwrap()),
            Some(&optional_dependencies),
            Some(&dependency_groups),
        )
        .unwrap();

        assert_eq!(
            resolved.dependency_groups[&group("dev")],
            vec![req("pytest")]
        );
        assert_eq!(
            resolved.dependency_groups[&group("test")],
            vec![req("numpy")]
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
        // A failure found while the extras loop is running (nothing in
        // `dependency_groups` is involved at all) must be attributed to
        // `Section::OptionalDependencies`.
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
        // Symmetric with the extras case above, but for a failure found
        // while the groups loop is running with no extras involved.
        let dependency_groups = indexmap! {
            group("iota") => vec![DependencyGroupSpecifier::IncludeGroup(group("alpha"))],
        };
        let err = resolve(None, None, Some(&dependency_groups)).unwrap_err();

        assert_eq!(err.section(), Section::DependencyGroups);
    }

    #[test]
    fn error_in_extra_referenced_only_from_a_group_is_attributed_to_groups() {
        // The extra itself is perfectly valid on its own -- `all` isn't a
        // top-level extra at all, so the extras loop never even looks at
        // it. The only reason this fails is that `dev` (a dependency
        // group) references `spam[all]`, and `all` doesn't exist. That
        // makes it a `ResolveErrorKind::OptionalDependencyNotFound` (the
        // error *variant* says "optional dependency"), but it must still
        // be attributed to `Section::DependencyGroups`, since that's the
        // loop that was actually running when it was discovered -- see
        // `ResolveError::section`'s docs for why the variant name and the
        // section can disagree.
        let optional_dependencies = indexmap! {
            extra("test") => vec![req("pytest")],
        };
        let dependency_groups = indexmap! {
            group("dev") => vec![DependencyGroupSpecifier::Requirement(req("spam[all]"))],
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
        // `resolve()` walks extras to completion before it ever looks at
        // groups (see its docs) -- so when a document has a real problem
        // in `[project.optional-dependencies]` *and* a `[dependency-groups]`
        // that would also fail on its own, the extras failure is what
        // comes back, never the groups one.
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
                    DependencyGroupSpecifier::Requirement(req("leaf"))
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
        assert_eq!(resolved.dependency_groups[&group("g0")], vec![req("leaf")]);
    }

    #[test]
    fn dependency_group_chain_exceeding_limit_is_rejected() {
        // One link longer than `dependency_group_chain_within_limit_resolves`
        // -- and, critically, still acyclic (every group name is distinct),
        // so the cycle check can't be what catches this. Before
        // `MAX_RESOLUTION_DEPTH` existed, this shape of input is exactly
        // what would recurse without bound.
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
        // (`resolve_optional_dependency`) from `include-group` -- needs its
        // own depth-limit test rather than assuming the dependency-group
        // coverage above also exercises this path.
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
