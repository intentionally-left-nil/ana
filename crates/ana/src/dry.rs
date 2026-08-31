//! `ana sync --dry`: compute what a real sync would do, without writing
//! `ana.lock` or touching the environment, and render the result in one
//! of four `--format`s.
//!
//! [`plan_sync`] is the read-only counterpart to `crate::sync_command`,
//! following the same two-phase shape (the current platform under the
//! environment's advisory lock, then any `--subdir` platforms after it's
//! released) but calling `ana_lockfile`'s `plan_*` functions instead of
//! `ensure_current_platform_locked`/`check`'s writing ones. [`render`]
//! then turns the resulting [`SyncPlan`] into text: `Toml`/`Json` are the
//! exact new `ana.lock` `ana_lockfile::render_sections` would produce;
//! `Diff` is a unified diff of the old and new text; `Summary` is a
//! per-package, one-line-per-package report, ANSI-colored when stdout is
//! a terminal.
//!
//! [`plan_sync_with_fallback`] additionally covers `config.toml`'s
//! `dry_solve_channels`: if solving with the caller's ordinary channels
//! fails, and a wider fallback scope is available, it retries once with
//! that wider scope before giving up. See [`DryOutcome`].

use std::borrow::Cow;
use std::io::IsTerminal;
use std::path::Path;

use ana_environment::Environment;
use ana_lockfile::{
    plan_current_platform, plan_platforms, PlatformSection, SectionPlan, SolveScope, Solver,
};
use rattler_conda_types::{Platform, RepoDataRecord};

use crate::Error;

/// `ana sync --dry --format`'s values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// The exact new `ana.lock`, as TOML.
    Toml,
    /// The exact new `ana.lock`, as JSON.
    Json,
    /// A unified diff between the existing `ana.lock` and the new one.
    Diff,
    /// A one-line-per-package, ANSI-colored report (the default).
    Summary,
}

/// What `ana sync --dry [--subdir <platform>]...` would do: one
/// [`SectionPlan`] for the current platform, plus one per requested
/// `--subdir`.
#[derive(Debug)]
pub struct SyncPlan {
    pub current: SectionPlan,
    pub subdirs: Vec<SectionPlan>,
}

impl SyncPlan {
    /// Every platform this plan covers, current platform first.
    fn sections(&self) -> impl Iterator<Item = &SectionPlan> {
        std::iter::once(&self.current).chain(self.subdirs.iter())
    }
}

/// Computes [`SyncPlan`]. See the module docs for how this mirrors
/// `crate::sync_command` without ever writing anything. The `--subdir`
/// phase covers the same platform set a real sync's `check --fix` phase
/// would (see [`plan_platforms`]), so the report names every section a
/// real sync would rewrite.
pub fn plan_sync(
    env: &Environment,
    subdirs: &[Platform],
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
) -> Result<SyncPlan, Error> {
    let current = plan_current_platform(env, Platform::current(), scope, solver)?;

    let subdirs = if subdirs.is_empty() {
        Vec::new()
    } else {
        plan_platforms(env, subdirs, scope, solver)?
    };

    Ok(SyncPlan { current, subdirs })
}

/// [`plan_sync_with_fallback`]'s result: whether `scope`'s own channels
/// were enough, or the plan only exists because `fallback` was searched
/// too.
#[derive(Debug)]
pub enum DryOutcome {
    /// Solved with `scope`, no fallback needed.
    Direct(SyncPlan),
    /// `scope` alone failed to solve; this plan came from retrying with a
    /// wider fallback scope instead.
    Widened(SyncPlan),
}

/// [`plan_sync`] against `scope`; if that fails and `fallback` is `Some`,
/// retries once against it before giving up. A retry that also fails
/// surfaces the *first* (unwidened) error, never the widened attempt's --
/// widening is a rescue for a solve the user's configured channels
/// couldn't complete, not a replacement for reporting failures in terms
/// of those channels.
pub fn plan_sync_with_fallback(
    env: &Environment,
    subdirs: &[Platform],
    scope: &SolveScope<'_>,
    fallback: Option<&SolveScope<'_>>,
    solver: &dyn Solver,
) -> Result<DryOutcome, Error> {
    match plan_sync(env, subdirs, scope, solver) {
        Ok(plan) => Ok(DryOutcome::Direct(plan)),
        Err(original_err) => {
            let Some(fallback) = fallback else {
                return Err(original_err);
            };
            match plan_sync(env, subdirs, fallback, solver) {
                Ok(plan) => Ok(DryOutcome::Widened(plan)),
                Err(_widened_err) => Err(original_err),
            }
        }
    }
}

/// Renders `plan` in `format`. `Toml`/`Json`/`Diff` all read `lock_path`'s
/// current on-disk content (missing reads as empty) to render every
/// untouched platform section/comment exactly as a real write would
/// leave it; `Summary` needs no disk access beyond what `plan_sync`
/// already did.
pub fn render(plan: &SyncPlan, lock_path: &Path, format: Format) -> Result<String, Error> {
    match format {
        Format::Toml => Ok(render_lock_file(plan, lock_path)?.toml()),
        Format::Json => Ok(render_lock_file(plan, lock_path)?.json()?),
        Format::Diff => render_diff(plan, lock_path),
        Format::Summary => Ok(render_summary(plan)),
    }
}

/// Only the sections a real sync would actually rewrite: an unchanged
/// section is spliced around, not rewritten, so rendering it would
/// report normalization differences (ordering, comments, a missing
/// `version` key) that a real sync leaves alone.
fn sections_for_render(plan: &SyncPlan) -> Vec<(Platform, &PlatformSection)> {
    plan.sections()
        .filter(|section_plan| section_plan.changed())
        .map(|section_plan| (section_plan.platform, &section_plan.next))
        .collect()
}

fn render_lock_file(
    plan: &SyncPlan,
    lock_path: &Path,
) -> Result<ana_lockfile::RenderedLockFile, Error> {
    Ok(ana_lockfile::render_sections(
        lock_path,
        &sections_for_render(plan),
    )?)
}

fn render_diff(plan: &SyncPlan, lock_path: &Path) -> Result<String, Error> {
    let old = std::fs::read_to_string(lock_path).unwrap_or_default();
    let new = render_lock_file(plan, lock_path)?.toml();
    let diff = similar::TextDiff::from_lines(old.as_str(), new.as_str());
    Ok(diff
        .unified_diff()
        .header("a/ana.lock", "b/ana.lock")
        .to_string())
}

const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const BLUE: &str = "\x1b[34m";
const WHITE: &str = "\x1b[37m";
const RESET: &str = "\x1b[0m";

/// One package's before/after state between `previous` and `next`.
enum PackageChange {
    Added { version: String },
    Removed { version: String },
    Updated { from: String, to: String },
    Unchanged { version: String },
}

/// One line of [`render_summary`]'s report.
struct PackageDiff {
    name: String,
    change: PackageChange,
}

impl PackageDiff {
    /// Renders this one line, ANSI-colored when `color` is set: green for
    /// `Added`, red for `Removed`, blue for `Updated`, plain/white for
    /// `Unchanged`. Names and versions come from `ana.lock` or channel
    /// repodata -- untrusted input -- so anything with control characters
    /// is escaped rather than written to the terminal raw.
    fn render(&self, color: bool) -> String {
        let (symbol, code, detail) = match &self.change {
            PackageChange::Added { version } => ('+', GREEN, escape_control(version)),
            PackageChange::Removed { version } => ('-', RED, escape_control(version)),
            PackageChange::Updated { from, to } => (
                '~',
                BLUE,
                format!("{} -> {}", escape_control(from), escape_control(to)).into(),
            ),
            PackageChange::Unchanged { version } => (' ', WHITE, escape_control(version)),
        };
        let name = escape_control(&self.name);
        if color {
            format!("{code}{symbol} {name} {detail}{RESET}")
        } else {
            format!("{symbol} {name} {detail}")
        }
    }
}

/// `text` with control characters rendered inert (`\u{...}` escapes) for
/// terminal output. Package names deserialize unchecked
/// (`PackageName::new_unchecked`), so a hand-edited `ana.lock` or a
/// compromised channel's repodata could otherwise smuggle ESC/CSI/OSC
/// sequences into the summary. Borrowed when clean, which legitimate
/// names and versions always are.
fn escape_control(text: &str) -> Cow<'_, str> {
    if text.chars().any(char::is_control) {
        Cow::Owned(text.chars().flat_map(char::escape_debug).collect())
    } else {
        Cow::Borrowed(text)
    }
}

/// Diffs `previous`'s and `next`'s packages by name: one [`PackageDiff`]
/// per name appearing in either, sorted by name. A name present in both
/// but with an unchanged version *and* build is `Unchanged`; any other
/// version/build difference is `Updated`.
fn package_diffs(previous: Option<&PlatformSection>, next: &PlatformSection) -> Vec<PackageDiff> {
    diff_packages(
        previous
            .map(|section| section.packages.as_slice())
            .unwrap_or(&[]),
        &next.packages,
    )
}

fn diff_packages(previous: &[RepoDataRecord], next: &[RepoDataRecord]) -> Vec<PackageDiff> {
    use std::collections::BTreeMap;

    fn by_name(records: &[RepoDataRecord]) -> BTreeMap<String, &RepoDataRecord> {
        records
            .iter()
            .map(|record| {
                (
                    record.package_record.name.as_normalized().to_string(),
                    record,
                )
            })
            .collect()
    }
    let before = by_name(previous);
    let after = by_name(next);

    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();

    names
        .into_iter()
        .map(|name| {
            let change = match (before.get(name), after.get(name)) {
                (None, Some(record)) => PackageChange::Added {
                    version: record.package_record.version.to_string(),
                },
                (Some(record), None) => PackageChange::Removed {
                    version: record.package_record.version.to_string(),
                },
                (Some(old), Some(new)) => {
                    if old.package_record.version == new.package_record.version
                        && old.package_record.build == new.package_record.build
                    {
                        PackageChange::Unchanged {
                            version: new.package_record.version.to_string(),
                        }
                    } else {
                        PackageChange::Updated {
                            from: old.package_record.version.to_string(),
                            to: new.package_record.version.to_string(),
                        }
                    }
                }
                (None, None) => unreachable!("name came from `before` or `after`'s own keys"),
            };
            PackageDiff {
                name: name.clone(),
                change,
            }
        })
        .collect()
}

/// Whether ANSI color codes should be emitted: only when stdout is a
/// terminal, and `NO_COLOR` (<https://no-color.org/>) is unset.
fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn render_summary(plan: &SyncPlan) -> String {
    let color = color_enabled();
    let multi_platform = !plan.subdirs.is_empty();
    let mut out = String::new();

    for section in plan.sections() {
        if multi_platform {
            out.push_str(&format!("{}:\n", section.platform));
        }
        let diffs = package_diffs(section.previous.as_ref(), &section.next);
        let indent = if multi_platform { "  " } else { "" };
        if diffs.is_empty() {
            out.push_str(indent);
            out.push_str("(no packages)\n");
            continue;
        }
        for diff in diffs {
            out.push_str(indent);
            out.push_str(&diff.render(color));
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;

    use ana_channels::ChannelPolicy;
    use ana_environment::{EnvironmentRequest, RequirementInput};
    use ana_lockfile::SolveRequest;
    use ana_pypi_conda_map::MappingHandle;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{Channel, PackageName, PackageRecord, Version};

    use super::*;

    fn no_mapping() -> MappingHandle {
        MappingHandle::from_map(HashMap::new())
    }

    fn test_channels() -> Vec<String> {
        vec!["defaults".to_string()]
    }

    fn record(name: &str, version: &str, build: &str) -> RepoDataRecord {
        let package_record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            build.to_string(),
        );
        let identifier =
            DistArchiveIdentifier::try_from_filename(&format!("{name}-{version}-{build}.conda"))
                .unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url: url::Url::parse(&format!(
                "https://repo.anaconda.com/pkgs/main/linux-64/{name}-{version}-{build}.conda"
            ))
            .unwrap(),
            channel: None,
        }
    }

    fn section(packages: Vec<RepoDataRecord>) -> PlatformSection {
        PlatformSection {
            requirements: Vec::new(),
            packages,
            channels_digest: String::new(),
        }
    }

    /// A solver that always resolves every requested spec to a canned
    /// `empty-0.1.0-h4616a5c_0` record.
    struct FakeSolver {
        calls: std::sync::Mutex<u32>,
    }

    impl FakeSolver {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl Solver for FakeSolver {
        fn solve(
            &self,
            _request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            *self.calls.lock().unwrap() += 1;
            Ok(vec![record("numpy", "1.0.0", "py312h1234567_0")])
        }
    }

    /// A solver that always fails, tagging each error with its call
    /// number so a test can tell which attempt's error propagated.
    struct FailingSolver {
        calls: std::sync::Mutex<u32>,
    }

    impl FailingSolver {
        fn new() -> Self {
            Self {
                calls: std::sync::Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl Solver for FailingSolver {
        fn solve(
            &self,
            _request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            Err(format!("solve failed (attempt {calls})").into())
        }
    }

    /// A solver that fails unless `required_channel_substring` appears in
    /// the requested channels -- simulates an unwidened solve failing and
    /// a widened one (searching a channel matching the substring)
    /// succeeding.
    struct RequiresChannelSolver {
        required_channel_substring: &'static str,
        calls: std::sync::Mutex<u32>,
    }

    impl RequiresChannelSolver {
        fn new(required_channel_substring: &'static str) -> Self {
            Self {
                required_channel_substring,
                calls: std::sync::Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl Solver for RequiresChannelSolver {
        fn solve(
            &self,
            request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            *self.calls.lock().unwrap() += 1;
            let has_required = request.channels.iter().any(|channel: &Channel| {
                channel
                    .base_url
                    .as_str()
                    .contains(self.required_channel_substring)
            });
            if has_required {
                Ok(vec![record("numpy", "1.0.0", "py312h1234567_0")])
            } else {
                Err("no version of numpy satisfies the request".into())
            }
        }
    }

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
dependencies = ["numpy"]
"#;

    fn project_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), PYPROJECT).unwrap();
        dir
    }

    fn resolve(dir: &std::path::Path, cache_root: &std::path::Path) -> Environment {
        ana_environment::resolve(&EnvironmentRequest {
            input: RequirementInput::ProjectDir { dir },
            groups: &[],
            extra: &[],
            platform: Platform::current(),
            pypi_to_conda_map: &no_mapping(),
            global_cache_root: cache_root,
        })
        .unwrap()
    }

    fn scope<'a>(
        map: &'a ana_pypi_conda_map::MappingHandle,
        policy: &'a ChannelPolicy,
    ) -> SolveScope<'a> {
        SolveScope {
            channels: policy,
            pypi_to_conda_map: map,
        }
    }

    #[test]
    fn plan_sync_with_no_lock_reports_a_change_and_never_writes_ana_lock() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver::new();

        let plan = plan_sync(&env, &[], &scope(&map, &policy), &solver).unwrap();

        assert!(plan.current.changed());
        assert!(plan.current.previous.is_none());
        assert_eq!(solver.calls(), 1);
        assert!(
            !dir.path().join("ana.lock").exists(),
            "a dry run must never write ana.lock"
        );
        assert!(
            !env.paths().env_path.exists(),
            "a dry run must never touch the environment"
        );
    }

    #[test]
    fn plan_sync_subdir_never_writes_ana_lock_or_touches_env_path() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver::new();
        let foreign = match Platform::current() {
            Platform::Win64 => Platform::Osx64,
            _ => Platform::Win64,
        };

        let plan = plan_sync(&env, &[foreign], &scope(&map, &policy), &solver).unwrap();

        assert_eq!(plan.subdirs.len(), 1);
        assert_eq!(plan.subdirs[0].platform, foreign);
        assert!(!dir.path().join("ana.lock").exists());
        assert!(!env.paths().env_path.exists());
    }

    #[test]
    fn render_toml_produces_a_lock_file_containing_the_planned_package() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: None,
                next: section(vec![record("numpy", "1.0.0", "py312h1234567_0")]),
            },
            subdirs: Vec::new(),
        };
        let lock_path = tempfile::tempdir().unwrap().path().join("ana.lock");

        let rendered = render(&plan, &lock_path, Format::Toml).unwrap();

        assert!(rendered.contains("linux-64"));
        assert!(rendered.contains("numpy"));
        assert!(!lock_path.exists(), "rendering must never write to disk");
    }

    #[test]
    fn render_json_produces_valid_json_containing_the_planned_package() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: None,
                next: section(vec![record("numpy", "1.0.0", "py312h1234567_0")]),
            },
            subdirs: Vec::new(),
        };
        let lock_path = tempfile::tempdir().unwrap().path().join("ana.lock");

        let rendered = render(&plan, &lock_path, Format::Json).unwrap();

        assert!(rendered.contains("\"numpy\""));
        assert!(rendered.contains("linux-64"));
    }

    #[test]
    fn render_diff_against_a_missing_lock_shows_every_line_added() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: None,
                next: section(vec![record("numpy", "1.0.0", "py312h1234567_0")]),
            },
            subdirs: Vec::new(),
        };
        let lock_path = tempfile::tempdir().unwrap().path().join("ana.lock");

        let rendered = render(&plan, &lock_path, Format::Diff).unwrap();

        assert!(rendered.starts_with("--- a/ana.lock"));
        assert!(rendered.contains("+++ b/ana.lock"));
        assert!(rendered.contains("+name = \"numpy\""));
        assert!(
            !rendered
                .lines()
                .any(|line| line.starts_with('-') && !line.starts_with("---")),
            "a from-scratch lock has nothing to remove: {rendered}"
        );
    }

    #[test]
    fn render_diff_against_an_unchanged_lock_is_empty() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: Some(section(vec![record("numpy", "1.0.0", "py312h1234567_0")])),
                next: section(vec![record("numpy", "1.0.0", "py312h1234567_0")]),
            },
            subdirs: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        let existing = render_lock_file(&plan, &lock_path).unwrap().toml();
        fs::write(&lock_path, &existing).unwrap();

        let rendered = render(&plan, &lock_path, Format::Diff).unwrap();

        assert_eq!(rendered, "", "an unchanged plan must produce an empty diff");
    }

    #[test]
    fn render_toml_with_an_unchanged_plan_preserves_the_existing_file_byte_for_byte() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: Some(section(vec![record("numpy", "1.0.0", "0")])),
                next: section(vec![record("numpy", "1.0.0", "0")]),
            },
            subdirs: Vec::new(),
        };
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("ana.lock");
        // No `version` key and a hand-written comment: a real sync leaves
        // this file alone when nothing changed, so the dry render must
        // too -- no version stamp, no renormalized section.
        let text = "# a hand-written comment\n\n[[platforms.linux-64.packages]]\nname = \"numpy\"\nversion = \"1.0.0\"\n";
        fs::write(&lock_path, text).unwrap();

        let rendered = render(&plan, &lock_path, Format::Toml).unwrap();

        assert_eq!(rendered, text);
    }

    #[test]
    fn render_summary_escapes_control_characters_in_package_names() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: None,
                next: section(vec![record(
                    "evil\x1b]8;;https://example.com\x07pkg",
                    "1.0.0",
                    "0",
                )]),
            },
            subdirs: Vec::new(),
        };

        let rendered = render_summary(&plan);

        assert!(
            !rendered.contains('\x1b') || rendered.contains("\\u{1b}"),
            "raw ESC must never reach the terminal: {rendered:?}"
        );
        assert!(rendered.contains("\\u{1b}"), "escaped form: {rendered:?}");
        assert!(!rendered.contains('\x07'));
    }

    #[test]
    fn render_summary_marks_added_removed_updated_and_unchanged_packages() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: Some(section(vec![
                    record("removed-pkg", "1.0.0", "0"),
                    record("updated-pkg", "1.0.0", "0"),
                    record("same-pkg", "1.0.0", "0"),
                ])),
                next: section(vec![
                    record("added-pkg", "2.0.0", "0"),
                    record("updated-pkg", "2.0.0", "0"),
                    record("same-pkg", "1.0.0", "0"),
                ]),
            },
            subdirs: Vec::new(),
        };

        let rendered = render_summary(&plan);
        let lines: Vec<&str> = rendered.lines().collect();

        assert!(lines.contains(&"+ added-pkg 2.0.0"));
        assert!(lines.contains(&"- removed-pkg 1.0.0"));
        assert!(lines.contains(&"~ updated-pkg 1.0.0 -> 2.0.0"));
        assert!(lines.contains(&"  same-pkg 1.0.0"));
    }

    #[test]
    fn render_summary_reports_no_packages_for_an_empty_section() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: None,
                next: section(Vec::new()),
            },
            subdirs: Vec::new(),
        };

        assert_eq!(render_summary(&plan), "(no packages)\n");
    }

    #[test]
    fn render_summary_with_a_subdir_prefixes_each_platform_with_its_own_header() {
        let plan = SyncPlan {
            current: SectionPlan {
                platform: Platform::Linux64,
                previous: None,
                next: section(vec![record("numpy", "1.0.0", "0")]),
            },
            subdirs: vec![SectionPlan {
                platform: Platform::OsxArm64,
                previous: None,
                next: section(vec![record("numpy", "1.0.0", "0")]),
            }],
        };

        let rendered = render_summary(&plan);

        assert!(rendered.contains("linux-64:\n"));
        assert!(rendered.contains("osx-arm64:\n"));
        assert!(rendered.contains("  + numpy 1.0.0"));
    }

    #[test]
    fn plan_sync_with_the_current_platform_as_a_subdir_plans_it_once() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver::new();

        let plan = plan_sync(
            &env,
            &[Platform::current(), Platform::current()],
            &scope(&map, &policy),
            &solver,
        )
        .unwrap();

        assert!(
            plan.subdirs.is_empty(),
            "the current platform is plan.current's alone, never a subdir's"
        );
        assert_eq!(solver.calls(), 1, "the current platform is solved once");
    }

    #[test]
    fn plan_sync_with_fallback_never_touches_the_fallback_when_the_direct_solve_succeeds() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let widened =
            ChannelPolicy::new(&["defaults".to_string(), "staging".to_string()], &[]).unwrap();
        let solver = RequiresChannelSolver::new("repo.anaconda.com");

        let outcome = plan_sync_with_fallback(
            &env,
            &[],
            &scope(&map, &policy),
            Some(&scope(&map, &widened)),
            &solver,
        )
        .unwrap();

        assert!(matches!(outcome, DryOutcome::Direct(_)));
        assert_eq!(solver.calls(), 1, "the fallback must never be attempted");
    }

    #[test]
    fn plan_sync_with_fallback_widens_when_the_direct_solve_fails() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let widened =
            ChannelPolicy::new(&["defaults".to_string(), "staging".to_string()], &[]).unwrap();
        let solver = RequiresChannelSolver::new("staging");

        let outcome = plan_sync_with_fallback(
            &env,
            &[],
            &scope(&map, &policy),
            Some(&scope(&map, &widened)),
            &solver,
        )
        .unwrap();

        assert!(matches!(outcome, DryOutcome::Widened(_)));
        assert_eq!(
            solver.calls(),
            2,
            "the direct attempt fails, then the widened one succeeds"
        );
    }

    #[test]
    fn plan_sync_with_fallback_reports_the_first_error_when_both_attempts_fail() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let widened =
            ChannelPolicy::new(&["defaults".to_string(), "staging".to_string()], &[]).unwrap();
        let solver = FailingSolver::new();

        let err = plan_sync_with_fallback(
            &env,
            &[],
            &scope(&map, &policy),
            Some(&scope(&map, &widened)),
            &solver,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("attempt 1"),
            "the direct (first) attempt's error must be the one reported: {err}"
        );
        assert_eq!(solver.calls(), 2, "both attempts still run");
    }

    #[test]
    fn plan_sync_with_fallback_without_a_fallback_fails_after_one_attempt() {
        let dir = project_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FailingSolver::new();

        let err =
            plan_sync_with_fallback(&env, &[], &scope(&map, &policy), None, &solver).unwrap_err();

        assert!(err.to_string().contains("attempt 1"));
        assert_eq!(solver.calls(), 1, "no fallback means no second attempt");
    }
}
