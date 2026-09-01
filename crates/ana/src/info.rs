//! `ana info`: a read-only snapshot of what ana currently knows about
//! the project's environment for the current platform.
//!
//! [`gather`] computes the *desired* `ana.lock` current-platform
//! section exactly as `ana sync --dry` would
//! (`ana_lockfile::plan_current_platform`, solving over the network if
//! the committed `ana.lock` is stale), then separately reads the
//! environment's own lock (`.env/ana.lock`, via `ana_lockfile::EnvLock`)
//! only to decide whether the materialized environment matches that
//! desired section. Every other field -- the project file, the
//! converted matchspecs, the package set, whether it would be
//! sandboxed -- comes from the desired section alone, never from
//! `.env/ana.lock`. [`render`] formats the result as `Summary` or
//! `Json`.

use std::borrow::Cow;
use std::path::PathBuf;

use ana_environment::{Environment, RequirementOrigin};
use ana_lockfile::{plan_current_platform, EnvLock, SolveScope, Solver};
use rattler_conda_types::{Platform, RepoDataRecord};

use crate::dry::{color_enabled, escape_control, GREEN, RED, RESET};
use crate::Error;

/// `ana info --format`'s values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// A human-readable, multi-section report (the default).
    Summary,
    /// The same data as structured JSON.
    Json,
}

/// Which kind of file governs the project's requirements, and where it
/// lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    pub kind: &'static str,
    pub path: PathBuf,
}

/// One matchspec the desired environment would be solved from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchspecInfo {
    pub matchspec: String,
    pub source: String,
}

/// One package in the desired package set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
    /// `None` when the record's channel can't be determined from
    /// either its own `channel` field or its `url` (see
    /// `ana_channels::trusted_channel`/`artifact_channel`).
    pub channel: Option<String>,
}

/// `ana info`'s whole report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoReport {
    pub platform: Platform,
    pub project_file: Option<ProjectFile>,
    /// Whether `.env/`'s materialized environment already matches
    /// `packages` -- the only field derived from `.env/ana.lock`.
    pub in_sync: bool,
    pub matchspecs: Vec<MatchspecInfo>,
    pub packages: Vec<PackageInfo>,
    pub sandboxed: bool,
}

/// Computes [`InfoReport`] for `env` on `platform`. See the module docs
/// for exactly which fields read `.env/ana.lock` (only `in_sync`) versus
/// the desired section `plan_current_platform` computes (everything
/// else).
pub fn gather(
    env: &Environment,
    platform: Platform,
    scope: &SolveScope<'_>,
    solver: &dyn Solver,
    sandboxed_channels: &[String],
) -> Result<InfoReport, Error> {
    let plan = plan_current_platform(env, platform, scope, solver)?;

    let env_lock = EnvLock::read(&env.paths().env_lock_path(), platform);
    let mut installed = env_lock.section.unwrap_or_default();
    installed.canonicalize();
    let in_sync = installed.packages == plan.next.packages;

    let matchspecs = plan
        .next
        .requirements
        .into_iter()
        .map(|req| MatchspecInfo {
            matchspec: req.matchspec,
            source: req.source,
        })
        .collect();
    let packages = plan.next.packages.iter().map(package_info).collect();
    let sandboxed =
        crate::sandbox::packages_require_sandbox(sandboxed_channels, &plan.next.packages)?;

    Ok(InfoReport {
        platform,
        project_file: project_file(env.origin()),
        in_sync,
        matchspecs,
        packages,
        sandboxed,
    })
}

/// Renders `report` in `format`.
pub fn render(report: &InfoReport, format: Format) -> Result<String, serde_json::Error> {
    match format {
        Format::Summary => Ok(render_summary(report)),
        Format::Json => render_json(report),
    }
}

fn project_file(origin: &RequirementOrigin) -> Option<ProjectFile> {
    match origin {
        RequirementOrigin::PyprojectToml { path } => Some(ProjectFile {
            kind: "pyproject.toml",
            path: path.clone(),
        }),
        RequirementOrigin::RequirementsTxt { path } => Some(ProjectFile {
            kind: "requirements.txt",
            path: path.clone(),
        }),
        RequirementOrigin::EnvironmentYml { path } => Some(ProjectFile {
            kind: "environment.yml",
            path: path.clone(),
        }),
        // Unreachable via `ana info`, which never resolves a
        // `CommandLine`/`Script` origin -- matched exhaustively rather
        // than assumed.
        RequirementOrigin::CommandLine | RequirementOrigin::Script { .. } => None,
    }
}

fn package_info(record: &RepoDataRecord) -> PackageInfo {
    PackageInfo {
        name: record.package_record.name.as_normalized().to_string(),
        version: record.package_record.version.to_string(),
        channel: ana_channels::trusted_channel(record)
            .or_else(|| ana_channels::artifact_channel(&record.url))
            .map(|channel| channel.as_str().to_string()),
    }
}

fn render_summary(report: &InfoReport) -> String {
    let color = color_enabled();
    let mut out = String::new();

    let project_file = match &report.project_file {
        Some(file) => format!("{} ({})", file.path.display(), file.kind),
        None => "(none)".to_string(),
    };
    out.push_str(&format!("project file: {project_file}\n"));

    let sync_label = if report.in_sync {
        "in sync"
    } else {
        "out of sync"
    };
    if color {
        let code = if report.in_sync { GREEN } else { RED };
        out.push_str(&format!("environment: {code}{sync_label}{RESET}\n"));
    } else {
        out.push_str(&format!("environment: {sync_label}\n"));
    }

    out.push_str(&format!(
        "sandboxed: {}\n",
        if report.sandboxed { "yes" } else { "no" }
    ));

    out.push('\n');
    out.push_str(&format!("matchspecs ({}):\n", report.platform));
    if report.matchspecs.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let rows: Vec<(Cow<'_, str>, Cow<'_, str>)> = report
            .matchspecs
            .iter()
            .map(|matchspec| {
                (
                    escape_control(&matchspec.matchspec),
                    escape_control(&matchspec.source),
                )
            })
            .collect();
        let matchspec_width = column_width(rows.iter().map(|(matchspec, _)| matchspec.as_ref()));
        for (matchspec, source) in &rows {
            out.push_str(&format!("  {matchspec:<matchspec_width$}  ({source})\n"));
        }
    }

    out.push('\n');
    out.push_str(&format!("packages ({}):\n", report.platform));
    if report.packages.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let rows: Vec<(Cow<'_, str>, Cow<'_, str>, Cow<'_, str>)> = report
            .packages
            .iter()
            .map(|package| {
                let channel = match &package.channel {
                    Some(channel) => escape_control(channel),
                    None => Cow::Borrowed("(unknown channel)"),
                };
                (
                    escape_control(&package.name),
                    escape_control(&package.version),
                    channel,
                )
            })
            .collect();
        let name_width = column_width(rows.iter().map(|(name, _, _)| name.as_ref()));
        let version_width = column_width(rows.iter().map(|(_, version, _)| version.as_ref()));
        for (name, version, channel) in &rows {
            out.push_str(&format!(
                "  {name:<name_width$}  {version:<version_width$}  {channel}\n"
            ));
        }
    }

    out
}

/// The display width (in characters, not bytes) of `column`'s widest
/// entry -- the padding every other row in the same column is stretched
/// to, so `render_summary`'s tables line up regardless of how long any
/// one name/version/matchspec is. `0` for an empty column.
fn column_width<'a>(column: impl Iterator<Item = &'a str>) -> usize {
    column.map(|entry| entry.chars().count()).max().unwrap_or(0)
}

#[derive(serde::Serialize)]
struct JsonReport<'a> {
    platform: &'static str,
    project_file: Option<JsonProjectFile>,
    in_sync: bool,
    matchspecs: Vec<JsonMatchspec<'a>>,
    packages: Vec<JsonPackage<'a>>,
    sandboxed: bool,
}

#[derive(serde::Serialize)]
struct JsonProjectFile {
    kind: &'static str,
    path: String,
}

#[derive(serde::Serialize)]
struct JsonMatchspec<'a> {
    matchspec: &'a str,
    source: &'a str,
}

#[derive(serde::Serialize)]
struct JsonPackage<'a> {
    name: &'a str,
    version: &'a str,
    channel: Option<&'a str>,
}

fn render_json(report: &InfoReport) -> Result<String, serde_json::Error> {
    let json = JsonReport {
        platform: report.platform.as_str(),
        project_file: report.project_file.as_ref().map(|file| JsonProjectFile {
            kind: file.kind,
            path: file.path.display().to_string(),
        }),
        in_sync: report.in_sync,
        matchspecs: report
            .matchspecs
            .iter()
            .map(|matchspec| JsonMatchspec {
                matchspec: &matchspec.matchspec,
                source: &matchspec.source,
            })
            .collect(),
        packages: report
            .packages
            .iter()
            .map(|package| JsonPackage {
                name: &package.name,
                version: &package.version,
                channel: package.channel.as_deref(),
            })
            .collect(),
        sandboxed: report.sandboxed,
    };
    let mut rendered = serde_json::to_string_pretty(&json)?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;

    use ana_channels::ChannelPolicy;
    use ana_environment::{EnvironmentRequest, RequirementInput};
    use ana_lockfile::{PlatformSection, SolveRequest};
    use ana_pypi_conda_map::MappingHandle;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{PackageName, PackageRecord, Version};

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

    /// A solver that always resolves every requested spec to a canned
    /// `numpy-1.0.0-py312h1234567_0` record.
    struct FakeSolver;

    impl Solver for FakeSolver {
        fn solve(
            &self,
            _request: SolveRequest,
        ) -> Result<Vec<RepoDataRecord>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![record("numpy", "1.0.0", "py312h1234567_0")])
        }
    }

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
dependencies = ["numpy"]
"#;

    const PYPROJECT_WITH_REQUIRES_PYTHON: &str = r#"
[project]
name = "myproj"
requires-python = ">=3.9"
dependencies = ["numpy"]
"#;

    fn project_root(pyproject: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), pyproject).unwrap();
        dir
    }

    fn requirements_txt_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("requirements.txt"), "numpy\n").unwrap();
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
    fn gather_with_no_lock_reports_out_of_sync_and_never_writes_anything() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &[],
        )
        .unwrap();

        assert!(!report.in_sync);
        assert!(!report.packages.is_empty());
        assert!(
            !dir.path().join("ana.lock").exists(),
            "ana info must never write ana.lock"
        );
        assert!(
            !env.paths().env_path.exists(),
            "ana info must never touch the environment"
        );
    }

    #[test]
    fn gather_reports_in_sync_when_the_env_lock_matches_the_plan() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;
        let platform = Platform::current();

        let section = PlatformSection {
            requirements: Vec::new(),
            packages: vec![record("numpy", "1.0.0", "py312h1234567_0")],
            channels_digest: String::new(),
        };
        EnvLock::write(
            &env.paths().env_lock_path(),
            platform,
            false,
            Some(&section),
        )
        .unwrap();

        let report = gather(&env, platform, &scope(&map, &policy), &solver, &[]).unwrap();

        assert!(report.in_sync);
    }

    #[test]
    fn gather_reports_out_of_sync_when_the_env_lock_differs() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;
        let platform = Platform::current();

        let section = PlatformSection {
            requirements: Vec::new(),
            packages: vec![record("numpy", "0.1.0", "py312h1234567_0")],
            channels_digest: String::new(),
        };
        EnvLock::write(
            &env.paths().env_lock_path(),
            platform,
            false,
            Some(&section),
        )
        .unwrap();

        let report = gather(&env, platform, &scope(&map, &policy), &solver, &[]).unwrap();

        assert!(!report.in_sync);
    }

    #[test]
    fn gather_reports_out_of_sync_when_env_lock_is_dirty() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;
        let platform = Platform::current();

        // A dirty env lock (even one whose stored section matches the
        // plan) is never special-cased by `gather` -- the observable
        // outcome here is `in_sync == false` only because no section
        // was written at all, not because of `dirty` itself.
        EnvLock::write(&env.paths().env_lock_path(), platform, true, None).unwrap();

        let report = gather(&env, platform, &scope(&map, &policy), &solver, &[]).unwrap();

        assert!(!report.in_sync);
    }

    #[test]
    fn gather_reports_project_file_for_pyproject_toml() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &[],
        )
        .unwrap();

        let file = report.project_file.unwrap();
        assert_eq!(file.kind, "pyproject.toml");
        assert_eq!(file.path, dir.path().join("pyproject.toml"));
    }

    #[test]
    fn gather_reports_project_file_for_requirements_txt() {
        let dir = requirements_txt_root();
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &[],
        )
        .unwrap();

        let file = report.project_file.unwrap();
        assert_eq!(file.kind, "requirements.txt");
        assert_eq!(file.path, dir.path().join("requirements.txt"));
    }

    #[test]
    fn gather_converts_requires_python_into_a_matchspec() {
        let dir = project_root(PYPROJECT_WITH_REQUIRES_PYTHON);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &[],
        )
        .unwrap();

        assert!(report.matchspecs.contains(&MatchspecInfo {
            matchspec: "python >=3.9".to_string(),
            source: "requires-python".to_string(),
        }));
    }

    #[test]
    fn gather_reports_the_channel_for_each_package() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &[],
        )
        .unwrap();

        let numpy = report
            .packages
            .iter()
            .find(|package| package.name == "numpy")
            .unwrap();
        assert_eq!(
            numpy.channel.as_deref(),
            Some("https://repo.anaconda.com/pkgs/main/")
        );
    }

    #[test]
    fn gather_reports_sandboxed_true_when_a_package_falls_under_a_sandboxed_channel() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;
        let sandboxed_channels = vec!["defaults".to_string()];

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &sandboxed_channels,
        )
        .unwrap();

        assert!(report.sandboxed);
    }

    #[test]
    fn gather_reports_sandboxed_false_when_none_do() {
        let dir = project_root(PYPROJECT);
        let cache_root = tempfile::tempdir().unwrap();
        let env = resolve(dir.path(), cache_root.path());
        let map = no_mapping();
        let policy = ChannelPolicy::new(&test_channels(), &[]).unwrap();
        let solver = FakeSolver;
        let sandboxed_channels = vec!["bioconda".to_string()];

        let report = gather(
            &env,
            Platform::current(),
            &scope(&map, &policy),
            &solver,
            &sandboxed_channels,
        )
        .unwrap();

        assert!(!report.sandboxed);
    }

    fn report(
        in_sync: bool,
        matchspecs: Vec<MatchspecInfo>,
        packages: Vec<PackageInfo>,
    ) -> InfoReport {
        InfoReport {
            platform: Platform::Linux64,
            project_file: Some(ProjectFile {
                kind: "pyproject.toml",
                path: PathBuf::from("/project/pyproject.toml"),
            }),
            in_sync,
            matchspecs,
            packages,
            sandboxed: false,
        }
    }

    #[test]
    fn render_summary_reports_in_sync_and_out_of_sync() {
        let in_sync = render_summary(&report(true, Vec::new(), Vec::new()));
        assert!(in_sync.contains("in sync"), "{in_sync}");
        assert!(!in_sync.contains("out of sync"), "{in_sync}");

        let out_of_sync = render_summary(&report(false, Vec::new(), Vec::new()));
        assert!(out_of_sync.contains("out of sync"), "{out_of_sync}");
    }

    #[test]
    fn render_summary_escapes_control_characters_in_package_and_matchspec_fields() {
        let rendered = render_summary(&report(
            true,
            vec![MatchspecInfo {
                matchspec: "evil\x1b]8;;https://example.com\x07pkg >=1".to_string(),
                source: "runtime".to_string(),
            }],
            vec![PackageInfo {
                name: "evil\x1bpkg".to_string(),
                version: "1.0.0".to_string(),
                channel: Some("https://example.com".to_string()),
            }],
        ));

        assert!(
            !rendered.contains('\x1b') || rendered.contains("\\u{1b}"),
            "raw ESC must never reach the terminal: {rendered:?}"
        );
        assert!(rendered.contains("\\u{1b}"), "escaped form: {rendered:?}");
        assert!(!rendered.contains('\x07'));
    }

    #[test]
    fn render_summary_reports_none_for_an_empty_matchspec_or_package_list() {
        let rendered = render_summary(&report(true, Vec::new(), Vec::new()));
        assert!(rendered.contains("matchspecs (linux-64):\n  (none)\n"));
        assert!(rendered.contains("packages (linux-64):\n  (none)\n"));
    }

    #[test]
    fn render_summary_pads_columns_to_the_widest_entry() {
        let matchspecs = vec![
            MatchspecInfo {
                matchspec: "numpy >=1.20".to_string(),
                source: "runtime".to_string(),
            },
            MatchspecInfo {
                matchspec: "python-duckdb".to_string(),
                source: "dev".to_string(),
            },
        ];
        let packages = vec![
            PackageInfo {
                name: "numpy".to_string(),
                version: "1.23.5".to_string(),
                channel: Some("https://repo.anaconda.com/pkgs/main/".to_string()),
            },
            PackageInfo {
                name: "python-duckdb".to_string(),
                version: "1.0.0".to_string(),
                channel: None,
            },
        ];
        let rendered = render_summary(&report(true, matchspecs.clone(), packages.clone()));

        // Same padding technique the code under test uses, applied here
        // only to build the *expected* strings -- this still exercises
        // `render_summary`'s real column-width computation, just without
        // hand-counting spaces (which would be brittle to get right).
        let matchspec_width = matchspecs
            .iter()
            .map(|m| m.matchspec.chars().count())
            .max()
            .unwrap();
        let name_width = packages
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap();
        let version_width = packages
            .iter()
            .map(|p| p.version.chars().count())
            .max()
            .unwrap();

        assert!(
            rendered.contains(&format!(
                "  {:<matchspec_width$}  (runtime)\n",
                "numpy >=1.20"
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("  {:<matchspec_width$}  (dev)\n", "python-duckdb")),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "  {:<name_width$}  {:<version_width$}  https://repo.anaconda.com/pkgs/main/\n",
                "numpy", "1.23.5"
            )),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "  {:<name_width$}  {:<version_width$}  (unknown channel)\n",
                "python-duckdb", "1.0.0"
            )),
            "{rendered}"
        );
    }

    #[test]
    fn render_json_produces_valid_json_with_every_field() {
        let value = report(
            false,
            vec![MatchspecInfo {
                matchspec: "numpy >=1.20".to_string(),
                source: "runtime".to_string(),
            }],
            vec![PackageInfo {
                name: "numpy".to_string(),
                version: "1.23.5".to_string(),
                channel: Some("https://repo.anaconda.com/pkgs/main/".to_string()),
            }],
        );

        let rendered = render(&value, Format::Json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(parsed["platform"], "linux-64");
        assert_eq!(parsed["project_file"]["kind"], "pyproject.toml");
        assert_eq!(parsed["project_file"]["path"], "/project/pyproject.toml");
        assert_eq!(parsed["in_sync"], false);
        assert_eq!(parsed["matchspecs"][0]["matchspec"], "numpy >=1.20");
        assert_eq!(parsed["matchspecs"][0]["source"], "runtime");
        assert_eq!(parsed["packages"][0]["name"], "numpy");
        assert_eq!(parsed["packages"][0]["version"], "1.23.5");
        assert_eq!(
            parsed["packages"][0]["channel"],
            "https://repo.anaconda.com/pkgs/main/"
        );
        assert_eq!(parsed["sandboxed"], false);
    }

    #[test]
    fn render_json_channel_is_null_when_undeterminable() {
        let value = report(
            true,
            Vec::new(),
            vec![PackageInfo {
                name: "numpy".to_string(),
                version: "1.23.5".to_string(),
                channel: None,
            }],
        );

        let rendered = render(&value, Format::Json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(parsed["packages"][0]["channel"], serde_json::Value::Null);
    }
}
