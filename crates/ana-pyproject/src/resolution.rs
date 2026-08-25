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
#[derive(Debug, Error)]
#[error(transparent)]
pub struct ResolveError(#[from] ResolveErrorKind);

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
}

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
/// `include-group`) points at a name that doesn't exist, or if following
/// references would recurse into a cycle.
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
            )?;
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
            )?;
        }
    }

    Ok(resolved)
}

/// Resolves a single optional dependency (extra).
fn resolve_optional_dependency(
    extra: &ExtraName,
    optional_dependencies: &IndexMap<ExtraName, Vec<Requirement>>,
    resolved: &mut ResolvedDependencies,
    parents: &mut Vec<Item>,
    project_name: Option<&PackageName>,
) -> Result<Vec<Requirement>, ResolveError> {
    if let Some(requirements) = resolved.optional_dependencies.get(extra) {
        return Ok(requirements.clone());
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
        }
        .into());
    };

    // Check for cycles.
    let item = Item::Extra(extra.clone());
    if parents.contains(&item) {
        return Err(ResolveErrorKind::DependencyGroupCycle(Cycle(parents.clone())).into());
    }
    parents.push(item);

    // Recurse into references, and add their resolved requirements to our own requirements.
    let mut resolved_requirements = Vec::with_capacity(unresolved_requirements.len());
    for unresolved_requirement in unresolved_requirements {
        if project_name.is_some_and(|project_name| *project_name == unresolved_requirement.name) {
            // Resolve each extra individually, as each refers to a different optional
            // dependency entry.
            for extra in &unresolved_requirement.extras {
                resolved_requirements.extend(resolve_optional_dependency(
                    extra,
                    optional_dependencies,
                    resolved,
                    parents,
                    project_name,
                )?);
            }
        } else {
            resolved_requirements.push(unresolved_requirement.clone());
        }
    }
    resolved
        .optional_dependencies
        .insert(extra.clone(), resolved_requirements.clone());
    parents.pop();
    Ok(resolved_requirements)
}

/// Resolves a single dependency group.
fn resolve_dependency_group(
    dep_group: &GroupName,
    optional_dependencies: &IndexMap<ExtraName, Vec<Requirement>>,
    dependency_groups: &IndexMap<GroupName, Vec<DependencyGroupSpecifier>>,
    resolved: &mut ResolvedDependencies,
    parents: &mut Vec<Item>,
    project_name: Option<&PackageName>,
) -> Result<Vec<Requirement>, ResolveError> {
    if let Some(requirements) = resolved.dependency_groups.get(dep_group) {
        return Ok(requirements.clone());
    }

    let Some(unresolved_requirements) = dependency_groups.get(dep_group) else {
        let parent = parents
            .last()
            .cloned()
            .ok_or_else(|| ResolveErrorKind::MissingParent(Item::Group(dep_group.clone())))?;
        return Err(ResolveErrorKind::DependencyGroupNotFound {
            name: dep_group.clone(),
            included_by: parent,
        }
        .into());
    };

    // Check for cycles.
    let item = Item::Group(dep_group.clone());
    if parents.contains(&item) {
        return Err(ResolveErrorKind::DependencyGroupCycle(Cycle(parents.clone())).into());
    }
    parents.push(item);

    // Perform recursion, as required, on the dependency group's specifiers.
    let mut resolved_requirements = Vec::with_capacity(unresolved_requirements.len());
    for unresolved_requirement in unresolved_requirements {
        match unresolved_requirement {
            DependencyGroupSpecifier::Requirement(spec) => {
                if project_name.is_some_and(|project_name| *project_name == spec.name) {
                    for extra in &spec.extras {
                        resolved_requirements.extend(resolve_optional_dependency(
                            extra,
                            optional_dependencies,
                            resolved,
                            parents,
                            project_name,
                        )?);
                    }
                } else {
                    resolved_requirements.push(spec.clone());
                }
            }
            DependencyGroupSpecifier::IncludeGroup(include_group) => {
                resolved_requirements.extend(resolve_dependency_group(
                    include_group,
                    optional_dependencies,
                    dependency_groups,
                    resolved,
                    parents,
                    project_name,
                )?);
            }
        }
    }
    resolved
        .dependency_groups
        .insert(dep_group.clone(), resolved_requirements.clone());
    parents.pop();
    Ok(resolved_requirements)
}

#[cfg(test)]
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
}
