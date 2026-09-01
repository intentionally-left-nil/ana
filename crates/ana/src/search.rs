//! `ana search`: read-only package discovery against the channels an
//! invocation would solve against.
//!
//! Answers "which of my channels carry this package, at what versions"
//! without a project, a lock file, or an environment. [`resolve_spec`]
//! turns the `<SPEC>` positional into a queryable [`MatchSpec`] -- PEP
//! 508 names go through the pypi-to-conda mapping and the mapping
//! decision is kept for display; [`resolve_channels`] settles which
//! channels to query (the configured defaults, `--channel`'s replacement
//! list, or the one channel a spec pins); [`search`] runs the
//! per-channel queries and collects a [`SearchReport`]; [`render`]
//! formats it.
//!
//! A search never fails as a whole: each channel's outcome is its own,
//! so "not on this channel", "channel isn't there", and "channel
//! couldn't be reached" stay distinguishable in the report.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use ana_channels::ChannelPolicy;
use ana_pypi_conda_map::MappingHandle;
use ana_solver::{ChannelQuery, ChannelQueryError};
use rattler_conda_types::{
    Channel, MatchSpec, PackageName, PackageNameMatcher, Platform, RepoDataRecord,
};
use uv_pep508::{Requirement, VersionOrUrl};

/// `ana search --format`'s values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SearchFormat {
    /// One section per channel, newest versions first (the default).
    Summary,
    /// The full result set as JSON, including every match's build,
    /// subdir, and direct dependencies.
    Json,
}

/// Which optional fields `Summary` output includes. `Json` ignores this
/// -- it always carries everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayOptions {
    /// Include each match's build string (`--builds`).
    pub builds: bool,
    /// Include each match's subdir (`--show-subdir`).
    pub subdir: bool,
    /// Show the newest match's direct dependencies per channel
    /// (`--deps`).
    pub deps: bool,
}

/// How a `<SPEC>`'s package name relates to the pypi-to-conda mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameMapping {
    /// A `::` matchspec -- the mapping was never consulted.
    NotConsulted,
    /// A PEP 508 name the mapping has no entry for -- searched as-is.
    Unmapped,
    /// A PEP 508 name with a mapping entry; carries the pypi name.
    Mapped(String),
}

/// A parsed `<SPEC>`, ready to query.
#[derive(Debug, Clone)]
pub struct SearchSpec {
    /// The spec as typed, for messages.
    pub input: String,
    /// The conda package name actually searched.
    pub conda_name: String,
    /// The mapping decision for the name.
    pub mapping: NameMapping,
    /// The query spec: an exact name plus an optional version
    /// constraint, never a channel pin (a pin moves to
    /// `pinned_channel`).
    pub spec: MatchSpec,
    /// The channel the spec pinned (`conda-forge::numpy`), normalized.
    /// When `Some`, it is the only channel searched.
    pub pinned_channel: Option<Channel>,
}

/// The full result of one `ana search` invocation.
#[derive(Debug)]
pub struct SearchReport {
    /// The `<SPEC>` as typed.
    pub input: String,
    /// The conda package name searched.
    pub conda_name: String,
    /// The mapping decision for the name.
    pub mapping: NameMapping,
    /// The subdirs searched (always includes `noarch`).
    pub platforms: Vec<Platform>,
    /// One entry per channel searched, in search order.
    pub channels: Vec<ChannelReport>,
}

impl SearchReport {
    /// Whether any channel returned at least one match.
    pub fn any_matches(&self) -> bool {
        self.channels
            .iter()
            .any(|channel| matches!(channel.status, ChannelStatus::Matches(_)))
    }

    /// Whether every channel failed to answer at all -- "not found"
    /// can't be concluded from a query that never completed.
    pub fn all_channels_failed(&self) -> bool {
        !self.channels.is_empty()
            && self.channels.iter().all(|channel| {
                matches!(
                    channel.status,
                    ChannelStatus::NoSubdir(_) | ChannelStatus::Failed(_)
                )
            })
    }

    /// Whether any channel failed to answer -- while one is
    /// unreachable, "not found" describes only the channels that
    /// answered.
    pub fn any_channel_failed(&self) -> bool {
        self.channels.iter().any(|channel| {
            matches!(
                channel.status,
                ChannelStatus::NoSubdir(_) | ChannelStatus::Failed(_)
            )
        })
    }
}

/// One searched channel's outcome.
#[derive(Debug)]
pub struct ChannelReport {
    /// The channel's canonical URL.
    pub url: String,
    /// What the channel answered.
    pub status: ChannelStatus,
}

/// What one channel answered, distinguished so "not here", "not
/// there", and "couldn't ask" never collapse into one message.
#[derive(Debug)]
pub enum ChannelStatus {
    /// Matching records, sorted newest-first (version, then build
    /// number). Never empty.
    Matches(Vec<Arc<RepoDataRecord>>),
    /// The channel answered; nothing matched.
    NoMatches,
    /// The channel serves none of the searched subdirs -- it isn't
    /// really there. Carries the missing subdir's name.
    NoSubdir(String),
    /// The channel couldn't be queried: network, auth, repodata parse.
    /// Carries the error message.
    Failed(String),
}

/// [`resolve_spec`]/[`resolve_channels`]'s failure modes.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// `<SPEC>` isn't a valid PEP 508 requirement or conda MatchSpec.
    #[error("could not parse `{spec}` as a package name or requirement: {source}")]
    Parse {
        spec: String,
        #[source]
        source: ana_dependency::ParseSpecifierError,
    },

    /// A PEP 508 requirement with extras. Extras only expand to
    /// additional dependencies at solve time; a repository query has no
    /// use for them.
    #[error("`{input}` names extras; search for the base package instead: `ana search {name}`")]
    Extras { input: String, name: String },

    /// A PEP 508 requirement with an environment marker. Markers are
    /// evaluated against an environment being solved, which a search
    /// doesn't have.
    #[error(
        "`{input}` has an environment marker; search doesn't evaluate markers -- \
         search for `{name}` instead"
    )]
    Marker { input: String, name: String },

    /// A `name @ url` requirement or a `::` matchspec with `url=`: the
    /// exact artifact is already known, so there is nothing to search
    /// for.
    #[error("`{input}` is a direct URL reference; search by package name instead")]
    DirectUrl { input: String },

    /// A `channel/subdir::name` matchspec. A subdir constraint selects
    /// which platform's repodata to search, but the query's matcher
    /// never consults it -- forwarding it would silently search the
    /// wrong platform, so it is rejected toward `--subdir` instead.
    #[error("`{input}` names a subdir; use `--subdir {subdir}` instead")]
    Subdir { input: String, subdir: String },

    /// A version constraint a conda matchspec can't express (a local
    /// `+label`, a pre-release without allow_pre).
    #[error(transparent)]
    Version(#[from] ana_pep508_to_matchspec::ConvertError),

    /// The mapping table's entry for this name is itself malformed.
    #[error(transparent)]
    Mapping(#[from] ana_pypi_conda_map::InvalidMappedName),

    /// The (possibly mapped) name isn't a valid conda package name.
    #[error("`{name}` is not a valid conda package name: {source}")]
    InvalidName {
        name: String,
        #[source]
        source: rattler_conda_types::InvalidPackageNameError,
    },

    /// Unreachable via `parse_specifier` (its parse options require an
    /// exact name) -- a real error rather than an unwrap, since that's
    /// an invariant of parse options set in another crate.
    #[error("`{input}` doesn't name exactly one package (globs aren't searchable)")]
    NonExactName { input: String },

    /// A spec pinned a channel and `--channel` was also passed -- the
    /// two would contradict each other.
    #[error("`{input}` pins a channel; `--channel` can't be combined with a channel pin")]
    ChannelPinConflict { input: String },

    /// A `--channel` entry or spec-pinned channel failed the policy
    /// check.
    #[error(transparent)]
    Channels(#[from] ana_channels::Error),
}

/// Parses `input` into a [`SearchSpec`]: a PEP 508 name/requirement, or
/// a conda MatchSpec via `::` -- the same grammar as `ana run -g`'s
/// requirement positionals.
///
/// PEP 508 names are mapped through `mapping`; a version constraint
/// (`numpy>=2`) becomes the query's version filter. Extras, environment
/// markers, and direct-URL requirements are rejected: they are
/// solve-time concepts with no meaning for a repository query.
pub fn resolve_spec(input: &str, mapping: &MappingHandle) -> Result<SearchSpec, SearchError> {
    match ana_dependency::parse_specifier(input) {
        Ok(ana_dependency::Dependency::Pep508(requirement)) => {
            resolve_pep508(input, &requirement, mapping)
        }
        Ok(ana_dependency::Dependency::Matchspec(spec)) => resolve_matchspec(input, *spec),
        Err(source) => Err(SearchError::Parse {
            spec: input.to_string(),
            source,
        }),
    }
}

/// Settles which channels a search queries:
///
/// * A channel pinned in the spec (`conda-forge::numpy`) is the only
///   channel searched, after an authorization check; combining a pin
///   with `--channel` is rejected as contradictory.
/// * A non-empty `--channel` list *replaces* the configured
///   `default_channels` for this invocation, each entry checked against
///   the policy -- the same replacement semantics a project's
///   `conda-channels` override gets.
/// * Otherwise the configured defaults are searched as-is.
///
/// The returned list is the union of every requested platform's
/// applicable channels, in first-occurrence order (a Windows-only
/// member like `msys2` is included whenever any requested platform is
/// Windows); a channel queried for a platform it doesn't serve simply
/// yields no records.
pub fn resolve_channels(
    policy: &ChannelPolicy,
    channel_args: &[String],
    spec: &SearchSpec,
    platforms: &[Platform],
) -> Result<Vec<Channel>, SearchError> {
    if let Some(pinned) = &spec.pinned_channel {
        if !channel_args.is_empty() {
            return Err(SearchError::ChannelPinConflict {
                input: spec.input.clone(),
            });
        }
        if !policy.authorizes_channel(&pinned.base_url) {
            return Err(ana_channels::Error::ChannelNotAllowed(format!(
                "  {:?} (pinned in {:?}): not in default_channels/allowed_channels",
                pinned.base_url.as_str(),
                spec.input,
            ))
            .into());
        }
        return Ok(vec![pinned.clone()]);
    }

    let set = if channel_args.is_empty() {
        policy.effective_channels(None, &[])?.set
    } else {
        policy.search_list(channel_args, "--channel")?
    };

    let mut seen = HashSet::new();
    let mut channels = Vec::new();
    for platform in platforms {
        for channel in set.for_platform(*platform) {
            if seen.insert(channel.base_url.clone()) {
                channels.push(channel);
            }
        }
    }
    Ok(channels)
}

/// Runs `spec` against every channel in `channels` (via `querier`) and
/// collects the per-channel outcomes into a [`SearchReport`], matches
/// sorted newest-first. Infallible: a failing channel is reported, not
/// propagated.
pub fn search(
    spec: &SearchSpec,
    channels: &[Channel],
    platforms: &[Platform],
    querier: &dyn ChannelQuery,
) -> SearchReport {
    // Every conda query needs noarch's records too, regardless of the
    // requested platforms -- same as a solve.
    let mut platforms = platforms.to_vec();
    if !platforms.contains(&Platform::NoArch) {
        platforms.push(Platform::NoArch);
    }

    let channels = querier
        .query_channels(channels, &platforms, &spec.spec)
        .into_iter()
        .map(|outcome| {
            let status = match outcome.result {
                Ok(records) if records.is_empty() => ChannelStatus::NoMatches,
                Ok(mut records) => {
                    records.sort_by(|a, b| {
                        b.package_record
                            .version
                            .cmp(&a.package_record.version)
                            .then(
                                b.package_record
                                    .build_number
                                    .cmp(&a.package_record.build_number),
                            )
                    });
                    ChannelStatus::Matches(records)
                }
                Err(ChannelQueryError::SubdirNotFound(subdir)) => ChannelStatus::NoSubdir(subdir),
                Err(ChannelQueryError::Fetch(message)) => ChannelStatus::Failed(message),
            };
            ChannelReport {
                url: outcome.channel.as_str().to_string(),
                status,
            }
        })
        .collect();

    SearchReport {
        input: spec.input.clone(),
        conda_name: spec.conda_name.clone(),
        mapping: spec.mapping.clone(),
        platforms,
        channels,
    }
}

/// Formats `report` for stdout. `display` applies to `Summary` only;
/// `Json` always carries every field.
pub fn render(
    report: &SearchReport,
    format: SearchFormat,
    display: DisplayOptions,
) -> Result<String, serde_json::Error> {
    match format {
        SearchFormat::Summary => Ok(render_summary(report, display)),
        SearchFormat::Json => render_json(report),
    }
}

fn resolve_pep508(
    input: &str,
    requirement: &Requirement,
    mapping: &MappingHandle,
) -> Result<SearchSpec, SearchError> {
    let name = requirement.name.as_str();
    if !requirement.extras.is_empty() {
        return Err(SearchError::Extras {
            input: input.to_string(),
            name: name.to_string(),
        });
    }
    if !requirement.marker.is_true() {
        return Err(SearchError::Marker {
            input: input.to_string(),
            name: name.to_string(),
        });
    }
    let version = match &requirement.version_or_url {
        None => None,
        Some(VersionOrUrl::Url(_)) => {
            return Err(SearchError::DirectUrl {
                input: input.to_string(),
            })
        }
        Some(VersionOrUrl::VersionSpecifier(specifiers)) => {
            // `allow_pre = false`, matching what a solve would accept.
            ana_pep508_to_matchspec::version_spec(specifiers, false)?
        }
    };

    let mapped = mapping.get(name)?;
    let mapping_decision = if mapped == name {
        NameMapping::Unmapped
    } else {
        NameMapping::Mapped(name.to_string())
    };
    let conda_name = PackageName::from_str(mapped).map_err(|source| SearchError::InvalidName {
        name: mapped.to_string(),
        source,
    })?;

    Ok(SearchSpec {
        input: input.to_string(),
        conda_name: conda_name.as_normalized().to_string(),
        mapping: mapping_decision,
        spec: MatchSpec {
            name: PackageNameMatcher::Exact(conda_name),
            version,
            ..MatchSpec::default()
        },
        pinned_channel: None,
    })
}

fn resolve_matchspec(input: &str, mut spec: MatchSpec) -> Result<SearchSpec, SearchError> {
    if spec.url.is_some() {
        return Err(SearchError::DirectUrl {
            input: input.to_string(),
        });
    }
    if let Some(subdir) = spec.subdir.take() {
        return Err(SearchError::Subdir {
            input: input.to_string(),
            subdir,
        });
    }
    let conda_name = spec
        .name
        .as_exact()
        .map(|name| name.as_normalized().to_string())
        .ok_or_else(|| SearchError::NonExactName {
            input: input.to_string(),
        })?;
    // A channel pin selects the one channel to search; the query itself
    // runs unpinned, since only that channel is queried anyway.
    let pinned_channel = spec.channel.take().map(Arc::unwrap_or_clone);
    Ok(SearchSpec {
        input: input.to_string(),
        conda_name,
        mapping: NameMapping::NotConsulted,
        spec,
        pinned_channel,
    })
}

fn render_summary(report: &SearchReport, display: DisplayOptions) -> String {
    let mut out = String::new();
    for (index, channel) in report.channels.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        match &channel.status {
            ChannelStatus::Matches(records) => {
                let [newest, ..] = records.as_slice() else {
                    // `Matches` is never empty by construction.
                    continue;
                };
                let rows = summary_rows(records, display);
                let noun = if display.builds || display.subdir {
                    "builds"
                } else {
                    "versions"
                };
                let latest = newest.package_record.version.to_string();
                out.push_str(&format!(
                    "{}: {} {noun}, latest {latest}\n",
                    channel.url,
                    rows.len(),
                ));
                for row in &rows {
                    out.push_str(&format!("  {row}\n"));
                }
                if display.deps && !newest.package_record.depends.is_empty() {
                    out.push_str(&format!("\n  latest ({latest}) depends on:\n"));
                    for dep in &newest.package_record.depends {
                        out.push_str(&format!("    {dep}\n"));
                    }
                }
            }
            ChannelStatus::NoMatches => {
                out.push_str(&format!("{}: no matches\n", channel.url));
            }
            ChannelStatus::NoSubdir(subdir) => {
                out.push_str(&format!("{}: no '{subdir}' subdir\n", channel.url));
            }
            ChannelStatus::Failed(message) => {
                out.push_str(&format!("{}: error: {message}\n", channel.url));
            }
        }
    }
    out
}

/// One row per distinct combination of the displayed fields, in
/// `records`'s (newest-first) order: with neither `--builds` nor
/// `--show-subdir` that's one row per distinct version -- the "at what
/// versions" answer without per-build noise.
fn summary_rows(records: &[Arc<RepoDataRecord>], display: DisplayOptions) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    for record in records {
        let package = &record.package_record;
        let key = (
            &package.version,
            display.builds.then_some(package.build.as_str()),
            display.subdir.then_some(package.subdir.as_str()),
        );
        if !seen.insert(key) {
            continue;
        }
        let mut row = package.version.to_string();
        if display.builds {
            row.push_str(&format!("  {}", package.build));
        }
        if display.subdir {
            row.push_str(&format!("  {}", package.subdir));
        }
        rows.push(row);
    }
    rows
}

#[derive(serde::Serialize)]
struct JsonReport<'a> {
    input: &'a str,
    name: &'a str,
    mapped_from: Option<&'a str>,
    platforms: Vec<&'static str>,
    channels: Vec<JsonChannel<'a>>,
}

#[derive(serde::Serialize)]
struct JsonChannel<'a> {
    url: &'a str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    matches: Option<Vec<JsonMatch<'a>>>,
}

#[derive(serde::Serialize)]
struct JsonMatch<'a> {
    version: String,
    build: &'a str,
    subdir: &'a str,
    depends: &'a [String],
}

fn render_json(report: &SearchReport) -> Result<String, serde_json::Error> {
    let mapped_from = match &report.mapping {
        NameMapping::Mapped(pypi_name) => Some(pypi_name.as_str()),
        NameMapping::NotConsulted | NameMapping::Unmapped => None,
    };
    let channels = report
        .channels
        .iter()
        .map(|channel| match &channel.status {
            ChannelStatus::Matches(records) => JsonChannel {
                url: &channel.url,
                status: "ok",
                detail: None,
                matches: Some(
                    records
                        .iter()
                        .map(|record| JsonMatch {
                            version: record.package_record.version.to_string(),
                            build: &record.package_record.build,
                            subdir: &record.package_record.subdir,
                            depends: &record.package_record.depends,
                        })
                        .collect(),
                ),
            },
            ChannelStatus::NoMatches => JsonChannel {
                url: &channel.url,
                status: "no_matches",
                detail: None,
                matches: None,
            },
            ChannelStatus::NoSubdir(subdir) => JsonChannel {
                url: &channel.url,
                status: "no_subdir",
                detail: Some(subdir),
                matches: None,
            },
            ChannelStatus::Failed(message) => JsonChannel {
                url: &channel.url,
                status: "error",
                detail: Some(message),
                matches: None,
            },
        })
        .collect();
    let json = JsonReport {
        input: &report.input,
        name: &report.conda_name,
        mapped_from,
        platforms: report
            .platforms
            .iter()
            .map(|platform| platform.as_str())
            .collect(),
        channels,
    };
    let mut rendered = serde_json::to_string_pretty(&json)?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use ana_solver::ChannelQueryOutcome;
    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{PackageRecord, Version};

    use super::*;

    fn mapping(entries: &[(&str, &str)]) -> MappingHandle {
        MappingHandle::from_map(
            entries
                .iter()
                .map(|(pypi, conda)| (pypi.to_string(), conda.to_string()))
                .collect::<HashMap<_, _>>(),
        )
    }

    fn record(
        name: &str,
        version: &str,
        build: &str,
        build_number: u64,
        subdir: &str,
        channel_url: &str,
    ) -> RepoDataRecord {
        let mut package_record = PackageRecord::new(
            PackageName::new_unchecked(name),
            Version::from_str(version).unwrap(),
            build.to_string(),
        );
        package_record.subdir = subdir.to_string();
        package_record.build_number = build_number;
        let identifier =
            DistArchiveIdentifier::try_from_filename(&format!("{name}-{version}-{build}.conda"))
                .unwrap();
        let url = url::Url::parse(&format!(
            "{channel_url}{subdir}/{name}-{version}-{build}.conda"
        ))
        .unwrap();
        RepoDataRecord {
            package_record,
            identifier,
            url,
            channel: Some(channel_url.to_string()),
        }
    }

    fn spec(text: &str) -> SearchSpec {
        resolve_spec(text, &mapping(&[])).unwrap()
    }

    struct FakeQuery {
        outcomes: Vec<Result<Vec<Arc<RepoDataRecord>>, ChannelQueryError>>,
    }

    impl ChannelQuery for FakeQuery {
        fn query_channels(
            &self,
            channels: &[Channel],
            _platforms: &[Platform],
            _spec: &MatchSpec,
        ) -> Vec<ChannelQueryOutcome> {
            channels
                .iter()
                .zip(&self.outcomes)
                .map(|(channel, result)| ChannelQueryOutcome {
                    channel: channel.base_url.clone(),
                    result: match result {
                        Ok(records) => Ok(records.clone()),
                        Err(ChannelQueryError::SubdirNotFound(subdir)) => {
                            Err(ChannelQueryError::SubdirNotFound(subdir.clone()))
                        }
                        Err(ChannelQueryError::Fetch(message)) => {
                            Err(ChannelQueryError::Fetch(message.clone()))
                        }
                    },
                })
                .collect()
        }
    }

    fn policy(defaults: &[&str], allowed: &[&str]) -> ChannelPolicy {
        let defaults: Vec<String> = defaults.iter().map(ToString::to_string).collect();
        let allowed: Vec<String> = allowed.iter().map(ToString::to_string).collect();
        ChannelPolicy::new(&defaults, &allowed).unwrap()
    }

    fn search_channels(policy: &ChannelPolicy, names: &[&str]) -> Vec<Channel> {
        let args: Vec<String> = names.iter().map(ToString::to_string).collect();
        resolve_channels(policy, &args, &spec("numpy"), &[Platform::Linux64]).unwrap()
    }

    #[test]
    fn a_bare_name_is_searched_unmapped() {
        let spec = spec("numpy");
        assert_eq!(spec.conda_name, "numpy");
        assert_eq!(spec.mapping, NameMapping::Unmapped);
        assert_eq!(spec.spec.version, None);
        assert_eq!(spec.pinned_channel, None);
    }

    #[test]
    fn a_mapped_pypi_name_searches_the_conda_name() {
        let spec = resolve_spec("duckdb", &mapping(&[("duckdb", "python-duckdb")])).unwrap();
        assert_eq!(spec.conda_name, "python-duckdb");
        assert_eq!(spec.mapping, NameMapping::Mapped("duckdb".to_string()));
        assert_eq!(
            spec.spec.name.as_exact().map(PackageName::as_normalized),
            Some("python-duckdb")
        );
    }

    #[test]
    fn a_version_constraint_becomes_the_query_filter() {
        let spec = spec("numpy>=2");
        assert_eq!(spec.conda_name, "numpy");
        assert!(spec.spec.version.is_some());
    }

    #[test]
    fn extras_markers_and_urls_are_rejected_with_the_base_name() {
        let err = resolve_spec("fastapi[standard]", &mapping(&[])).unwrap_err();
        assert!(
            matches!(&err, SearchError::Extras { name, .. } if name == "fastapi"),
            "{err:?}"
        );

        let err = resolve_spec("numpy; os_name == 'linux'", &mapping(&[])).unwrap_err();
        assert!(
            matches!(&err, SearchError::Marker { name, .. } if name == "numpy"),
            "{err:?}"
        );

        let err = resolve_spec("numpy @ https://example.com/numpy.whl", &mapping(&[])).unwrap_err();
        assert!(matches!(err, SearchError::DirectUrl { .. }), "{err:?}");
    }

    #[test]
    fn a_matchspec_spec_skips_the_mapping() {
        let spec = spec("::python-duckdb");
        assert_eq!(spec.conda_name, "python-duckdb");
        assert_eq!(spec.mapping, NameMapping::NotConsulted);
    }

    /// A `channel/subdir::name` spec's subdir is never consulted by the
    /// query's matcher, so it is rejected toward `--subdir` rather than
    /// silently searching the wrong platform.
    #[test]
    fn a_matchspec_with_a_subdir_is_rejected_toward_the_subdir_flag() {
        let err = resolve_spec("main/linux-64::conda", &mapping(&[])).unwrap_err();
        assert!(
            matches!(&err, SearchError::Subdir { subdir, .. } if subdir == "linux-64"),
            "{err:?}"
        );
        assert!(err.to_string().contains("--subdir linux-64"), "{err}");
    }

    #[test]
    fn a_channel_pin_is_lifted_out_of_the_spec() {
        let spec = spec("conda-forge::numpy");
        let pinned = spec.pinned_channel.as_ref().unwrap();
        assert_eq!(
            pinned.base_url.as_str(),
            "https://conda.anaconda.org/conda-forge/"
        );
        assert!(spec.spec.channel.is_none());
    }

    #[test]
    fn an_unparseable_spec_is_a_parse_error() {
        let err = resolve_spec("!!!", &mapping(&[])).unwrap_err();
        assert!(matches!(err, SearchError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn the_configured_defaults_are_the_default_search_list() {
        let policy = policy(&["conda-forge"], &[]);
        let channels = search_channels(&policy, &[]);
        let urls: Vec<&str> = channels.iter().map(|c| c.base_url.as_str()).collect();
        assert_eq!(urls, ["https://conda.anaconda.org/conda-forge/"]);
    }

    #[test]
    fn channel_args_replace_the_defaults() {
        let policy = policy(&["defaults"], &["bioconda"]);
        let channels = search_channels(&policy, &["bioconda"]);
        let urls: Vec<&str> = channels.iter().map(|c| c.base_url.as_str()).collect();
        assert_eq!(urls, ["https://conda.anaconda.org/bioconda/"]);
    }

    #[test]
    fn an_unauthorized_channel_arg_names_the_flag() {
        let policy = policy(&["conda-forge"], &[]);
        let args = vec!["bioconda".to_string()];
        let err =
            resolve_channels(&policy, &args, &spec("numpy"), &[Platform::Linux64]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bioconda"), "{message}");
        assert!(message.contains("--channel"), "{message}");
    }

    #[test]
    fn a_pinned_channel_is_the_only_channel_searched() {
        let policy = policy(&["conda-forge"], &[]);
        let spec = spec("conda-forge::numpy");
        let channels = resolve_channels(&policy, &[], &spec, &[Platform::Linux64]).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(
            channels[0].base_url.as_str(),
            "https://conda.anaconda.org/conda-forge/"
        );
    }

    #[test]
    fn an_unauthorized_pin_names_the_spec() {
        let policy = policy(&["conda-forge"], &[]);
        let spec = spec("bioconda::numpy");
        let err = resolve_channels(&policy, &[], &spec, &[Platform::Linux64]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bioconda"), "{message}");
        assert!(message.contains("bioconda::numpy"), "{message}");
    }

    #[test]
    fn a_pin_and_channel_args_conflict() {
        let policy = policy(&["conda-forge"], &[]);
        let spec = spec("conda-forge::numpy");
        let args = vec!["conda-forge".to_string()];
        let err = resolve_channels(&policy, &args, &spec, &[Platform::Linux64]).unwrap_err();
        assert!(
            matches!(err, SearchError::ChannelPinConflict { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_channel_set_unions_across_platforms() {
        // `msys2` is Windows-only within `defaults`: it appears exactly
        // once, and only when a Windows platform is requested.
        let policy = policy(&["defaults"], &[]);
        let linux_only = search_channels(&policy, &[]);
        assert!(
            !linux_only
                .iter()
                .any(|c| c.base_url.as_str().contains("msys2")),
            "{linux_only:?}"
        );

        let args: Vec<String> = Vec::new();
        let with_windows = resolve_channels(
            &policy,
            &args,
            &spec("numpy"),
            &[Platform::Linux64, Platform::Win64],
        )
        .unwrap();
        let msys2_count = with_windows
            .iter()
            .filter(|c| c.base_url.as_str().contains("msys2"))
            .count();
        assert_eq!(msys2_count, 1, "{with_windows:?}");
    }

    #[test]
    fn matches_are_sorted_newest_first_and_failures_stay_per_channel() {
        let policy = policy(&["conda-forge"], &[]);
        let channels = search_channels(&policy, &[]);
        let url = channels[0].base_url.as_str().to_string();
        let querier = FakeQuery {
            outcomes: vec![Ok(vec![
                Arc::new(record("numpy", "1.0.0", "py310_0", 0, "linux-64", &url)),
                Arc::new(record("numpy", "2.0.0", "py310_0", 0, "linux-64", &url)),
                Arc::new(record("numpy", "2.0.0", "py310_1", 1, "linux-64", &url)),
            ])],
        };

        let report = search(&spec("numpy"), &channels, &[Platform::Linux64], &querier);

        assert!(report.any_matches());
        assert!(!report.any_channel_failed());
        assert_eq!(report.platforms, vec![Platform::Linux64, Platform::NoArch]);
        let ChannelStatus::Matches(records) = &report.channels[0].status else {
            panic!("expected matches: {:?}", report.channels[0].status);
        };
        let versions: Vec<String> = records
            .iter()
            .map(|r| {
                format!(
                    "{}-{}-{}",
                    r.package_record.version, r.package_record.build, r.package_record.build_number
                )
            })
            .collect();
        assert_eq!(
            versions,
            ["2.0.0-py310_1-1", "2.0.0-py310_0-0", "1.0.0-py310_0-0"]
        );
    }

    #[test]
    fn empty_and_failed_outcomes_become_their_own_statuses() {
        let policy = policy(&["conda-forge", "bioconda"], &[]);
        let channels = search_channels(&policy, &[]);
        let querier = FakeQuery {
            outcomes: vec![
                Ok(Vec::new()),
                Err(ChannelQueryError::Fetch("connection refused".to_string())),
            ],
        };

        let report = search(&spec("numpy"), &channels, &[Platform::Linux64], &querier);

        assert!(!report.any_matches());
        assert!(!report.all_channels_failed());
        assert!(report.any_channel_failed());
        assert!(matches!(
            report.channels[0].status,
            ChannelStatus::NoMatches
        ));
        assert!(matches!(
            report.channels[1].status,
            ChannelStatus::Failed(_)
        ));
    }

    #[test]
    fn all_channels_failed_only_when_none_answered() {
        let policy = policy(&["conda-forge"], &[]);
        let channels = search_channels(&policy, &[]);
        let querier = FakeQuery {
            outcomes: vec![Err(ChannelQueryError::SubdirNotFound("noarch".to_string()))],
        };

        let report = search(&spec("numpy"), &channels, &[Platform::Linux64], &querier);

        assert!(report.all_channels_failed());
        assert!(report.any_channel_failed());
        assert!(matches!(
            report.channels[0].status,
            ChannelStatus::NoSubdir(_)
        ));
    }

    fn report_with(records: Vec<RepoDataRecord>) -> SearchReport {
        let url = "https://conda.anaconda.org/conda-forge/";
        SearchReport {
            input: "numpy".to_string(),
            conda_name: "numpy".to_string(),
            mapping: NameMapping::Unmapped,
            platforms: vec![Platform::Linux64, Platform::NoArch],
            channels: vec![ChannelReport {
                url: url.to_string(),
                status: ChannelStatus::Matches(records.into_iter().map(Arc::new).collect()),
            }],
        }
    }

    #[test]
    fn the_default_summary_lists_distinct_versions_only() {
        let url = "https://conda.anaconda.org/conda-forge/";
        let report = report_with(vec![
            record("numpy", "2.0.0", "py310h6a678d5_0", 0, "linux-64", url),
            record("numpy", "2.0.0", "py39h6a678d5_0", 0, "linux-64", url),
            record("numpy", "1.0.0", "py310h6a678d5_0", 0, "noarch", url),
        ]);

        let rendered = render(&report, SearchFormat::Summary, DisplayOptions::default()).unwrap();

        assert_eq!(
            rendered,
            "https://conda.anaconda.org/conda-forge/: 2 versions, latest 2.0.0\n  2.0.0\n  1.0.0\n"
        );
    }

    #[test]
    fn builds_and_subdir_are_shown_only_when_asked_for() {
        let url = "https://conda.anaconda.org/conda-forge/";
        let report = report_with(vec![record(
            "numpy",
            "2.0.0",
            "py310h6a678d5_0",
            0,
            "linux-64",
            url,
        )]);

        let with_builds = render(
            &report,
            SearchFormat::Summary,
            DisplayOptions {
                builds: true,
                ..DisplayOptions::default()
            },
        )
        .unwrap();
        assert!(
            with_builds.contains("  2.0.0  py310h6a678d5_0\n"),
            "{with_builds}"
        );
        assert!(!with_builds.contains("linux-64"), "{with_builds}");

        let with_subdir = render(
            &report,
            SearchFormat::Summary,
            DisplayOptions {
                subdir: true,
                ..DisplayOptions::default()
            },
        )
        .unwrap();
        assert!(with_subdir.contains("  2.0.0  linux-64\n"), "{with_subdir}");
        assert!(!with_subdir.contains("py310h6a678d5_0"), "{with_subdir}");
    }

    #[test]
    fn deps_lists_the_newest_matchs_dependencies() {
        let url = "https://conda.anaconda.org/conda-forge/";
        let mut newest = record("numpy", "2.0.0", "py310_0", 0, "linux-64", url);
        newest.package_record.depends = vec!["python >=3.10".to_string()];
        let report = report_with(vec![
            newest,
            record("numpy", "1.0.0", "py39_0", 0, "linux-64", url),
        ]);

        let rendered = render(
            &report,
            SearchFormat::Summary,
            DisplayOptions {
                deps: true,
                ..DisplayOptions::default()
            },
        )
        .unwrap();

        assert!(
            rendered.contains("\n  latest (2.0.0) depends on:\n    python >=3.10\n"),
            "{rendered}"
        );
    }

    #[test]
    fn the_summary_renders_each_failure_mode_on_its_own_line() {
        let report = SearchReport {
            input: "numpy".to_string(),
            conda_name: "numpy".to_string(),
            mapping: NameMapping::Unmapped,
            platforms: vec![Platform::Linux64, Platform::NoArch],
            channels: vec![
                ChannelReport {
                    url: "https://a.example/".to_string(),
                    status: ChannelStatus::NoMatches,
                },
                ChannelReport {
                    url: "https://b.example/".to_string(),
                    status: ChannelStatus::NoSubdir("noarch".to_string()),
                },
                ChannelReport {
                    url: "https://c.example/".to_string(),
                    status: ChannelStatus::Failed("connection refused".to_string()),
                },
            ],
        };

        let rendered = render(&report, SearchFormat::Summary, DisplayOptions::default()).unwrap();

        assert_eq!(
            rendered,
            "https://a.example/: no matches\n\
             \n\
             https://b.example/: no 'noarch' subdir\n\
             \n\
             https://c.example/: error: connection refused\n"
        );
    }

    #[test]
    fn json_carries_every_field_regardless_of_display_options() {
        let url = "https://conda.anaconda.org/conda-forge/";
        let mut newest = record("numpy", "2.0.0", "py310_0", 0, "linux-64", url);
        newest.package_record.depends = vec!["python >=3.10".to_string()];
        let mut report = report_with(vec![newest]);
        report.mapping = NameMapping::Mapped("np".to_string());
        report.channels.push(ChannelReport {
            url: "https://down.example/".to_string(),
            status: ChannelStatus::Failed("connection refused".to_string()),
        });

        let rendered = render(&report, SearchFormat::Json, DisplayOptions::default()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(value["input"], "numpy");
        assert_eq!(value["name"], "numpy");
        assert_eq!(value["mapped_from"], "np");
        assert_eq!(
            value["platforms"],
            serde_json::json!(["linux-64", "noarch"])
        );
        assert_eq!(value["channels"][0]["status"], "ok");
        assert_eq!(value["channels"][0]["matches"][0]["version"], "2.0.0");
        assert_eq!(value["channels"][0]["matches"][0]["build"], "py310_0");
        assert_eq!(value["channels"][0]["matches"][0]["subdir"], "linux-64");
        assert_eq!(
            value["channels"][0]["matches"][0]["depends"],
            serde_json::json!(["python >=3.10"])
        );
        assert_eq!(value["channels"][1]["status"], "error");
        assert_eq!(value["channels"][1]["detail"], "connection refused");
    }
}
