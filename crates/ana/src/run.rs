//! The `ana run` flow: discover the environment's paths, bring its lock
//! up to date, report the command that would have been run.
//!
//! Environment creation/activation (`investigations/sync_algorithm.md`'s
//! steps 3-4) doesn't exist yet, so the command is printed, not executed.
//! The real solver behind [`Solver`] is `ana-solver`'s `RattlerSolver`
//! (wired in by `main.rs`); [`NoSolver`] stays here as a solver-free
//! stand-in for tests, turning "the lock actually needs regenerating"
//! into an explicit error instead of a silent wrong answer whenever a
//! test deliberately doesn't want a real, network-bound solve.

use std::path::Path;

use ana_lockfile::{ensure_current_platform, EnsureOutcome, Project, SolveRequest, Solver};
use ana_paths::discover_paths;
use rattler_conda_types::{PackageRecord, Platform};
use uv_normalize::GroupName;

use crate::Error;

/// What a successful `ana run` did.
#[derive(Debug)]
pub struct RunOutcome {
    /// What the lockfile check did, for the caller to report.
    pub ensure: EnsureOutcome,
    /// The command that would have been run inside the environment,
    /// verbatim.
    pub command: Vec<String>,
}

/// `ana run [--group <name>]... <command>...`, with `project_dir` as the
/// project root (the process's working directory, in the binary).
///
/// `env_storage.md`'s discovery procedure (via `ana-paths`), then
/// `ana-lockfile`'s default mode for the current platform. The command is
/// returned, not run.
///
/// There is deliberately no walk-up to find the root: `project_dir` must
/// be the directory containing `pyproject.toml` (see `env_storage.md`'s
/// amendment history).
pub fn run_command(
    project_dir: &Path,
    groups: &[GroupName],
    command: &[String],
    solver: &dyn Solver,
) -> Result<RunOutcome, Error> {
    if !project_dir.join("pyproject.toml").is_file() {
        return Err(Error::NoProjectRoot);
    }
    let paths = discover_paths(project_dir, groups);
    let project = Project::load(project_dir)?;
    let ensure = ensure_current_platform(&project, &paths, groups, Platform::current(), solver)?;
    Ok(RunOutcome {
        ensure,
        command: command.to_vec(),
    })
}

/// A solver-free [`Solver`] stand-in: any invocation that actually needs a
/// solve fails explicitly, rather than silently. `ana-solver`'s
/// `RattlerSolver` is the real implementation (wired in by `main.rs`);
/// this one exists for tests that want to assert "the solver was never
/// consulted" or exercise the offline stage-1/stage-2 paths without
/// pulling in network I/O. Fresh-lock invocations never reach it.
pub struct NoSolver;

impl Solver for NoSolver {
    fn solve(
        &self,
        _request: SolveRequest,
    ) -> Result<Vec<PackageRecord>, Box<dyn std::error::Error + Send + Sync>> {
        Err(Box::new(SolveNotImplemented))
    }
}

/// [`NoSolver`]'s error, named so a test using it reads as an intentional
/// "no solver was supplied" notice rather than a failure of the solve
/// itself.
#[derive(Debug, thiserror::Error)]
#[error(
    "regenerating the lock requires a solver, and no solver is wired into ana yet \
     (this invocation used NoSolver -- see ana-solver for the real implementation)"
)]
struct SolveNotImplemented;

/// Render a command the way a user could paste it back into a shell:
/// arguments joined with spaces, any argument containing shell-significant
/// characters single-quoted. Display-only -- nothing here is executed.
pub fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote `arg` for [`shell_join`] if it isn't already a bare-safe word.
fn shell_quote(arg: &str) -> String {
    let bare = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._/:=@+%".contains(c));
    if bare {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;
    use std::str::FromStr;
    use std::sync::Mutex;

    use ana_lockfile::SolveRequest;
    use rattler_conda_types::{PackageName, Version};

    use super::*;

    const PYPROJECT: &str = r#"
[project]
name = "myproj"
dependencies = ["requests"]

[dependency-groups]
dev = ["ruff"]
"#;

    /// The same canned-record fake `ana-lockfile` tests with: one
    /// `name-1.0.0` record per exact-named spec.
    struct FakeSolver;

    impl Solver for FakeSolver {
        fn solve(
            &self,
            request: SolveRequest,
        ) -> Result<Vec<PackageRecord>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(request
                .specs
                .iter()
                .filter_map(|spec| spec.name.as_exact())
                .map(|name| {
                    let mut record = PackageRecord::new(
                        PackageName::new_unchecked(name.as_normalized()),
                        Version::from_str("1.0.0").unwrap(),
                        "py312h1234567_0".to_string(),
                    );
                    record.subdir = request.platform.as_str().to_string();
                    record
                })
                .collect())
        }
    }

    fn project_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pyproject.toml"), PYPROJECT).unwrap();
        dir
    }

    #[test]
    fn fresh_lock_reports_fresh_and_echoes_command() {
        let dir = project_root();
        let command = vec!["python".to_string(), "--version".to_string()];

        let first = run_command(dir.path(), &[], &command, &FakeSolver).unwrap();
        assert_eq!(first.ensure, EnsureOutcome::Resolved);
        assert_eq!(first.command, command);
        assert!(dir.path().join("ana.lock").exists());

        // Second run hits the stage-1 cache: no re-solve, nothing
        // rewritten.
        let second = run_command(dir.path(), &[], &command, &FakeSolver).unwrap();
        assert_eq!(second.ensure, EnsureOutcome::Fresh);
    }

    #[test]
    fn group_selection_uses_hashed_paths() {
        let dir = project_root();
        let groups = vec![GroupName::from_str("dev").unwrap()];
        let outcome = run_command(dir.path(), &groups, &["ruff".to_string()], &FakeSolver).unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Resolved);
        assert!(dir.path().join(".ana/ef260e9a/ana.lock").exists());
        // The default selection's paths are untouched.
        assert!(!dir.path().join("ana.lock").exists());
    }

    #[test]
    fn no_solver_errors_only_when_a_solve_is_needed() {
        let dir = project_root();
        let command = vec!["python".to_string()];
        // No lock yet: regeneration is required, so the missing solver
        // surfaces.
        let err = run_command(dir.path(), &[], &command, &NoSolver).unwrap_err();
        assert!(err.to_string().contains("no solver is wired into ana yet"));

        // With a fresh lock, NoSolver is never consulted.
        run_command(dir.path(), &[], &command, &FakeSolver).unwrap();
        let outcome = run_command(dir.path(), &[], &command, &NoSolver).unwrap();
        assert_eq!(outcome.ensure, EnsureOutcome::Fresh);
    }

    #[test]
    fn missing_project_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            run_command(dir.path(), &[], &["true".to_string()], &FakeSolver),
            Err(Error::NoProjectRoot)
        ));
    }

    #[test]
    fn unknown_group_is_an_error() {
        let dir = project_root();
        let groups = vec![GroupName::from_str("nope").unwrap()];
        assert!(matches!(
            run_command(dir.path(), &groups, &["true".to_string()], &FakeSolver),
            Err(Error::Lockfile(ana_lockfile::Error::UnknownGroup(name))) if name == "nope"
        ));
    }

    #[test]
    fn shell_join_quotes_only_when_needed() {
        assert_eq!(shell_join(&["python".to_string()]), "python");
        assert_eq!(
            shell_join(&[
                "python".to_string(),
                "-c".to_string(),
                "print(\"hi\")".to_string(),
            ]),
            "python -c 'print(\"hi\")'"
        );
        assert_eq!(
            shell_join(&["echo".to_string(), "it's".to_string()]),
            "echo 'it'\\''s'"
        );
        assert_eq!(shell_join(&[String::new()]), "''");
    }

    /// `run_command` must not consult the solver on a stage-1 hit even
    /// when the lock exists but the environment was never materialized
    /// (the scaffold's steady state).
    #[test]
    fn fresh_check_never_touches_solver() {
        struct CountingSolver(Mutex<u32>);
        impl Solver for CountingSolver {
            fn solve(
                &self,
                _request: SolveRequest,
            ) -> Result<Vec<PackageRecord>, Box<dyn std::error::Error + Send + Sync>> {
                *self.0.lock().unwrap() += 1;
                Ok(Vec::new())
            }
        }

        let dir = project_root();
        let solver = CountingSolver(Mutex::new(0));
        let command = vec!["true".to_string()];
        run_command(dir.path(), &[], &command, &solver).unwrap();
        run_command(dir.path(), &[], &command, &solver).unwrap();
        assert_eq!(*solver.0.lock().unwrap(), 1);
    }
}
