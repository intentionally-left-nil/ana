//! This machine's known marker facts, as a `MarkerTree` assumption for
//! [`crate::to_matchspec_condition`]'s `restrict()` call.
//!
//! Per `investigations/pep508_to_matchspec_api.md`'s "Slow path, take 2":
//! `ana` installs onto one concrete machine, so unlike a portable-matchspec
//! design (which would need a `CondaTarget` per possible subdir), every
//! non-python-version marker key is fixed for the lifetime of the process.
//! Two of those are policy, not host facts -- `implementation_name`/
//! `platform_python_implementation` are always `cpython`/`CPython`, since
//! CPython is the only interpreter `ana` supports, regardless of subdir.
//! The rest (`os_name`, `sys_platform`, `platform_system`,
//! `platform_machine`) are a pure function of the subdir being installed
//! onto -- the same `_SUBDIR_PLATFORM` table reroll's
//! `dependencies/environment.py` already validated, ported here 1:1 rather
//! than derived from `rattler_conda_types::Arch`'s own strings: Windows's
//! `platform_machine` is `"AMD64"`/`"ARM64"` (uppercase, historical WOW64
//! naming), not `Arch::as_str()`'s lowercase `"x86_64"`/`"arm64"` --
//! confirmed directly against `rattler_conda_types` 0.52.0's own `Arch`
//! source, not assumed.
//!
//! Deliberately excluded from the assumption: `platform_release`/
//! `platform_version` (the OS kernel release/build strings). These are
//! real per-machine facts, but they have no matchspec equivalent even once
//! known, and probing them would mean a raw `uname()` FFI call (the same
//! shape `rattler_virtual_packages` already makes for a different
//! purpose -- extracting glibc's version, not the full uname string PEP
//! 508 wants) for two keys reroll's own fast-path table already treats as
//! always-unconvertible. Leaving them out of the assumption rather than
//! erroring here means `restrict()` still simplifies every other clause
//! in a marker that happens to also mention one of these keys -- the
//! marker surfaces the untouched clause in the residual, where
//! [`crate::condition`]'s existing "no matchspec equivalent"
//! `Unconvertible` case catches it, same as it always would have.
//!
//! No string is ever formatted and reparsed to build the assumption:
//! every leaf is a typed `MarkerExpression::String { key, operator:
//! MarkerOperator::Equal, value }`, folded into one tree with `.and()` --
//! see `investigations/pep508_to_matchspec_api.md`'s headline finding,
//! now extended to assumption-building, not just leaf conversion.

use rattler_conda_types::Platform;
use uv_pep508::{MarkerExpression, MarkerOperator, MarkerTree, MarkerValueString};

/// `subdir` has no known marker-environment mapping -- `ana` only installs
/// onto `linux-64`, `linux-aarch64`, `osx-64`, `osx-arm64`, `win-64`, or
/// `win-arm64` today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "platform {0:?} has no known marker-environment mapping (ana only installs onto \
     linux-64, linux-aarch64, osx-64, osx-arm64, win-64, or win-arm64)"
)]
pub struct UnsupportedPlatform(pub Platform);

/// One subdir's `os_name`/`sys_platform`/`platform_system`/
/// `platform_machine` marker values -- ported from reroll's
/// `dependencies/environment.py`'s `_SUBDIR_PLATFORM`, not derived from
/// `rattler_conda_types::Arch`'s own strings (see this module's docs for
/// why: Windows's spelling diverges).
struct SubdirMarkers {
    platform_system: &'static str,
    platform_machine: &'static str,
    sys_platform: &'static str,
    os_name: &'static str,
}

const fn subdir_markers(subdir: Platform) -> Option<SubdirMarkers> {
    match subdir {
        Platform::Linux64 => Some(SubdirMarkers {
            platform_system: "Linux",
            platform_machine: "x86_64",
            sys_platform: "linux",
            os_name: "posix",
        }),
        Platform::LinuxAarch64 => Some(SubdirMarkers {
            platform_system: "Linux",
            platform_machine: "aarch64",
            sys_platform: "linux",
            os_name: "posix",
        }),
        Platform::Osx64 => Some(SubdirMarkers {
            platform_system: "Darwin",
            platform_machine: "x86_64",
            sys_platform: "darwin",
            os_name: "posix",
        }),
        Platform::OsxArm64 => Some(SubdirMarkers {
            platform_system: "Darwin",
            platform_machine: "arm64",
            sys_platform: "darwin",
            os_name: "posix",
        }),
        Platform::Win64 => Some(SubdirMarkers {
            platform_system: "Windows",
            platform_machine: "AMD64",
            sys_platform: "win32",
            os_name: "nt",
        }),
        Platform::WinArm64 => Some(SubdirMarkers {
            platform_system: "Windows",
            platform_machine: "ARM64",
            sys_platform: "win32",
            os_name: "nt",
        }),
        _ => None,
    }
}

/// One `key == value` leaf, as a `MarkerTree` -- never a formatted-then-
/// reparsed string; see this module's docs.
fn equals(key: MarkerValueString, value: &str) -> MarkerTree {
    MarkerTree::expression(MarkerExpression::String {
        key,
        operator: MarkerOperator::Equal,
        value: value.into(),
    })
}

/// This machine's known marker facts, as a `MarkerTree` assumption for
/// [`crate::to_matchspec_condition`] -- see this module's docs for what's
/// in it (six equalities: two fixed CPython-policy constants, four
/// subdir-derived host facts) and what's deliberately not
/// (`platform_release`/`platform_version`).
///
/// Pure function of `subdir`, no I/O: safe to call once per process and
/// reuse the resulting `MarkerTree` (a `Copy` interned handle) for every
/// dependency conversion.
///
/// # Errors
///
/// Returns [`UnsupportedPlatform`] if `subdir` isn't one of the six
/// subdirs `ana` knows how to install onto.
pub fn known_values_assumption(subdir: Platform) -> Result<MarkerTree, UnsupportedPlatform> {
    let markers = subdir_markers(subdir).ok_or(UnsupportedPlatform(subdir))?;
    Ok([
        (MarkerValueString::ImplementationName, "cpython"),
        (MarkerValueString::PlatformPythonImplementation, "CPython"),
        (MarkerValueString::OsName, markers.os_name),
        (MarkerValueString::SysPlatform, markers.sys_platform),
        (MarkerValueString::PlatformSystem, markers.platform_system),
        (MarkerValueString::PlatformMachine, markers.platform_machine),
    ]
    .into_iter()
    .fold(MarkerTree::TRUE, |acc, (key, value)| {
        acc.and(equals(key, value))
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::str::FromStr;

    use uv_pep508::Requirement;

    use super::*;

    /// `entry` parsed as a `Requirement`, with an explicit `VerbatimUrl`
    /// URL type -- without it, type inference has nothing to pin `T` to,
    /// since only `requirement.marker` (a field independent of `T`) is
    /// ever used below.
    fn req(entry: &str) -> Requirement {
        Requirement::from_str(entry).unwrap()
    }

    /// `entry`'s marker, restricted under `subdir`'s assumption -- the
    /// same call [`crate::to_matchspec_condition`] makes, but exposed
    /// directly here so this module's own tests can inspect the residual
    /// without going through the whole `Applicability`/`Unconvertible`
    /// orchestration in `condition.rs`.
    fn restricted(entry: &str, subdir: Platform) -> MarkerTree {
        let requirement = req(entry);
        let assumption = known_values_assumption(subdir).unwrap();
        requirement.marker.restrict(assumption)
    }

    mod unsupported_platform {
        use super::*;

        #[test]
        fn every_supported_subdir_builds_an_assumption() {
            for subdir in [
                Platform::Linux64,
                Platform::LinuxAarch64,
                Platform::Osx64,
                Platform::OsxArm64,
                Platform::Win64,
                Platform::WinArm64,
            ] {
                assert!(
                    known_values_assumption(subdir).is_ok(),
                    "{subdir:?} should be supported"
                );
            }
        }

        #[test]
        fn an_unsupported_platform_is_rejected() {
            let err = known_values_assumption(Platform::Linux32).unwrap_err();
            assert_eq!(err, UnsupportedPlatform(Platform::Linux32));
        }

        #[test]
        fn noarch_is_rejected() {
            // `NoArch` has no `sys_platform`/`os_name`/etc. of its own --
            // it isn't a real installation target, and shouldn't silently
            // build an assumption for one.
            assert!(known_values_assumption(Platform::NoArch).is_err());
        }
    }

    /// Every known-value equality actually resolves a marker referencing
    /// it -- one test per key, each on `linux-64` (an arbitrary but fixed
    /// choice; `subdir_table` below covers the per-subdir differences).
    mod known_key_resolution {
        use super::*;

        #[test]
        fn implementation_name_resolves() {
            assert!(restricted(
                r#"requests; implementation_name == "cpython""#,
                Platform::Linux64
            )
            .is_true());
            assert!(restricted(
                r#"requests; implementation_name == "pypy""#,
                Platform::Linux64
            )
            .is_false());
        }

        #[test]
        fn platform_python_implementation_resolves() {
            assert!(restricted(
                r#"requests; platform_python_implementation == "CPython""#,
                Platform::Linux64
            )
            .is_true());
            assert!(restricted(
                r#"requests; platform_python_implementation == "PyPy""#,
                Platform::Linux64
            )
            .is_false());
        }

        #[test]
        fn os_name_resolves() {
            assert!(restricted(r#"requests; os_name == "posix""#, Platform::Linux64).is_true());
            assert!(!restricted(r#"requests; os_name == "nt""#, Platform::Linux64).is_true());
            assert!(restricted(r#"requests; os_name != "nt""#, Platform::Linux64).is_true());
        }

        #[test]
        fn sys_platform_resolves() {
            assert!(
                restricted(r#"requests; sys_platform == "linux""#, Platform::Linux64).is_true()
            );
            assert!(
                restricted(r#"requests; sys_platform == "win32""#, Platform::Linux64).is_false()
            );
        }

        #[test]
        fn platform_system_resolves() {
            assert!(
                restricted(r#"requests; platform_system == "Linux""#, Platform::Linux64).is_true()
            );
            assert!(restricted(
                r#"requests; platform_system == "Darwin""#,
                Platform::Linux64
            )
            .is_false());
        }

        #[test]
        fn platform_machine_resolves() {
            assert!(restricted(
                r#"requests; platform_machine == "x86_64""#,
                Platform::Linux64
            )
            .is_true());
            assert!(restricted(
                r#"requests; platform_machine == "aarch64""#,
                Platform::Linux64
            )
            .is_false());
        }

        /// A deprecated marker-key alias (`os.name`, PEP 345 spelling)
        /// resolves identically to its canonical `os_name` form --
        /// confirmed directly against `uv_pep508` 0.12.6's own
        /// canonicalization (the internal BDD variable is
        /// `CanonicalMarkerValueString`, which unifies `OsName` and
        /// `OsNameDeprecated` into the same dimension), not assumed.
        #[test]
        fn deprecated_alias_resolves_the_same_as_the_canonical_key() {
            assert!(restricted(r#"requests; "posix" == os.name"#, Platform::Linux64).is_true());
        }

        /// Ordering comparators against a known string key are decidable
        /// too, not just `==`/`!=` -- `restrict()` uses the same
        /// lexicographic range machinery `evaluate()` does internally.
        #[test]
        fn ordering_comparators_against_a_known_key_resolve() {
            assert!(
                restricted(r#"requests; sys_platform >= "linux""#, Platform::Linux64).is_true()
            );
            assert!(
                restricted(r#"requests; sys_platform > "linux""#, Platform::Linux64).is_false()
            );
        }
    }

    /// The four subdir-derived keys, once per subdir -- pins the exact
    /// values against reroll's own already-validated table, not just "some
    /// value resolves."
    mod subdir_table {
        use super::*;

        #[test]
        fn linux_64() {
            assert!(restricted(
                r#"requests; platform_system == "Linux" and platform_machine == "x86_64" and 
                   sys_platform == "linux" and os_name == "posix""#,
                Platform::Linux64
            )
            .is_true());
        }

        #[test]
        fn linux_aarch64() {
            assert!(restricted(
                r#"requests; platform_system == "Linux" and platform_machine == "aarch64" and 
                   sys_platform == "linux" and os_name == "posix""#,
                Platform::LinuxAarch64
            )
            .is_true());
        }

        #[test]
        fn osx_64() {
            assert!(restricted(
                r#"requests; platform_system == "Darwin" and platform_machine == "x86_64" and 
                   sys_platform == "darwin" and os_name == "posix""#,
                Platform::Osx64
            )
            .is_true());
        }

        #[test]
        fn osx_arm64() {
            assert!(restricted(
                r#"requests; platform_system == "Darwin" and platform_machine == "arm64" and 
                   sys_platform == "darwin" and os_name == "posix""#,
                Platform::OsxArm64
            )
            .is_true());
        }

        /// Windows's `platform_machine` is `"AMD64"` -- uppercase, not
        /// `rattler_conda_types::Arch::as_str()`'s lowercase `"x86_64"`.
        /// This is the whole reason this table is hand-authored rather
        /// than derived from `Arch`; see the module docs.
        #[test]
        fn win_64() {
            assert!(restricted(
                r#"requests; platform_system == "Windows" and platform_machine == "AMD64" and 
                   sys_platform == "win32" and os_name == "nt""#,
                Platform::Win64
            )
            .is_true());
            assert!(
                restricted(r#"requests; platform_machine == "x86_64""#, Platform::Win64).is_false()
            );
        }

        #[test]
        fn win_arm64() {
            assert!(restricted(
                r#"requests; platform_system == "Windows" and platform_machine == "ARM64" and 
                   sys_platform == "win32" and os_name == "nt""#,
                Platform::WinArm64
            )
            .is_true());
            assert!(restricted(
                r#"requests; platform_machine == "arm64""#,
                Platform::WinArm64
            )
            .is_false());
        }
    }

    /// `platform_release`/`platform_version` are deliberately absent from
    /// the assumption -- a marker referencing them is left untouched by
    /// `restrict()`, not resolved either way.
    mod deliberately_excluded_keys {
        use super::*;

        #[test]
        fn platform_release_is_left_unresolved() {
            let residual = restricted(
                r#"requests; platform_release == "5.10.0""#,
                Platform::Linux64,
            );
            assert!(!residual.is_true());
            assert!(!residual.is_false());
        }

        #[test]
        fn platform_version_is_left_unresolved() {
            let residual = restricted(
                r#"requests; platform_version == "5.10.0-generic""#,
                Platform::Linux64,
            );
            assert!(!residual.is_true());
            assert!(!residual.is_false());
        }

        /// A marker mixing an excluded key with a known key still
        /// simplifies the known part -- `restrict()` works clause by
        /// clause, not all-or-nothing.
        #[test]
        fn excluded_key_alongside_a_known_key_only_leaves_the_excluded_part() {
            let residual = restricted(
                r#"requests; platform_release == "5.10.0" and sys_platform == "linux""#,
                Platform::Linux64,
            );
            // sys_platform == "linux" is true under the assumption, so it
            // drops out of the `and`, leaving just the release clause.
            let expected =
                uv_pep508::MarkerTree::from_str(r#"platform_release == "5.10.0""#).unwrap();
            assert_eq!(residual, expected);
        }
    }

    /// python_version/python_full_version/implementation_version are the
    /// free variable: never resolved by the assumption, always left as-is
    /// in the residual.
    mod free_variable {
        use super::*;

        #[test]
        fn python_version_is_left_unresolved() {
            let residual = restricted(r#"requests; python_version >= "3.9""#, Platform::Linux64);
            assert!(!residual.is_true());
            assert!(!residual.is_false());
        }

        #[test]
        fn python_full_version_is_left_unresolved() {
            let residual = restricted(
                r#"requests; python_full_version >= "3.9.0""#,
                Platform::Linux64,
            );
            assert!(!residual.is_true());
            assert!(!residual.is_false());
        }

        #[test]
        fn implementation_version_is_left_unresolved() {
            let residual = restricted(
                r#"requests; implementation_version >= "3.9.0""#,
                Platform::Linux64,
            );
            assert!(!residual.is_true());
            assert!(!residual.is_false());
        }

        /// A known-key clause combined (via `or`) with a free-variable
        /// clause collapses the whole marker to `TRUE` when the known
        /// part alone already holds -- `or` short-circuits even though
        /// the other side is undecidable.
        #[test]
        fn known_key_true_short_circuits_an_or_with_a_free_variable() {
            let residual = restricted(
                r#"requests; sys_platform == "linux" or python_version >= "3.9""#,
                Platform::Linux64,
            );
            assert!(residual.is_true());
        }

        /// Mirrors this exact shape from `restrict()`'s own upstream
        /// test (`uv-pep508`'s `tree.rs`): a disjunction of subdir facts
        /// `and`ed with a free-variable clause collapses to just the
        /// free-variable residual.
        #[test]
        fn subdir_disjunction_and_free_variable_collapses_to_the_free_variable() {
            let residual = restricted(
                r#"requests; ((platform_machine == "x86_64" and sys_platform == "darwin") or 
                   (platform_machine == "x86_64" and sys_platform == "linux") or 
                   (platform_machine == "AMD64" and sys_platform == "win32")) and 
                   python_version < "3.11""#,
                Platform::Linux64,
            );
            let expected = uv_pep508::MarkerTree::from_str(r#"python_version < "3.11""#).unwrap();
            assert_eq!(residual, expected);
        }
    }

    /// `restrict()`'s own doc comment warns that the residual "may have a
    /// different value outside of [the assumption]" -- these tests check
    /// the identity this workspace actually relies on
    /// (`marker.restrict(assumption).and(assumption) == marker.and(assumption)`,
    /// the same one `restrict()`'s own upstream test uses) across a wide
    /// sweep of shapes, rather than trusting the doc comment's one
    /// worked example to generalize. See
    /// `investigations/pep508_to_matchspec_api.md`'s testing-strategy
    /// section.
    mod restrict_semantics {
        use super::*;

        fn assert_reconjoining_reconstructs(entry: &str, subdir: Platform) {
            let requirement = req(entry);
            let assumption = known_values_assumption(subdir).unwrap();
            let marker = requirement.marker;
            let simplified = marker.restrict(assumption);
            let reconstructed = simplified.and(assumption);
            let expected = marker.and(assumption);
            assert_eq!(
                reconstructed, expected,
                "entry {entry:?} subdir {subdir:?}: restrict() then re-and()ing the \
                 assumption should reconstruct the same thing as and()ing the assumption \
                 onto the original marker directly"
            );
        }

        #[test]
        fn known_key_equality() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "linux""#,
                Platform::Linux64,
            );
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "win32""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn known_key_inequality() {
            assert_reconjoining_reconstructs(r#"requests; os_name != "nt""#, Platform::Linux64);
        }

        #[test]
        fn known_key_ordering() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform >= "linux""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn free_variable_alone() {
            assert_reconjoining_reconstructs(
                r#"requests; python_version >= "3.9""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn conjunction_of_known_and_free() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "linux" and python_version >= "3.9""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn disjunction_of_known_and_free() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "win32" or python_version >= "3.9""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn disjunction_of_two_known_keys() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "linux" or sys_platform == "darwin""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn extra_alongside_an_environment_clause() {
            assert_reconjoining_reconstructs(
                r#"requests; extra == "foo" and sys_platform == "linux""#,
                Platform::Linux64,
            );
            assert_reconjoining_reconstructs(
                r#"requests; extra == "foo" or sys_platform == "win32""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn deliberately_excluded_key_alongside_a_known_key() {
            assert_reconjoining_reconstructs(
                r#"requests; platform_release == "5.10.0" and sys_platform == "linux""#,
                Platform::Linux64,
            );
            assert_reconjoining_reconstructs(
                r#"requests; platform_release == "5.10.0" or sys_platform == "win32""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn deliberately_excluded_key_alongside_the_free_variable() {
            assert_reconjoining_reconstructs(
                r#"requests; platform_version == "5.10.0-generic" and python_version >= "3.9""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn three_way_mix_of_known_free_and_excluded_keys() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "linux" and python_version >= "3.9" and 
                   platform_release == "5.10.0""#,
                Platform::Linux64,
            );
        }

        #[test]
        fn already_true_under_the_assumption_restricts_to_true() {
            let assumption = known_values_assumption(Platform::Linux64).unwrap();
            assert!(assumption.restrict(assumption).is_true());
        }

        /// Runs the same sweep on a second subdir, to check the identity
        /// isn't accidentally only true for `linux-64`'s particular
        /// assumption shape.
        #[test]
        fn holds_on_a_different_subdir_too() {
            assert_reconjoining_reconstructs(
                r#"requests; sys_platform == "win32" and python_version >= "3.9""#,
                Platform::WinArm64,
            );
        }
    }
}
