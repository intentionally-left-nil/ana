//! ana's nono sandbox integration: [`packages_require_sandbox`] decides
//! whether a solved environment must run under a sandbox (a package came
//! from a `sandboxed_channels` entry), and [`translate_policy`] /
//! [`nono_argv`] build the `nono run` invocation that enforces it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ana_channels::ChannelPolicy;
use rattler_conda_types::RepoDataRecord;
use serde_json::Value;

/// ana's built-in nono profile, used when `config.toml` sets
/// `sandboxed_channels` but not `sandbox_policy`. Adapted from
/// <https://github.com/intentionally-left-nil/nono_packs/blob/main/python/policy.json>.
pub const DEFAULT_POLICY: &str = include_str!("sandbox_policy.default.json");

/// The channel nono itself is installed from.
pub const NONO_CHANNEL: &str = "conda-forge";

/// nono's package name on [`NONO_CHANNEL`].
pub const NONO_PACKAGE: &str = "nono";

/// Every way building a sandbox invocation can fail.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `sandboxed_channels` didn't resolve to a valid channel list.
    #[error(transparent)]
    Channels(#[from] ana_channels::Error),

    /// A sandbox policy is not valid JSON.
    #[error("sandbox policy is not valid JSON: {0}")]
    InvalidPolicyJson(#[source] serde_json::Error),

    /// A sandbox policy is valid JSON but uses a shape
    /// [`translate_policy`] doesn't recognize -- an unknown key, a value
    /// of the wrong type, or an unsupported value for a fixed-set key
    /// (`extends`, `workdir.access`).
    #[error("sandbox policy is invalid: {0}")]
    InvalidPolicy(String),
}

/// Whether any of `packages` came from a channel in `sandboxed_channels`
/// -- if so, the environment they were solved into must run under a
/// sandbox.
///
/// A package's channel is derived from its own `url` -- the only field
/// anything ever fetches from -- via `ana_channels::artifact_channel`.
/// The `channel` field is solver-supplied free text, never trusted on
/// its own: a package whose `url` doesn't decompose into a channel is
/// treated as not sandboxed.
pub fn packages_require_sandbox(
    sandboxed_channels: &[String],
    packages: &[RepoDataRecord],
) -> Result<bool, Error> {
    if sandboxed_channels.is_empty() {
        return Ok(false);
    }
    let policy = ChannelPolicy::new(sandboxed_channels, &[])?;
    Ok(packages
        .iter()
        .any(|package| package_is_sandboxed(&policy, package)))
}

/// Whether `package` falls under `policy`.
fn package_is_sandboxed(policy: &ChannelPolicy, package: &RepoDataRecord) -> bool {
    ana_channels::artifact_channel(&package.url)
        .is_some_and(|channel| policy.authorizes_channel(&channel))
}

/// A sandbox policy translated into what `nono run` actually needs:
/// `args` (filesystem/workdir grant flags) and `env` (from
/// `environment.set_vars`), with every `$PREFIX`/`$WORKDIR` placeholder
/// already substituted.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TranslatedPolicy {
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Translates `template` (ana's [`DEFAULT_POLICY`], or a user's own
/// `sandbox_policy`) into `nono run` CLI arguments and environment
/// variables, substituting `$PREFIX`/`$WORKDIR` with real paths.
///
/// No profile file is ever written: it would have to live on disk inside
/// the very environment prefix an untrusted package was just installed
/// into. Unrecognized keys are a hard [`Error::InvalidPolicy`], not a
/// silent skip, so a policy can't understate what the sandbox restricts.
pub fn translate_policy(
    template: &str,
    env_prefix: &Path,
    workdir: &Path,
) -> Result<TranslatedPolicy, Error> {
    let value: Value = serde_json::from_str(template).map_err(Error::InvalidPolicyJson)?;
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidPolicy("must be a JSON object".to_string()))?;
    let prefix = env_prefix.to_string_lossy();
    let cwd = workdir.to_string_lossy();

    let mut translated = TranslatedPolicy::default();
    for (key, value) in object {
        match key.as_str() {
            "extends" => validate_extends(value)?,
            "meta" => validate_meta(value)?,
            "workdir" => translated.args.extend(workdir_args(value, &cwd)?),
            "filesystem" => translated
                .args
                .extend(filesystem_args(value, &prefix, &cwd)?),
            "environment" => translated
                .env
                .extend(environment_vars(value, &prefix, &cwd)?),
            other => return Err(Error::InvalidPolicy(format!("unknown key `{other}`"))),
        }
    }
    Ok(translated)
}

/// The `nono` argv (excluding `nono` itself) that applies `policy_args`
/// (from [`translate_policy`]) and execs `command`. `--allow-cwd` is
/// passed unconditionally so nono never blocks on its interactive
/// CWD-sharing prompt.
pub fn nono_argv(policy_args: &[String], workdir: &Path, command: &[String]) -> Vec<String> {
    let mut argv = vec!["run".to_string()];
    argv.extend(policy_args.iter().cloned());
    argv.push("--workdir".to_string());
    argv.push(workdir.to_string_lossy().into_owned());
    argv.push("--allow-cwd".to_string());
    argv.push("--".to_string());
    argv.extend(command.iter().cloned());
    argv
}

/// An environment prefix's executable directories: `bin/` on Unix, the
/// prefix itself plus `Scripts/` on Windows.
pub fn env_bin_dirs(env_path: &Path) -> Vec<PathBuf> {
    if cfg!(windows) {
        vec![env_path.to_path_buf(), env_path.join("Scripts")]
    } else {
        vec![env_path.join("bin")]
    }
}

/// The only accepted `extends` value is `"default"` (nono's base preset,
/// merged into every run whether named or not); it is validated but
/// never translated into an argument.
fn validate_extends(value: &Value) -> Result<(), Error> {
    match value.as_str() {
        Some("default") => Ok(()),
        Some(other) => Err(Error::InvalidPolicy(format!(
            "`extends` must be \"default\" (got {other:?}); extending any other profile isn't supported without a profile file"
        ))),
        None => Err(Error::InvalidPolicy(
            "`extends` must be a string".to_string(),
        )),
    }
}

/// `meta` is informational: validated, never translated into an argument.
fn validate_meta(value: &Value) -> Result<(), Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidPolicy("`meta` must be an object".to_string()))?;
    for (key, value) in object {
        match key.as_str() {
            "name" | "description" if value.is_string() => {}
            "name" | "description" => {
                return Err(Error::InvalidPolicy(format!(
                    "`meta.{key}` must be a string"
                )))
            }
            other => return Err(Error::InvalidPolicy(format!("unknown key `meta.{other}`"))),
        }
    }
    Ok(())
}

/// `workdir.access`'s CLI equivalent; `"none"` needs no flag.
fn workdir_args(value: &Value, cwd: &str) -> Result<Vec<String>, Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidPolicy("`workdir` must be an object".to_string()))?;
    let mut args = Vec::new();
    for (key, value) in object {
        if key != "access" {
            return Err(Error::InvalidPolicy(format!("unknown key `workdir.{key}`")));
        }
        let flag = match value.as_str() {
            Some("none") => None,
            Some("read") => Some("--read"),
            Some("write") => Some("--write"),
            Some("readwrite") => Some("--allow"),
            Some(other) => {
                return Err(Error::InvalidPolicy(format!(
                    "`workdir.access` must be one of \"none\", \"read\", \"write\", \"readwrite\" (got {other:?})"
                )))
            }
            None => {
                return Err(Error::InvalidPolicy(
                    "`workdir.access` must be a string".to_string(),
                ))
            }
        };
        if let Some(flag) = flag {
            args.push(flag.to_string());
            args.push(cwd.to_string());
        }
    }
    Ok(args)
}

/// `filesystem.{read,allow,write,read_file,allow_file,write_file}`,
/// translated 1:1 into nono's directory (`--read`/`--allow`/`--write`)
/// and single-file (`--read-file`/`--allow-file`/`--write-file`) grant
/// flags.
fn filesystem_args(value: &Value, prefix: &str, cwd: &str) -> Result<Vec<String>, Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidPolicy("`filesystem` must be an object".to_string()))?;
    let mut args = Vec::new();
    for (key, value) in object {
        let flag = match key.as_str() {
            "read" => "--read",
            "allow" => "--allow",
            "write" => "--write",
            "read_file" => "--read-file",
            "allow_file" => "--allow-file",
            "write_file" => "--write-file",
            other => {
                return Err(Error::InvalidPolicy(format!(
                    "unknown key `filesystem.{other}`"
                )))
            }
        };
        let paths = value.as_array().ok_or_else(|| {
            Error::InvalidPolicy(format!("`filesystem.{key}` must be an array of strings"))
        })?;
        for path in paths {
            let path = path.as_str().ok_or_else(|| {
                Error::InvalidPolicy(format!("`filesystem.{key}` must be an array of strings"))
            })?;
            args.push(flag.to_string());
            args.push(expand(path, prefix, cwd));
        }
    }
    Ok(args)
}

/// `environment.set_vars`, as variables the caller sets on the `nono`
/// process itself: nono passes its own environment through to the
/// sandboxed child (this translator never emits `allow_vars`), so this
/// is equivalent to a profile file's `set_vars`.
fn environment_vars(
    value: &Value,
    prefix: &str,
    cwd: &str,
) -> Result<BTreeMap<String, String>, Error> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidPolicy("`environment` must be an object".to_string()))?;
    let mut env = BTreeMap::new();
    for (key, value) in object {
        if key != "set_vars" {
            return Err(Error::InvalidPolicy(format!(
                "unknown key `environment.{key}`"
            )));
        }
        let set_vars = value.as_object().ok_or_else(|| {
            Error::InvalidPolicy("`environment.set_vars` must be an object".to_string())
        })?;
        for (name, value) in set_vars {
            let value = value.as_str().ok_or_else(|| {
                Error::InvalidPolicy(format!("`environment.set_vars.{name}` must be a string"))
            })?;
            env.insert(name.clone(), expand(value, prefix, cwd));
        }
    }
    Ok(env)
}

/// Replaces every `$PREFIX`/`$WORKDIR` occurrence in `raw` with
/// `prefix`/`cwd`.
fn expand(raw: &str, prefix: &str, cwd: &str) -> String {
    raw.replace("$PREFIX", prefix).replace("$WORKDIR", cwd)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{NoArchType, PackageName, PackageRecord, Version};
    use std::str::FromStr;

    use super::*;

    /// A minimal, otherwise-arbitrary [`RepoDataRecord`], with `channel`
    /// and `url` set to whatever the test wants to exercise.
    fn record(channel: Option<&str>, url: &str) -> RepoDataRecord {
        let mut package_record = PackageRecord::new(
            PackageName::new_unchecked("some-package"),
            Version::from_str("1.0.0").unwrap(),
            "0".to_string(),
        );
        package_record.subdir = "noarch".to_string();
        package_record.noarch = NoArchType::generic();
        let filename = "some-package-1.0.0-0.conda";
        let identifier = DistArchiveIdentifier::try_from_filename(filename).unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url: url::Url::parse(url).unwrap(),
            channel: channel.map(ToString::to_string),
        }
    }

    #[test]
    fn default_policy_is_valid_json() {
        assert!(serde_json::from_str::<Value>(DEFAULT_POLICY).is_ok());
    }

    #[test]
    fn empty_sandboxed_channels_never_requires_a_sandbox() {
        let packages = [record(
            Some("https://conda.anaconda.org/conda-forge/"),
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(!packages_require_sandbox(&[], &packages).unwrap());
    }

    #[test]
    fn a_package_from_a_sandboxed_channel_requires_a_sandbox() {
        let sandboxed = vec!["conda-forge".to_string()];
        let packages = [record(
            Some("https://conda.anaconda.org/conda-forge/"),
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn a_package_from_an_unrelated_channel_never_requires_a_sandbox() {
        let sandboxed = vec!["bioconda".to_string()];
        let packages = [record(
            Some("https://conda.anaconda.org/conda-forge/"),
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(!packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn a_missing_channel_field_falls_back_to_the_package_url() {
        let sandboxed = vec!["conda-forge".to_string()];
        let packages = [record(
            None,
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn a_channel_field_that_is_not_a_url_falls_back_to_the_package_url() {
        let sandboxed = vec!["conda-forge".to_string()];
        let packages = [record(
            Some("not-a-url"),
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn a_channel_field_that_contradicts_the_url_is_not_trusted() {
        let sandboxed = vec!["https://repo.anaconda.com/pkgs/main".to_string()];
        let packages = [record(
            Some("https://conda.anaconda.org/conda-forge/"),
            "https://repo.anaconda.com/pkgs/main/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn a_url_without_a_channel_layout_is_never_sandboxed() {
        let sandboxed = vec!["conda-forge".to_string()];
        let packages = [record(
            None,
            "https://conda.anaconda.org/conda-forge/some-package-1.0.0-0.conda",
        )];
        assert!(!packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn a_meta_channel_name_expands_to_its_members() {
        let sandboxed = vec!["defaults".to_string()];
        let packages = [record(
            Some("https://repo.anaconda.com/pkgs/main/"),
            "https://repo.anaconda.com/pkgs/main/noarch/some-package-1.0.0-0.conda",
        )];
        assert!(packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn only_one_sandboxed_package_out_of_many_still_requires_a_sandbox() {
        let sandboxed = vec!["bioconda".to_string()];
        let packages = [
            record(
                Some("https://conda.anaconda.org/conda-forge/"),
                "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
            ),
            record(
                Some("https://conda.anaconda.org/bioconda/"),
                "https://conda.anaconda.org/bioconda/noarch/some-package-1.0.0-0.conda",
            ),
        ];
        assert!(packages_require_sandbox(&sandboxed, &packages).unwrap());
    }

    #[test]
    fn translate_policy_rejects_malformed_json() {
        assert!(matches!(
            translate_policy("not json", Path::new("/env/prefix"), Path::new("/project")),
            Err(Error::InvalidPolicyJson(_))
        ));
    }

    #[test]
    fn translate_policy_rejects_a_non_object_top_level() {
        assert!(matches!(
            translate_policy("[]", Path::new("/env/prefix"), Path::new("/project")),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn translate_policy_rejects_an_unknown_top_level_key() {
        let err = translate_policy(
            r#"{"nonsense": true}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidPolicy(message) if message.contains("nonsense")));
    }

    #[test]
    fn translate_policy_accepts_extends_default() {
        let translated = translate_policy(
            r#"{"extends": "default"}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert_eq!(translated, TranslatedPolicy::default());
    }

    #[test]
    fn translate_policy_rejects_an_extends_other_than_default() {
        assert!(matches!(
            translate_policy(
                r#"{"extends": "some-other-pack"}"#,
                Path::new("/env/prefix"),
                Path::new("/project"),
            ),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn translate_policy_rejects_an_unknown_meta_key() {
        assert!(matches!(
            translate_policy(
                r#"{"meta": {"nonsense": true}}"#,
                Path::new("/env/prefix"),
                Path::new("/project"),
            ),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn translate_policy_workdir_read_grants_the_workdir() {
        let translated = translate_policy(
            r#"{"workdir": {"access": "read"}}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert_eq!(translated.args, vec!["--read", "/project"]);
    }

    #[test]
    fn translate_policy_workdir_none_grants_nothing() {
        let translated = translate_policy(
            r#"{"workdir": {"access": "none"}}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert!(translated.args.is_empty());
    }

    #[test]
    fn translate_policy_rejects_an_unknown_workdir_access() {
        assert!(matches!(
            translate_policy(
                r#"{"workdir": {"access": "sudo"}}"#,
                Path::new("/env/prefix"),
                Path::new("/project"),
            ),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn translate_policy_filesystem_substitutes_both_placeholders() {
        let translated = translate_policy(
            r#"{"filesystem": {"read": ["$PREFIX", "$WORKDIR/tmp"]}}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert_eq!(
            translated.args,
            vec!["--read", "/env/prefix", "--read", "/project/tmp"]
        );
    }

    #[test]
    fn translate_policy_filesystem_read_file_maps_to_read_file_flag() {
        let translated = translate_policy(
            r#"{"filesystem": {"read_file": ["$HOME/.condarc"]}}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert_eq!(translated.args, vec!["--read-file", "$HOME/.condarc"]);
    }

    #[test]
    fn translate_policy_rejects_an_unknown_filesystem_key() {
        assert!(matches!(
            translate_policy(
                r#"{"filesystem": {"nonsense": []}}"#,
                Path::new("/env/prefix"),
                Path::new("/project"),
            ),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn translate_policy_rejects_a_non_string_filesystem_entry() {
        assert!(matches!(
            translate_policy(
                r#"{"filesystem": {"read": [1]}}"#,
                Path::new("/env/prefix"),
                Path::new("/project"),
            ),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn translate_policy_environment_set_vars_substitutes_both_placeholders() {
        let translated = translate_policy(
            r#"{"environment": {"set_vars": {"TMPDIR": "$WORKDIR/tmp", "HOME_COPY": "$PREFIX/home"}}}"#,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert_eq!(translated.env.get("TMPDIR").unwrap(), "/project/tmp");
        assert_eq!(translated.env.get("HOME_COPY").unwrap(), "/env/prefix/home");
    }

    #[test]
    fn translate_policy_rejects_an_unknown_environment_key() {
        assert!(matches!(
            translate_policy(
                r#"{"environment": {"allow_vars": ["FOO"]}}"#,
                Path::new("/env/prefix"),
                Path::new("/project"),
            ),
            Err(Error::InvalidPolicy(_))
        ));
    }

    #[test]
    fn default_policy_translates_cleanly() {
        let translated = translate_policy(
            DEFAULT_POLICY,
            Path::new("/env/prefix"),
            Path::new("/project"),
        )
        .unwrap();
        assert!(!translated.args.is_empty());
        assert!(!translated.env.is_empty());
        for dir in translated.env.values() {
            assert!(
                dir.starts_with("/env/prefix"),
                "{dir} must live under /env/prefix"
            );
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn env_bin_dirs_is_bin_on_unix() {
        assert_eq!(
            env_bin_dirs(Path::new("/env/prefix")),
            vec![PathBuf::from("/env/prefix/bin")]
        );
    }

    #[test]
    fn nono_argv_builds_the_expected_invocation() {
        let argv = nono_argv(
            &["--read".to_string(), "/env/prefix".to_string()],
            Path::new("/project"),
            &["python".to_string(), "script.py".to_string()],
        );
        assert_eq!(
            argv,
            vec![
                "run",
                "--read",
                "/env/prefix",
                "--workdir",
                "/project",
                "--allow-cwd",
                "--",
                "python",
                "script.py",
            ]
        );
    }
}
