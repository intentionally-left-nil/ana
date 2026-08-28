//! Command-line parsing, via clap's derive API.
//!
//! The one sharp edge is `run`'s trailing command: `trailing_var_arg` +
//! `allow_hyphen_values` make clap hand everything after the first
//! positional (or `--`) to the command verbatim, flags included (`ana run
//! python -c 'print("hi")'` keeps `-c` as the command's own argument, not
//! ana's). Help, usage errors, and exit codes (0 for `--help`, 2 for
//! parse failures) are clap's standard behavior, surfaced through
//! [`parse`]'s `Err(clap::Error)` -- `main` just calls `err.exit()`.

use std::str::FromStr;

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
        /// Also include a dependency group (repeatable)
        #[arg(long, value_name = "NAME", value_parser = parse_group)]
        group: Vec<GroupName>,

        /// The command to run inside the project environment
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },

    /// Bring the project environment up to date, without running anything
    Sync {
        /// Also include a dependency group (repeatable)
        #[arg(long, value_name = "NAME", value_parser = parse_group)]
        group: Vec<GroupName>,

        /// Delete the environment before syncing, forcing a full reinstall
        #[arg(long)]
        clean: bool,

        /// Also solve (but do not install) an additional platform's
        /// section of ana.lock (repeatable) -- packages are only ever
        /// installed for the current platform
        #[arg(long, value_name = "SUBDIR", value_parser = parse_platform)]
        subdir: Vec<Platform>,
    },

    /// Remove every materialized environment, keeping the lock file(s)
    Clean,
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
    fn run_with_plain_command() {
        assert_eq!(
            parse(&args(&["run", "python", "-c", "print(\"hi\")"])).unwrap(),
            Command::Run {
                group: vec![],
                command: args(&["python", "-c", "print(\"hi\")"]),
            }
        );
    }

    #[test]
    fn run_collects_groups_both_spellings() {
        let Command::Run { group, command } =
            parse(&args(&["run", "--group", "dev", "--group=doc", "pytest"])).unwrap()
        else {
            panic!("expected Command::Run");
        };
        let names: Vec<&str> = group.iter().map(|name| name.as_str()).collect();
        assert_eq!(names, vec!["dev", "doc"]);
        assert_eq!(command, args(&["pytest"]));
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
    fn double_dash_ends_flag_parsing() {
        assert_eq!(
            parse(&args(&["run", "--", "--group", "not-a-flag"])).unwrap(),
            Command::Run {
                group: vec![],
                command: args(&["--group", "not-a-flag"]),
            }
        );
    }

    #[test]
    fn command_flags_are_verbatim() {
        // `-c` belongs to python, not ana -- and so does a later
        // `--group`-shaped argument.
        assert_eq!(
            parse(&args(&["run", "python", "-c", "--group", "x"])).unwrap(),
            Command::Run {
                group: vec![],
                command: args(&["python", "-c", "--group", "x"]),
            }
        );
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
        assert!(text.contains("COMMAND"));
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
        assert_eq!(parse(&args(&["clean"])).unwrap(), Command::Clean);
    }

    #[test]
    fn clean_rejects_extra_arguments() {
        assert!(parse(&args(&["clean", "extra"])).is_err());
    }
}
