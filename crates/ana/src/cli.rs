//! Command-line parsing, via clap's derive API.
//!
//! `run`'s three trailing positionals -- `<primary>`, `<program>`, and
//! `ARGS...` -- follow the same "sequential optional positionals" shape
//! as `git diff`'s `<commit> <commit> <path>...`: `<primary>` is
//! required, `<program>` optional, and `ARGS...` (`last = true`) is only
//! ever populated after a literal `--`; omitting `--` means zero args.
//! Whether `<primary>` participates in the requirement set at all is
//! gated on `-g`/`--global` alone, never inferred from its shape --
//! [`resolve_run_invocation`] does that parsing, once flags have already
//! settled which mode applies. `<program>` is only meaningful under
//! `-g`; without it, `resolve_run_invocation` rejects a present
//! `<program>` rather than silently discarding it, since without `-g`
//! the only way for a second bare token to reach the target program's
//! own argument list is after a literal `--`.

use std::str::FromStr;

use ana_dependency::{Dependency, ParseSpecifierError};
use clap::{Parser, Subcommand};
use rattler_conda_types::Platform;
use uv_normalize::GroupName;

/// The top-level parser.
#[derive(Debug, Parser)]
#[command(
    name = "ana",
    about = "project-scoped conda environments for Python projects"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// A parsed invocation.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run a command inside the project environment
    Run {
        /// Also include a dependency group (repeatable) -- illegal with
        /// `-g`, since a CLI-declared environment has no group concept
        #[arg(long, value_name = "NAME", value_parser = parse_group, conflicts_with = "global")]
        group: Vec<GroupName>,

        /// Run in an ad hoc, CLI-declared environment instead of the
        /// project's: `<primary>` is parsed as a requirement and joins
        /// it, rather than being the literal program to run
        #[arg(short = 'g', long)]
        global: bool,

        /// Add an extra requirement (PEP 508, or a conda MatchSpec via
        /// `::`) on top of the targeted environment, repeatable
        #[arg(short = 'i', long = "include", value_name = "SPEC")]
        include: Vec<String>,

        /// Suppress ana's own output; only the command's stdout/stderr is
        /// ever printed, even if ana itself fails
        #[arg(short, long)]
        quiet: bool,

        /// Fail if ana.lock does not satisfy pyproject.toml's requirements,
        /// instead of updating the lock file
        #[arg(long)]
        frozen: bool,

        /// Use a pypi-to-conda name mapping cache older than a week
        /// (refreshing it in the background) instead of blocking for a
        /// fresh download -- useful when offline, or when the mapping
        /// endpoint is temporarily unreachable
        #[arg(long)]
        allow_stale_mapping: bool,

        /// Under `-g`, a requirement specifier joining the environment;
        /// otherwise the literal program to run
        #[arg(required = true, value_name = "PRIMARY")]
        primary: String,

        /// Under `-g`, the literal program to run, overriding what
        /// `PRIMARY` would derive; illegal without `-g` (add a literal
        /// `--` to pass it to `PRIMARY` as an argument instead)
        #[arg(value_name = "PROGRAM")]
        program: Option<String>,

        /// Arguments passed to the executed program, after a literal `--`
        #[arg(last = true, allow_hyphen_values = true, value_name = "ARGS")]
        args: Vec<String>,
    },

    /// Bring the project environment up to date, without running anything
    Sync {
        /// Also include a dependency group (repeatable)
        #[arg(long, value_name = "NAME", value_parser = parse_group)]
        group: Vec<GroupName>,

        /// Delete the environment before syncing, forcing a full reinstall
        #[arg(long)]
        clean: bool,

        /// Fail if ana.lock does not satisfy pyproject.toml's requirements,
        /// instead of updating the lock file
        #[arg(long)]
        frozen: bool,

        /// Use a pypi-to-conda name mapping cache older than a week
        /// (refreshing it in the background) instead of blocking for a
        /// fresh download -- useful when offline, or when the mapping
        /// endpoint is temporarily unreachable
        #[arg(long)]
        allow_stale_mapping: bool,

        /// Also solve (but do not install) an additional platform's
        /// section of ana.lock (repeatable) -- packages are only ever
        /// installed for the current platform
        #[arg(long, value_name = "SUBDIR", value_parser = parse_platform)]
        subdir: Vec<Platform>,
    },

    /// Remove every materialized environment, keeping the lock file(s)
    Clean {
        /// Remove every ad hoc (`ana run -g`) environment in the global
        /// cache instead of the current project's -- does not require a
        /// project file in the working directory
        #[arg(long)]
        global: bool,
    },

    /// Log in to Anaconda.org
    ///
    /// A fixed `ana run -g anaconda-auth anaconda -- login` invocation:
    /// materializes an ad hoc global environment containing
    /// `anaconda-auth`, then runs its `anaconda auth login` inside it.
    /// Interactive -- the environment is reused (not re-solved) by every
    /// later `ana login`, the same as any other `ana run -g` target.
    Login {
        /// Suppress ana's own output; only `anaconda auth login`'s own
        /// stdout/stderr is ever printed, even if ana itself fails
        #[arg(short, long)]
        quiet: bool,

        /// Use a pypi-to-conda name mapping cache older than a week
        /// (refreshing it in the background) instead of blocking for a
        /// fresh download -- useful when offline, or when the mapping
        /// endpoint is temporarily unreachable
        #[arg(long)]
        allow_stale_mapping: bool,

        /// Extra arguments passed to `anaconda auth login`, after a
        /// literal `--`
        #[arg(last = true, allow_hyphen_values = true, value_name = "ARGS")]
        args: Vec<String>,
    },

    /// Inspect or edit ana's config.toml
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

/// A parsed `ana config` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConfigAction {
    /// Print the effective value of every field, or just one
    Get {
        #[arg(value_parser = parse_config_key)]
        key: Option<ana_config::Key>,
    },
    /// Write one field to config.toml
    #[cfg_attr(feature = "commercial-config", command(hide = true))]
    Set {
        #[arg(value_parser = parse_config_key)]
        key: ana_config::Key,
        /// One or more values for a channel list; exactly one for
        /// pypi_to_conda_uri. `set` always requires at least one value --
        /// there is no way to clear a key back to unset yet (a future
        /// `ana config delete`/`--delete` would cover that).
        #[arg(required = true, trailing_var_arg = true, num_args = 1..)]
        values: Vec<String>,
    },
}

fn parse_config_key(value: &str) -> Result<ana_config::Key, ana_config::ParseKeyError> {
    value.parse()
}

/// Parse the process arguments (already stripped of `argv[0]`).
///
/// `Err` covers both real parse failures and `--help`: clap renders both
/// as an error value whose `exit()` prints the right text to the right
/// stream and exits with the right code.
pub fn parse(args: &[String]) -> Result<Command, clap::Error> {
    let argv = std::iter::once("ana".to_string()).chain(args.iter().cloned());
    Cli::try_parse_from(argv).map(|cli| cli.command)
}

/// What `ana run`'s CLI-declared inputs resolve to, independent of any
/// project: which requirements to add, and the command to actually exec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInvocation {
    /// Every CLI-declared dependency to add to the targeted environment:
    /// under `-g`, `<primary>` (first) plus every `-i`; otherwise, just
    /// every `-i`.
    pub cli_deps: Vec<Dependency>,
    /// The program and arguments to exec, verbatim.
    pub exec_command: Vec<String>,
}

/// [`resolve_run_invocation`]'s failure modes -- neither needs a mapping
/// table or network, so both fail fast before `Engine::build`.
#[derive(Debug, thiserror::Error)]
pub enum ResolveRunError {
    /// `<primary>` (under `-g`) or an `-i` value isn't a valid PEP 508
    /// requirement or conda MatchSpec.
    #[error("could not parse `{spec}` as a requirement: {source}")]
    Specifier {
        spec: String,
        #[source]
        source: ParseSpecifierError,
    },
    /// A second plain positional (`<program>`) was given without `-g`.
    /// Without `-g`, `<primary>` is already the program to run, so a
    /// second bare token can only be meant for that program's own
    /// argument list -- and reaching it requires a literal `--`, the
    /// same as any other argument.
    #[error(
        "`{program}` must come after `--` to be passed to `{primary}` (e.g. `ana run {primary} -- {program}`); a second positional is only used to name the program to run under `-g`"
    )]
    UnexpectedProgram { primary: String, program: String },
    /// `-g` with no `<program>`, and `<primary>` has no bare package name
    /// to derive one from. Unreachable via any `<primary>` string today
    /// -- [`ana_dependency::parse_specifier`]'s fixed `MatchSpec` parse
    /// options (`exact_names_only: true`) reject a glob/regex package
    /// name as a parse error before [`ana_dependency::bare_name`] could
    /// ever see it -- but that's an invariant of a parse option set
    /// elsewhere, not of this function's own types, so a real error
    /// stays here rather than a silent fallback that would otherwise
    /// treat `primary`'s raw spec text as if it were an executable name.
    #[error(
        "`-g {primary}` needs a <program> to run: `{primary}` has no bare package name to derive one from"
    )]
    NoProgram { primary: String },
}

/// Resolves `ana run`'s CLI-declared inputs to a [`RunInvocation`], once
/// clap has already settled which mode applies. Needs no mapping table
/// or network -- runs and fails fast before `Engine::build`.
///
/// Under `-g`, `primary` joins the requirement set (parsed via
/// [`ana_dependency::parse_specifier`]) and the exec program defaults to
/// its bare package name (via [`ana_dependency::bare_name`]) -- a
/// pypi-to-conda rename never reaches this function, so it can never
/// change what gets exec'd. A name-less `primary` (see
/// [`ResolveRunError::NoProgram`]'s docs for why that's unreachable
/// today) fails clearly rather than exec'ing `primary`'s raw spec text.
///
/// Without `-g`, `primary` is never parsed: it is the literal program to
/// run. A `program` positional in that case is rejected with
/// [`ResolveRunError::UnexpectedProgram`] rather than silently discarded
/// -- without `-g`, the only way for a second bare token to reach the
/// program's own argument list is after a literal `--`.
pub fn resolve_run_invocation(
    global: bool,
    primary: String,
    program: Option<String>,
    include: Vec<String>,
    args: Vec<String>,
) -> Result<RunInvocation, ResolveRunError> {
    let mut cli_deps = Vec::with_capacity(include.len() + usize::from(global));

    let program = if global {
        let primary_dep = ana_dependency::parse_specifier(&primary).map_err(|source| {
            ResolveRunError::Specifier {
                spec: primary.clone(),
                source,
            }
        })?;
        let program = program
            .or_else(|| ana_dependency::bare_name(&primary_dep))
            .ok_or_else(|| ResolveRunError::NoProgram {
                primary: primary.clone(),
            })?;
        cli_deps.push(primary_dep);
        program
    } else if let Some(program) = program {
        return Err(ResolveRunError::UnexpectedProgram { primary, program });
    } else {
        primary
    };

    for spec in include {
        match ana_dependency::parse_specifier(&spec) {
            Ok(dependency) => cli_deps.push(dependency),
            Err(source) => return Err(ResolveRunError::Specifier { spec, source }),
        }
    }

    let mut exec_command = Vec::with_capacity(1 + args.len());
    exec_command.push(program);
    exec_command.extend(args);

    Ok(RunInvocation {
        cli_deps,
        exec_command,
    })
}

/// Validate and normalize a `--group` value (PEP 735: lowercase, runs of
/// `-`/`_`/`.` collapsed to a single `-`).
fn parse_group(value: &str) -> Result<GroupName, uv_normalize::InvalidNameError> {
    GroupName::from_str(value)
}

/// Validate a `--subdir` value against rattler's known platform/subdir
/// strings (e.g. `linux-64`, `osx-arm64`, `win-64`).
fn parse_platform(value: &str) -> Result<Platform, rattler_conda_types::ParsePlatformError> {
    Platform::from_str(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use clap::error::ErrorKind;

    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn run_with_plain_program_no_args() {
        assert_eq!(
            parse(&args(&["run", "pytest"])).unwrap(),
            Command::Run {
                group: vec![],
                global: false,
                include: vec![],
                quiet: false,
                frozen: false,
                allow_stale_mapping: false,
                primary: "pytest".to_string(),
                program: None,
                args: vec![],
            }
        );
    }

    #[test]
    fn run_collects_groups_both_spellings() {
        let Command::Run { group, primary, .. } =
            parse(&args(&["run", "--group", "dev", "--group=doc", "pytest"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        let names: Vec<&str> = group.iter().map(|name| name.as_str()).collect();
        assert_eq!(names, vec!["dev", "doc"]);
        assert_eq!(primary, "pytest");
    }

    #[test]
    fn group_names_are_normalized() {
        let Command::Run { group, .. } =
            parse(&args(&["run", "--group", "Dev_Docs", "pytest"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        assert_eq!(group[0].as_str(), "dev-docs");
    }

    #[test]
    fn double_dash_populates_args() {
        assert_eq!(
            parse(&args(&["run", "pytest", "--", "-k", "foo"])).unwrap(),
            Command::Run {
                group: vec![],
                global: false,
                include: vec![],
                quiet: false,
                frozen: false,
                allow_stale_mapping: false,
                primary: "pytest".to_string(),
                program: None,
                args: args(&["-k", "foo"]),
            }
        );
    }

    #[test]
    fn no_double_dash_means_zero_args() {
        let Command::Run { args, .. } = parse(&args(&["run", "pytest"])).unwrap() else {
            panic!("expected Command::Run");
        };
        assert_eq!(args, Vec::<String>::new());
    }

    #[test]
    fn a_second_plain_positional_is_accepted_as_program() {
        // Structurally legal at the clap-parsing level in either mode --
        // whether it's *used* (under `-g`) or rejected (see
        // `resolve_run_invocation`'s docs) is a semantic question this
        // layer doesn't decide.
        let Command::Run {
            primary, program, ..
        } = parse(&args(&["run", "pytest", "subcmd"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        assert_eq!(primary, "pytest");
        assert_eq!(program, Some("subcmd".to_string()));
    }

    #[test]
    fn a_hyphen_prefixed_token_requires_double_dash_to_reach_the_program() {
        // The breaking change decision 1 calls out: without `--`, a
        // flag-shaped token is parsed as an (unknown) `ana` flag, not
        // silently handed to the target program.
        assert_eq!(
            parse(&args(&["run", "pytest", "-k", "foo"]))
                .unwrap_err()
                .kind(),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn run_global_short_and_long_flags() {
        let Command::Run { global, .. } = parse(&args(&["run", "-g", "::python==3.14"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        assert!(global);

        let Command::Run { global, .. } =
            parse(&args(&["run", "--global", "::python==3.14"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        assert!(global);
    }

    #[test]
    fn run_global_with_program_and_args() {
        assert_eq!(
            parse(&args(&[
                "run",
                "-g",
                "::python==3.14",
                "pip",
                "--",
                "freeze"
            ]))
            .unwrap(),
            Command::Run {
                group: vec![],
                global: true,
                include: vec![],
                quiet: false,
                frozen: false,
                allow_stale_mapping: false,
                primary: "::python==3.14".to_string(),
                program: Some("pip".to_string()),
                args: args(&["freeze"]),
            }
        );
    }

    #[test]
    fn run_include_is_repeatable_both_spellings() {
        let Command::Run { include, .. } = parse(&args(&[
            "run",
            "-i",
            "black",
            "--include",
            "::ruff",
            "pytest",
        ]))
        .unwrap() else {
            panic!("expected Command::Run");
        };
        assert_eq!(include, vec!["black".to_string(), "::ruff".to_string()]);
    }

    #[test]
    fn run_global_conflicts_with_group() {
        assert_eq!(
            parse(&args(&["run", "--group", "dev", "-g", "pytest"]))
                .unwrap_err()
                .kind(),
            ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn resolve_non_global_never_parses_primary() {
        // "true" is not a valid PEP 508 requirement's bare form in every
        // sense that would matter here, but under non-`-g` it must never
        // even be attempted -- `ana run true` must not try to install a
        // package named `true`.
        let invocation =
            resolve_run_invocation(false, "true".to_string(), None, vec![], vec![]).unwrap();
        assert_eq!(invocation.cli_deps, vec![]);
        assert_eq!(invocation.exec_command, vec!["true".to_string()]);
    }

    #[test]
    fn resolve_non_global_rejects_a_present_program_positional() {
        // A second plain positional without `-g` used to be silently
        // dropped (a real regression: `ana run pytest tests/` used to
        // exec bare `pytest`, discarding `tests/`). It must now be a
        // clear error instead, telling the user to use `--`.
        let result = resolve_run_invocation(
            false,
            "pytest".to_string(),
            Some("tests/".to_string()),
            vec![],
            vec![],
        );
        assert!(matches!(
            result,
            Err(ResolveRunError::UnexpectedProgram { .. })
        ));
    }

    #[test]
    fn resolve_non_global_rejects_a_present_program_positional_even_with_trailing_args() {
        let result = resolve_run_invocation(
            false,
            "pytest".to_string(),
            Some("subcmd".to_string()),
            vec![],
            vec!["-k".to_string(), "foo".to_string()],
        );
        assert!(matches!(
            result,
            Err(ResolveRunError::UnexpectedProgram { .. })
        ));
    }

    #[test]
    fn unexpected_program_error_names_both_primary_and_program() {
        let err = ResolveRunError::UnexpectedProgram {
            primary: "pytest".to_string(),
            program: "tests/".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("pytest"));
        assert!(text.contains("tests/"));
        assert!(text.contains("--"));
    }

    #[test]
    fn resolve_non_global_collects_include_but_not_primary() {
        let invocation = resolve_run_invocation(
            false,
            "pytest".to_string(),
            None,
            vec!["black".to_string()],
            vec![],
        )
        .unwrap();
        assert_eq!(invocation.cli_deps.len(), 1);
        assert_eq!(
            ana_dependency::bare_name(&invocation.cli_deps[0]),
            Some("black".to_string())
        );
    }

    #[test]
    fn resolve_global_parses_primary_and_prepends_it() {
        let invocation = resolve_run_invocation(
            true,
            "::python==3.14".to_string(),
            None,
            vec!["fastapi[standard]".to_string()],
            vec!["dev".to_string()],
        )
        .unwrap();
        assert_eq!(invocation.cli_deps.len(), 2);
        assert_eq!(
            ana_dependency::bare_name(&invocation.cli_deps[0]),
            Some("python".to_string()),
            "primary comes first"
        );
        assert_eq!(
            ana_dependency::bare_name(&invocation.cli_deps[1]),
            Some("fastapi".to_string())
        );
    }

    #[test]
    fn resolve_global_defaults_the_program_to_primarys_bare_name() {
        let invocation = resolve_run_invocation(
            true,
            "fastapi[standard]".to_string(),
            None,
            vec![],
            vec!["dev".to_string()],
        )
        .unwrap();
        assert_eq!(
            invocation.exec_command,
            vec!["fastapi".to_string(), "dev".to_string()]
        );
    }

    #[test]
    fn resolve_global_explicit_program_overrides_the_bare_name() {
        let invocation = resolve_run_invocation(
            true,
            "::python==3.14".to_string(),
            Some("pip".to_string()),
            vec![],
            vec!["freeze".to_string()],
        )
        .unwrap();
        assert_eq!(
            invocation.exec_command,
            vec!["pip".to_string(), "freeze".to_string()]
        );
    }

    #[test]
    fn resolve_global_program_defaults_to_the_bare_specifier_name_never_a_mapped_name() {
        // `resolve_run_invocation` takes no pypi-to-conda mapping table
        // at all -- a rename applied only at matchspec-conversion/solve
        // time downstream (e.g. `torch` -> `pytorch`, in
        // `ana-environment`/`ana-matchspec-convert`) can never reach
        // here, so `ana run -g torch` always execs `torch`, never
        // whatever conda package name the solve ends up installing.
        let invocation =
            resolve_run_invocation(true, "torch".to_string(), None, vec![], vec![]).unwrap();
        assert_eq!(invocation.exec_command, vec!["torch".to_string()]);
    }

    #[test]
    fn no_program_error_names_the_primary() {
        // Unreachable via `resolve_run_invocation` with any real
        // `primary` string today (see `ResolveRunError::NoProgram`'s and
        // `ana_dependency::bare_name`'s docs, and
        // `ana_dependency::tests::bare_name_is_none_for_a_matchspec_with_a_non_exact_name_matcher`
        // for why the `None` case it guards against is nonetheless a
        // real, reachable state of the underlying `Dependency` type) --
        // exercised directly here rather than through a `primary` string
        // that can't reach it.
        let err = ResolveRunError::NoProgram {
            primary: "::something".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("::something"));
        assert!(text.contains("<program>"));
    }

    #[test]
    fn resolve_global_rejects_an_invalid_primary_specifier() {
        let result = resolve_run_invocation(
            true,
            "!!!not a requirement!!!".to_string(),
            None,
            vec![],
            vec![],
        );
        assert!(matches!(result, Err(ResolveRunError::Specifier { .. })));
    }

    #[test]
    fn resolve_rejects_an_invalid_include_specifier() {
        let result = resolve_run_invocation(
            false,
            "pytest".to_string(),
            None,
            vec!["!!!not a requirement!!!".to_string()],
            vec![],
        );
        assert!(matches!(result, Err(ResolveRunError::Specifier { .. })));
    }

    #[test]
    fn end_to_end_global_run_with_include_and_derived_program() {
        // `ana run -g -i ::python==3.14 'fastapi[standard]' -- dev`:
        // installs both, execs `fastapi dev`.
        let Command::Run {
            global,
            include,
            primary,
            program,
            args,
            ..
        } = parse(&args(&[
            "run",
            "-g",
            "-i",
            "::python==3.14",
            "fastapi[standard]",
            "--",
            "dev",
        ]))
        .unwrap()
        else {
            panic!("expected Command::Run");
        };

        let invocation = resolve_run_invocation(global, primary, program, include, args).unwrap();
        assert_eq!(invocation.cli_deps.len(), 2);
        assert_eq!(invocation.exec_command, vec!["fastapi", "dev"]);
    }

    #[test]
    fn end_to_end_global_run_with_explicit_program() {
        // `ana run -g ::python==3.14 pip -- freeze`: execs `pip freeze`.
        let Command::Run {
            global,
            include,
            primary,
            program,
            args,
            ..
        } = parse(&args(&[
            "run",
            "-g",
            "::python==3.14",
            "pip",
            "--",
            "freeze",
        ]))
        .unwrap()
        else {
            panic!("expected Command::Run");
        };

        let invocation = resolve_run_invocation(global, primary, program, include, args).unwrap();
        assert_eq!(invocation.cli_deps.len(), 1);
        assert_eq!(invocation.exec_command, vec!["pip", "freeze"]);
    }

    #[test]
    fn end_to_end_group_and_include_without_global() {
        // `ana run --group dev -i black pytest -- -k foo`: runtime + dev
        // + black, execs `pytest -k foo`.
        let Command::Run {
            global,
            include,
            primary,
            program,
            args,
            ..
        } = parse(&args(&[
            "run", "--group", "dev", "-i", "black", "pytest", "--", "-k", "foo",
        ]))
        .unwrap()
        else {
            panic!("expected Command::Run");
        };

        let invocation = resolve_run_invocation(global, primary, program, include, args).unwrap();
        assert_eq!(invocation.cli_deps.len(), 1);
        assert_eq!(invocation.exec_command, vec!["pytest", "-k", "foo"]);
    }

    #[test]
    fn end_to_end_plain_run_behaves_like_the_legacy_grammar_once_double_dashed() {
        // `ana run pytest -- -k foo` is the new grammar's spelling of
        // legacy's `ana run pytest -k foo`.
        let Command::Run {
            global,
            include,
            primary,
            program,
            args,
            ..
        } = parse(&args(&["run", "pytest", "--", "-k", "foo"])).unwrap()
        else {
            panic!("expected Command::Run");
        };

        let invocation = resolve_run_invocation(global, primary, program, include, args).unwrap();
        assert_eq!(invocation.cli_deps, vec![]);
        assert_eq!(invocation.exec_command, vec!["pytest", "-k", "foo"]);
    }

    #[test]
    fn run_quiet_short_and_long() {
        let Command::Run { quiet, .. } = parse(&args(&["run", "-q", "true"])).unwrap() else {
            panic!("expected Command::Run");
        };
        assert!(quiet);

        let Command::Run { quiet, .. } = parse(&args(&["run", "--quiet", "true"])).unwrap() else {
            panic!("expected Command::Run");
        };
        assert!(quiet);
    }

    #[test]
    fn run_frozen_flag() {
        let Command::Run { frozen, .. } = parse(&args(&["run", "--frozen", "true"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        assert!(frozen);
    }

    #[test]
    fn run_allow_stale_mapping_flag() {
        let Command::Run {
            allow_stale_mapping,
            ..
        } = parse(&args(&["run", "--allow-stale-mapping", "true"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        assert!(allow_stale_mapping);
    }

    #[test]
    fn help_is_rendered_at_both_levels() {
        let err = parse(&args(&["--help"])).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
        assert!(err.to_string().contains("run"));

        let err = parse(&args(&["run", "--help"])).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
        let text = err.to_string();
        assert!(text.contains("--group"));
        assert!(text.contains("--global"));
        assert!(text.contains("--include"));
        assert!(text.contains("--quiet"));
        assert!(text.contains("--frozen"));
        assert!(text.contains("--allow-stale-mapping"));
        assert!(text.contains("PRIMARY"));
        assert!(text.contains("PROGRAM"));
        assert!(text.contains("ARGS"));
    }

    #[test]
    fn errors() {
        assert_eq!(
            parse(&args(&["frobnicate"])).unwrap_err().kind(),
            ErrorKind::InvalidSubcommand
        );
        assert_eq!(
            parse(&args(&["run"])).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
        // `--group` with no value, and with an invalid one.
        assert!(parse(&args(&["run", "--group"])).is_err());
        assert_eq!(
            parse(&args(&["run", "--group", "bad name!", "x"]))
                .unwrap_err()
                .kind(),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn sync_defaults() {
        assert_eq!(
            parse(&args(&["sync"])).unwrap(),
            Command::Sync {
                group: vec![],
                clean: false,
                frozen: false,
                allow_stale_mapping: false,
                subdir: vec![],
            }
        );
    }

    #[test]
    fn sync_collects_groups_clean_and_subdirs() {
        let Command::Sync {
            group,
            clean,
            subdir,
            ..
        } = parse(&args(&[
            "sync",
            "--group",
            "dev",
            "--clean",
            "--subdir",
            "osx-arm64",
            "--subdir",
            "win-64",
        ]))
        .unwrap()
        else {
            panic!("expected Command::Sync");
        };
        let names: Vec<&str> = group.iter().map(|name| name.as_str()).collect();
        assert_eq!(names, vec!["dev"]);
        assert!(clean);
        assert_eq!(subdir, vec![Platform::OsxArm64, Platform::Win64]);
    }

    #[test]
    fn sync_frozen_flag() {
        let Command::Sync { frozen, .. } = parse(&args(&["sync", "--frozen"])).unwrap() else {
            panic!("expected Command::Sync");
        };
        assert!(frozen);
    }

    #[test]
    fn sync_allow_stale_mapping_flag() {
        let Command::Sync {
            allow_stale_mapping,
            ..
        } = parse(&args(&["sync", "--allow-stale-mapping"])).unwrap()
        else {
            panic!("expected Command::Sync");
        };
        assert!(allow_stale_mapping);
    }

    #[test]
    fn sync_subdir_rejects_an_unknown_platform() {
        assert_eq!(
            parse(&args(&["sync", "--subdir", "not-a-real-subdir"]))
                .unwrap_err()
                .kind(),
            ErrorKind::ValueValidation
        );
    }

    #[test]
    fn sync_takes_no_positional_command() {
        assert_eq!(
            parse(&args(&["sync", "pytest"])).unwrap_err().kind(),
            ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn clean_takes_no_arguments() {
        assert_eq!(
            parse(&args(&["clean"])).unwrap(),
            Command::Clean { global: false }
        );
    }

    #[test]
    fn clean_rejects_extra_arguments() {
        assert!(parse(&args(&["clean", "extra"])).is_err());
    }

    #[test]
    fn clean_global_flag() {
        assert_eq!(
            parse(&args(&["clean", "--global"])).unwrap(),
            Command::Clean { global: true }
        );
    }

    #[test]
    fn login_defaults() {
        assert_eq!(
            parse(&args(&["login"])).unwrap(),
            Command::Login {
                quiet: false,
                allow_stale_mapping: false,
                args: vec![],
            }
        );
    }

    #[test]
    fn login_quiet_short_and_long() {
        let Command::Login { quiet, .. } = parse(&args(&["login", "-q"])).unwrap() else {
            panic!("expected Command::Login");
        };
        assert!(quiet);

        let Command::Login { quiet, .. } = parse(&args(&["login", "--quiet"])).unwrap() else {
            panic!("expected Command::Login");
        };
        assert!(quiet);
    }

    #[test]
    fn login_allow_stale_mapping_flag() {
        let Command::Login {
            allow_stale_mapping,
            ..
        } = parse(&args(&["login", "--allow-stale-mapping"])).unwrap()
        else {
            panic!("expected Command::Login");
        };
        assert!(allow_stale_mapping);
    }

    #[test]
    fn login_args_after_double_dash() {
        let Command::Login {
            args: login_args, ..
        } = parse(&args(&["login", "--", "--key", "abc"])).unwrap()
        else {
            panic!("expected Command::Login");
        };
        assert_eq!(login_args, vec!["--key".to_string(), "abc".to_string()]);
    }

    #[test]
    fn login_takes_no_bare_positional() {
        // Unlike `run`, `login` has nothing to accept as a plain
        // positional -- a hyphen-free bare token must go through `--`
        // just like a hyphen-prefixed one would.
        assert!(parse(&args(&["login", "extra"])).is_err());
    }

    #[test]
    fn config_get_with_no_key() {
        assert_eq!(
            parse(&args(&["config", "get"])).unwrap(),
            Command::Config {
                action: ConfigAction::Get { key: None },
            }
        );
    }

    #[test]
    fn config_get_default_channels() {
        assert_eq!(
            parse(&args(&["config", "get", "default_channels"])).unwrap(),
            Command::Config {
                action: ConfigAction::Get {
                    key: Some(ana_config::Key::DefaultChannels),
                },
            }
        );
    }

    #[test]
    fn config_set_default_channels_with_multiple_values() {
        assert_eq!(
            parse(&args(&[
                "config",
                "set",
                "default_channels",
                "conda-forge",
                "bioconda",
            ]))
            .unwrap(),
            Command::Config {
                action: ConfigAction::Set {
                    key: ana_config::Key::DefaultChannels,
                    values: args(&["conda-forge", "bioconda"]),
                },
            }
        );
    }

    #[test]
    fn config_set_pypi_to_conda_uri() {
        assert_eq!(
            parse(&args(&["config", "set", "pypi_to_conda_uri", "https://x",])).unwrap(),
            Command::Config {
                action: ConfigAction::Set {
                    key: ana_config::Key::PypiToCondaUri,
                    values: args(&["https://x"]),
                },
            }
        );
    }

    #[test]
    fn config_set_rejects_zero_values() {
        assert!(parse(&args(&["config", "set", "default_channels"])).is_err());
    }

    #[test]
    fn config_get_rejects_an_unknown_key() {
        assert_eq!(
            parse(&args(&["config", "get", "not_a_real_key"]))
                .unwrap_err()
                .kind(),
            ErrorKind::ValueValidation
        );
    }

    #[cfg(feature = "commercial-config")]
    #[test]
    fn commercial_config_hides_set_from_help_but_still_parses_it() {
        let err = parse(&args(&["config", "--help"])).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::DisplayHelp);
        let text = err.to_string();
        assert!(
            !text
                .lines()
                .any(|line| line.trim_start().starts_with("set")),
            "`set` must not be listed as a subcommand in a commercial-config build's help: {text}"
        );

        // Parsing still succeeds; `config_set` refuses at runtime instead.
        assert_eq!(
            parse(&args(&["config", "set", "default_channels", "x"])).unwrap(),
            Command::Config {
                action: ConfigAction::Set {
                    key: ana_config::Key::DefaultChannels,
                    values: args(&["x"]),
                },
            }
        );
    }
}
