//! [`ChannelPolicy`]: the only code that compares a channel/artifact URL
//! against the configured `default_channels ∪ allowed_channels` set.
//!
//! `allowed_channels` entries are rules: an entry ending `/*` becomes a
//! prefix rule after its prefix is credential-checked; any other entry is
//! normalized and becomes an exact rule. `default_channels` is both a
//! search list and a set of rules, so its entries must be concrete and
//! contribute exact rules -- a `/*` pattern there is
//! [`Error::WildcardNotAllowedHere`], since a search list names channels
//! to actually search, not a pattern to match against.
//!
//! A `/*` pattern is legal only in `allowed_channels`. A pattern must end
//! in `/*` so matching lands on a path-segment boundary:
//! `https://example.com/pkgs/main/*` authorizes `.../pkgs/main/dev/` and
//! `.../pkgs/main/` itself, but not `.../pkgs/mainline/`.

use std::path::PathBuf;
use std::str::FromStr;

use rattler_conda_types::package::DistArchiveIdentifier;
use rattler_conda_types::{Channel, ChannelConfig, ChannelUrl, Platform, RepoDataRecord};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::alias::meta_channel_members;
use crate::error::Error;
use crate::normalize::{normalize_channel, parse_alias_url};

/// The channel `package` can be trusted to have actually been fetched
/// from: `package.channel` when it parses as a URL and its own
/// [`ChannelUrl`] reconstructs `package.url` exactly --
/// `<channel>/<subdir>/<filename>`, `<subdir>` a real [`Platform`] (never
/// an extra path segment beyond it), `<filename>` matching
/// `package.identifier` -- the layout every real solve produces (a fetch
/// URL is always `channel` joined with its subdir and filename; see
/// `rattler_repodata_gateway`'s record construction). `None` when
/// `channel` is absent, doesn't parse as a URL, or doesn't account for
/// `url` this way.
///
/// `package.channel` is free-text, independently settable from `url` in
/// a hand-edited `ana.lock`, and never itself consulted by anything that
/// actually fetches a package -- only `url` is ever fetched from -- so a
/// mismatch here must never be trusted on `channel`'s word alone. This is
/// the one place in the workspace that resolves a locked/solved
/// package's *actual* channel identity; every caller that needs a
/// channel-based decision about such a package (is it still allowed?
/// does it need to run under a sandbox?) goes through this check first,
/// falling back to deriving a channel from `package.url` itself (via
/// [`artifact_channel`]) for a package this returns `None` for.
///
/// A channel whose repodata redirects packages to a mirror via its own
/// `base_url` override (a real conda feature) produces a `url` this
/// can't reconstruct, so such a record is rejected rather than trusted;
/// re-solving is how that channel is picked back up.
pub fn trusted_channel(package: &RepoDataRecord) -> Option<ChannelUrl> {
    let channel_url: ChannelUrl = Url::parse(package.channel.as_deref()?).ok()?.into();
    let subdir = Platform::from_str(&package.package_record.subdir).ok()?;
    let expected = channel_url
        .platform_url(subdir)
        .join(&package.identifier.to_string())
        .ok()?;
    (expected == package.url).then_some(channel_url)
}

/// The channel an artifact `url` was fetched from, derived from the
/// url's own conventional `<channel>/<subdir>/<filename>` layout: no
/// query or fragment, last path segment a package archive filename, the
/// segment before it a known [`Platform`] subdir -- everything above
/// that subdir is the channel. `None` when `url` has any other shape
/// (in particular, extra path segments between the channel and the
/// subdir are fine -- they are part of the channel -- but nothing may
/// follow the filename, and the subdir segment must be a real
/// [`Platform`]).
///
/// This is the only way a bare artifact URL acquires a channel identity
/// in `ana`; once derived, the channel is matched against a
/// [`ChannelPolicy`] exactly like a declared channel
/// ([`ChannelPolicy::authorizes_channel`]) -- rules never match against
/// an artifact URL directly.
pub fn artifact_channel(url: &Url) -> Option<ChannelUrl> {
    if url.query().is_some() || url.fragment().is_some() {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.collect();
    let (filename, rest) = segments.split_last()?;
    let (subdir, channel_segments) = rest.split_last()?;
    Platform::from_str(subdir).ok()?;
    DistArchiveIdentifier::try_from_filename(filename)?;
    let mut base = url.clone();
    if channel_segments.is_empty() {
        base.set_path("/");
    } else {
        base.set_path(&format!("/{}/", channel_segments.join("/")));
    }
    Some(base.into())
}

/// One allow-list rule: an exact channel, or a `/*`-suffixed prefix.
#[derive(Debug, Clone)]
enum Rule {
    Exact(ChannelUrl),
    Prefix(String),
}

impl Rule {
    fn authorizes_channel(&self, url: &ChannelUrl) -> bool {
        match self {
            Rule::Exact(exact) => exact == url,
            Rule::Prefix(prefix) => url.as_str().starts_with(prefix.as_str()),
        }
    }
}

/// One [`ChannelSet`] member: a resolved [`Channel`], plus whether it
/// applies only on Windows (the `msys2` meta-channel constituent).
#[derive(Debug, Clone)]
struct ChannelSetMember {
    channel: Channel,
    windows_only: bool,
}

/// An ordered, first-occurrence-wins collection of channels, each carrying
/// the platforms it applies to. [`ChannelSet::for_platform`] is the one
/// place the Windows-only `msys2` rule lives.
#[derive(Debug, Clone, Default)]
pub struct ChannelSet {
    members: Vec<ChannelSetMember>,
}

impl ChannelSet {
    fn push_if_new(&mut self, channel: Channel, windows_only: bool) {
        if self
            .members
            .iter()
            .any(|member| member.channel.base_url == channel.base_url)
        {
            return;
        }
        self.members.push(ChannelSetMember {
            channel,
            windows_only,
        });
    }

    /// Every member applicable to `platform`, in order.
    pub fn for_platform(&self, platform: Platform) -> Vec<Channel> {
        self.members
            .iter()
            .filter(|member| !member.windows_only || platform.is_windows())
            .map(|member| member.channel.clone())
            .collect()
    }

    /// Every member's channel url, in order -- the [`digest_of`] input.
    fn channel_urls(&self) -> impl Iterator<Item = &ChannelUrl> {
        self.members.iter().map(|member| &member.channel.base_url)
    }

    /// Whether `url` names one of this set's members, regardless of
    /// platform applicability. Used to validate an already-locked
    /// package's `channel` against the set it was solved from -- a
    /// record's own platform is irrelevant to whether its channel is
    /// still a member of the set.
    pub fn contains(&self, url: &ChannelUrl) -> bool {
        self.members
            .iter()
            .any(|member| &member.channel.base_url == url)
    }
}

/// One per-package channel claim a caller wants checked: a `channel::`
/// override (`channel`) or a bare package-URL dependency (`url`) -- never
/// both, since a `MatchSpec` never sets more than one of its own `channel`/
/// `url` fields.
pub struct ChannelOverride<'a> {
    pub channel: Option<&'a Channel>,
    pub url: Option<&'a Url>,
    /// Where this override came from, for the violation message
    /// (`"runtime"`, `"group:dev"`, ...).
    pub context: &'a str,
}

/// [`ChannelPolicy::effective_channels`]'s result: the channel set a solve
/// should run against, plus a fingerprint of that same set for later
/// staleness comparison (see [`digest_of`]).
#[derive(Debug, Clone)]
pub struct EffectiveChannels {
    pub set: ChannelSet,
    pub digest: String,
}

/// The channel allow-policy: `default_channels ∪ allowed_channels`,
/// resolved once. The only code that compares a channel/artifact URL
/// against that set.
#[derive(Debug, Clone, Default)]
pub struct ChannelPolicy {
    defaults: ChannelSet,
    rules: Vec<Rule>,
}

impl ChannelPolicy {
    /// Resolves and validates `default_channels`/`allowed_channels` once.
    /// A meta-channel name (`"defaults"`) in either list expands to its
    /// members. A `/*`-suffixed entry is legal only in `allowed_channels`;
    /// one in `default_channels` is [`Error::WildcardNotAllowedHere`].
    pub fn new(default_channels: &[String], allowed_channels: &[String]) -> Result<Self, Error> {
        let mut defaults = ChannelSet::default();
        let mut rules: Vec<Rule> = Vec::new();

        for raw in default_channels {
            reject_wildcard(raw)?;
            match resolve_entry(raw)? {
                ResolvedEntry::Single(channel) => {
                    push_rule(&mut rules, Rule::Exact(channel.base_url.clone()));
                    defaults.push_if_new(channel, false);
                }
                ResolvedEntry::Meta(members) => {
                    for (channel, windows_only) in members {
                        push_rule(&mut rules, Rule::Exact(channel.base_url.clone()));
                        defaults.push_if_new(channel, windows_only);
                    }
                }
            }
        }

        for raw in allowed_channels {
            if let Some(prefix_pattern) = raw.strip_suffix("/*") {
                push_rule(&mut rules, Rule::Prefix(validate_prefix(prefix_pattern)?));
                continue;
            }
            match resolve_entry(raw)? {
                ResolvedEntry::Single(channel) => {
                    push_rule(&mut rules, Rule::Exact(channel.base_url));
                }
                ResolvedEntry::Meta(members) => {
                    for (channel, _) in members {
                        push_rule(&mut rules, Rule::Exact(channel.base_url));
                    }
                }
            }
        }

        Ok(Self { defaults, rules })
    }

    /// Whether `url` is authorized: an exact rule matches by equality, a
    /// prefix rule matches by string prefix (see [`Rule::authorizes_channel`]).
    /// The only authorization question the policy answers -- an artifact
    /// URL is first reduced to its channel via [`artifact_channel`], then
    /// asked here.
    pub fn authorizes_channel(&self, url: &ChannelUrl) -> bool {
        self.rules.iter().any(|rule| rule.authorizes_channel(url))
    }

    /// Validates `project_channels` (if the project declares an override)
    /// and every `overrides` entry against this policy, then returns the
    /// channel set a solve should run against, plus its digest.
    ///
    /// `project_channels`, if present, *replaces* the configured defaults
    /// as the search list (every entry still checked against the policy);
    /// `None` means the configured defaults are used as-is, never checked
    /// against the policy themselves. Every violation is collected into
    /// one [`Error::ChannelNotAllowed`] rather than failing on the first;
    /// a malformed channel string fails fast instead, as
    /// [`Error::InvalidChannel`].
    pub fn effective_channels(
        &self,
        project_channels: Option<&[String]>,
        overrides: &[ChannelOverride<'_>],
    ) -> Result<EffectiveChannels, Error> {
        let mut violations: Vec<String> = Vec::new();

        let mut set = match project_channels {
            Some(list) => {
                let mut set = ChannelSet::default();
                for raw in list {
                    reject_wildcard(raw)?;
                    match resolve_entry(raw)? {
                        ResolvedEntry::Single(channel) => {
                            if self.authorizes_channel(&channel.base_url) {
                                set.push_if_new(channel, false);
                            } else {
                                violations.push(format!(
                                    "  {raw:?} (from tool.ana.conda-channels): not in \
                                     default_channels/allowed_channels"
                                ));
                            }
                        }
                        ResolvedEntry::Meta(members) => {
                            for (channel, windows_only) in members {
                                if self.authorizes_channel(&channel.base_url) {
                                    set.push_if_new(channel, windows_only);
                                } else {
                                    violations.push(format!(
                                        "  {raw:?} (from tool.ana.conda-channels): not in \
                                         default_channels/allowed_channels"
                                    ));
                                }
                            }
                        }
                    }
                }
                set
            }
            None => self.defaults.clone(),
        };

        for over in overrides {
            if let Some(channel) = over.channel {
                let normalized = normalize_channel(channel.clone())?;
                if self.authorizes_channel(&normalized.base_url) {
                    set.push_if_new(normalized, false);
                } else {
                    violations.push(format!(
                        "  channel {:?} (from {}): not in default_channels/allowed_channels",
                        normalized.canonical_name(),
                        over.context
                    ));
                }
            } else if let Some(url) = over.url {
                match artifact_channel(url) {
                    Some(channel) if self.authorizes_channel(&channel) => {
                        set.push_if_new(Channel::from_url(channel), false);
                    }
                    _ => violations.push(format!(
                        "  url {:?} (from {}): does not fall under any allowed channel",
                        url.as_str(),
                        over.context
                    )),
                }
            }
        }

        if !violations.is_empty() {
            return Err(Error::ChannelNotAllowed(violations.join("\n")));
        }

        let digest = digest_of(&set);
        Ok(EffectiveChannels { set, digest })
    }
}

/// Which position a `config.toml`/`ana config set` channel-list entry
/// occupies: a search list (`default_channels`, `dry_solve_channels`, a
/// project's own `conda-channels`) or the allow list (`allowed_channels`)
/// -- the only position a `/*` wildcard pattern is legal in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelListPosition {
    SearchList,
    AllowList,
}

/// Validates one channel-list entry for its position, without building a
/// full [`ChannelPolicy`]: used by `ana config set` (and a compiled
/// config's own build-time validation) to catch a bad entry -- a
/// `file://` channel, a credentialed URL, or a misplaced `/*` pattern --
/// before it is ever written, with the same rules [`ChannelPolicy::new`]
/// enforces for a full list.
pub fn validate_channel_entry(position: ChannelListPosition, raw: &str) -> Result<(), Error> {
    match position {
        ChannelListPosition::SearchList => {
            reject_wildcard(raw)?;
            resolve_entry(raw)?;
        }
        ChannelListPosition::AllowList => {
            if let Some(prefix) = raw.strip_suffix("/*") {
                validate_prefix(prefix)?;
            } else {
                resolve_entry(raw)?;
            }
        }
    }
    Ok(())
}

/// One `default_channels`/`allowed_channels`/`conda-channels` string,
/// resolved: either a single channel, or a meta-channel's expansion (each
/// member paired with whether it is Windows-only).
enum ResolvedEntry {
    Single(Channel),
    Meta(Vec<(Channel, bool)>),
}

/// Resolves one channel-list string to its [`ResolvedEntry`]: a
/// meta-channel name (`"defaults"`) expands to its members (each run
/// through [`normalize_channel`]); anything else is a single channel, also
/// normalized.
fn resolve_entry(raw: &str) -> Result<ResolvedEntry, Error> {
    if raw.contains('*') {
        // Only a `/*`-suffixed `allowed_channels` entry is a wildcard
        // pattern (stripped and handled before this function is ever
        // called); anything else containing a literal `*` is malformed,
        // not a literal channel name/URL character ana recognizes.
        return Err(Error::InvalidChannel {
            name: raw.to_string(),
            source: rattler_conda_types::ParseChannelError::InvalidName(raw.to_string()),
        });
    }

    let channel_config = ChannelConfig::default_with_root_dir(PathBuf::new());
    let channel =
        Channel::from_str(raw, &channel_config).map_err(|source| Error::InvalidChannel {
            name: raw.to_string(),
            source,
        })?;

    if let Some(name) = channel.name.as_deref() {
        if let Some(members) = meta_channel_members(name) {
            let mut resolved = Vec::with_capacity(members.len());
            for member in members {
                let url = parse_alias_url(member.alias.url)?;
                let member_channel = normalize_channel(Channel::from_url(url))?;
                resolved.push((member_channel, member.windows_only));
            }
            return Ok(ResolvedEntry::Meta(resolved));
        }
    }

    Ok(ResolvedEntry::Single(normalize_channel(channel)?))
}

/// A `/*` pattern is legal only in `allowed_channels`; reject it wherever
/// else a channel-list string appears (a search list, or a single
/// per-package pin).
fn reject_wildcard(raw: &str) -> Result<(), Error> {
    if raw.ends_with("/*") {
        return Err(Error::WildcardNotAllowedHere {
            entry: raw.to_string(),
        });
    }
    Ok(())
}

/// Validates and normalizes an `allowed_channels` prefix pattern's prefix
/// (the part before `/*`): parsed as a URL, credential-checked, rejected
/// if it resolves to a local filesystem path, then rendered back out with
/// its trailing slash restored -- so `pkgs/main/*` authorizes
/// `pkgs/main/dev/` and `pkgs/main/` itself, but never `pkgs/mainline/`.
fn validate_prefix(prefix: &str) -> Result<String, Error> {
    let url = Url::parse(prefix).map_err(|source| Error::InvalidChannel {
        name: format!("{prefix}/*"),
        source: rattler_conda_types::ParseChannelError::ParseUrlError(source),
    })?;
    // Reuses `normalize_channel`'s own credential/local-filesystem gate by
    // running the prefix through it as a channel URL; the resulting
    // `base_url` (forced trailing slash, alias-rewritten if applicable) is
    // exactly the string a later prefix-match compares against.
    let channel = normalize_channel(Channel::from_url(url))?;
    Ok(channel.base_url.as_str().to_string())
}

fn push_rule(rules: &mut Vec<Rule>, rule: Rule) {
    let duplicate = match &rule {
        Rule::Exact(url) => rules
            .iter()
            .any(|existing| matches!(existing, Rule::Exact(existing_url) if existing_url == url)),
        Rule::Prefix(prefix) => rules.iter().any(
            |existing| matches!(existing, Rule::Prefix(existing_prefix) if existing_prefix == prefix),
        ),
    };
    if !duplicate {
        rules.push(rule);
    }
}

/// A stable fingerprint of `set`'s member channel URLs, in order -- never
/// their literal spelling. A staleness-detection fingerprint, not a
/// security boundary, so collision resistance beyond SHA-256's own is not
/// required. Each entry is length-prefixed so no concatenation of two
/// entries can collide with a different split of the same bytes.
fn digest_of(set: &ChannelSet) -> String {
    let mut hasher = Sha256::new();
    for url in set.channel_urls() {
        let bytes = url.as_str().as_bytes();
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        use std::fmt::Write as _;
        // Writing two hex chars into a String never fails.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use rattler_conda_types::package::DistArchiveIdentifier;
    use rattler_conda_types::{NoArchType, PackageName, PackageRecord, Platform, Version};

    use super::*;

    fn channels(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    fn channel_url(text: &str) -> ChannelUrl {
        Url::parse(text).unwrap().into()
    }

    fn matchspec_channel(text: &str) -> Channel {
        let config = ChannelConfig::default_with_root_dir(PathBuf::new());
        normalize_channel(Channel::from_str(text, &config).unwrap()).unwrap()
    }

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
            url: Url::parse(url).unwrap(),
            channel: channel.map(ToString::to_string),
        }
    }

    // -- trusted_channel ----------------------------------------------------

    #[test]
    fn trusted_channel_accepts_a_channel_that_accounts_for_the_url() {
        let package = record(
            Some("https://conda.anaconda.org/conda-forge/"),
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        );
        assert_eq!(
            trusted_channel(&package),
            Some(channel_url("https://conda.anaconda.org/conda-forge/"))
        );
    }

    #[test]
    fn trusted_channel_rejects_a_channel_whose_url_actually_points_elsewhere() {
        // `channel` claims conda-forge, but `url` -- what `rattler`'s
        // installer actually fetches from -- is really bioconda. A
        // hand-edited or malicious `ana.lock` can set these two fields
        // inconsistently; `channel`'s claim must never be trusted on its
        // own.
        let package = record(
            Some("https://conda.anaconda.org/conda-forge/"),
            "https://conda.anaconda.org/bioconda/noarch/some-package-1.0.0-0.conda",
        );
        assert_eq!(trusted_channel(&package), None);
    }

    #[test]
    fn trusted_channel_rejects_a_channel_that_is_not_a_url() {
        // `channel` is solver-supplied free text -- it may not even parse
        // as a URL at all.
        let package = record(
            Some("not-a-url"),
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        );
        assert_eq!(trusted_channel(&package), None);
    }

    #[test]
    fn trusted_channel_is_none_without_a_channel_field() {
        let package = record(
            None,
            "https://conda.anaconda.org/conda-forge/noarch/some-package-1.0.0-0.conda",
        );
        assert_eq!(trusted_channel(&package), None);
    }

    // -- authorizes_channel / artifact_channel ----------------------------

    #[test]
    fn exact_rule_authorizes_only_its_own_channel() {
        let policy = ChannelPolicy::new(&[], &channels(&["conda-forge"])).unwrap();
        assert!(policy.authorizes_channel(&channel_url("https://conda.anaconda.org/conda-forge/")));
        assert!(!policy.authorizes_channel(&channel_url("https://conda.anaconda.org/bioconda/")));
    }

    #[test]
    fn prefix_rule_authorizes_a_segment_boundary_not_a_naive_substring() {
        let policy =
            ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main/*"])).unwrap();
        assert!(
            policy.authorizes_channel(&channel_url("https://example.com/pkgs/main/dev/")),
            "a sub-path under the prefix is authorized"
        );
        assert!(
            policy.authorizes_channel(&channel_url("https://example.com/pkgs/main/")),
            "the prefix's own base is authorized"
        );
        assert!(
            !policy.authorizes_channel(&channel_url("https://example.com/pkgs/mainline/")),
            "a same-prefix-string-but-different-segment channel must not match"
        );
    }

    #[test]
    fn artifact_channel_derives_the_channel_from_a_conventional_url() {
        let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0-0.conda")
            .unwrap();
        assert_eq!(
            artifact_channel(&url),
            Some(channel_url("https://conda.anaconda.org/conda-forge/"))
        );
    }

    #[test]
    fn artifact_channel_treats_intermediate_segments_as_part_of_the_channel() {
        let url =
            Url::parse("https://example.com/pkgs/main/dev/linux-64/numpy-1.0-0.conda").unwrap();
        assert_eq!(
            artifact_channel(&url),
            Some(channel_url("https://example.com/pkgs/main/dev/"))
        );
    }

    #[test]
    fn artifact_channel_accepts_a_channel_at_the_root() {
        let url = Url::parse("https://example.com/noarch/numpy-1.0-0.conda").unwrap();
        assert_eq!(
            artifact_channel(&url),
            Some(channel_url("https://example.com/"))
        );
    }

    #[test]
    fn artifact_channel_rejects_a_url_without_a_known_subdir() {
        // The segment above the filename is not a Platform, so the url
        // has no trustworthy channel identity at all -- an exact rule
        // must not match it by string prefix.
        let url =
            Url::parse("https://conda.anaconda.org/conda-forge/evil/numpy-1.0-0.conda").unwrap();
        assert_eq!(artifact_channel(&url), None);
    }

    #[test]
    fn artifact_channel_rejects_a_url_with_a_deep_path_below_the_channel() {
        // `evil/` sits *below* the subdir position: the derived channel
        // is `.../conda-forge/evil/`, which an exact conda-forge rule
        // must not authorize.
        let policy = ChannelPolicy::new(&[], &channels(&["conda-forge"])).unwrap();
        let url =
            Url::parse("https://conda.anaconda.org/conda-forge/evil/linux-64/numpy-1.0-0.conda")
                .unwrap();
        let derived = artifact_channel(&url).unwrap();
        assert!(!policy.authorizes_channel(&derived));
    }

    #[test]
    fn artifact_channel_rejects_query_and_fragment() {
        let query = Url::parse(
            "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0-0.conda?token=abc",
        )
        .unwrap();
        assert_eq!(artifact_channel(&query), None);
        let fragment =
            Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0-0.conda#sha256")
                .unwrap();
        assert_eq!(artifact_channel(&fragment), None);
    }

    #[test]
    fn artifact_channel_rejects_a_non_archive_filename() {
        let url =
            Url::parse("https://conda.anaconda.org/conda-forge/linux-64/repodata.json").unwrap();
        assert_eq!(artifact_channel(&url), None);
    }

    #[test]
    fn a_derived_channel_is_authorized_like_a_declared_one() {
        let exact = ChannelPolicy::new(&[], &channels(&["conda-forge"])).unwrap();
        let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0-0.conda")
            .unwrap();
        assert!(exact.authorizes_channel(&artifact_channel(&url).unwrap()));
        let other =
            Url::parse("https://packages.evil.example/x/linux-64/numpy-1.0-0.conda").unwrap();
        assert!(!exact.authorizes_channel(&artifact_channel(&other).unwrap()));

        let prefix =
            ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main/*"])).unwrap();
        let url =
            Url::parse("https://example.com/pkgs/main/dev/linux-64/numpy-1.0-0.conda").unwrap();
        assert!(prefix.authorizes_channel(&artifact_channel(&url).unwrap()));
        let outside =
            Url::parse("https://example.com/pkgs/mainline/linux-64/numpy-1.0-0.conda").unwrap();
        assert!(!prefix.authorizes_channel(&artifact_channel(&outside).unwrap()));
    }

    // -- Rule parsing -------------------------------------------------------

    #[test]
    fn wildcard_is_accepted_in_allowed_channels() {
        assert!(ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main/*"])).is_ok());
    }

    #[test]
    fn wildcard_is_rejected_in_default_channels() {
        let err =
            ChannelPolicy::new(&channels(&["https://example.com/pkgs/main/*"]), &[]).unwrap_err();
        assert!(matches!(err, Error::WildcardNotAllowedHere { .. }));
    }

    #[test]
    fn a_pattern_not_ending_in_slash_star_is_rejected() {
        let err =
            ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main*"])).unwrap_err();
        assert!(matches!(err, Error::InvalidChannel { .. }));
    }

    // -- ChannelSet::for_platform --------------------------------------------

    #[test]
    fn for_platform_includes_msys2_only_on_windows() {
        let policy = ChannelPolicy::new(&channels(&["defaults"]), &[]).unwrap();
        let effective = policy.effective_channels(None, &[]).unwrap();

        let linux: Vec<String> = effective
            .set
            .for_platform(Platform::Linux64)
            .into_iter()
            .map(|c| c.base_url.as_str().to_string())
            .collect();
        assert_eq!(
            linux,
            vec![
                "https://repo.anaconda.com/pkgs/main/",
                "https://repo.anaconda.cloud/repo/main-x/",
                "https://repo.anaconda.com/pkgs/r/",
            ]
        );

        let windows: Vec<String> = effective
            .set
            .for_platform(Platform::Win64)
            .into_iter()
            .map(|c| c.base_url.as_str().to_string())
            .collect();
        assert_eq!(
            windows,
            vec![
                "https://repo.anaconda.com/pkgs/main/",
                "https://repo.anaconda.cloud/repo/main-x/",
                "https://repo.anaconda.com/pkgs/r/",
                "https://repo.anaconda.com/pkgs/msys2/",
            ]
        );
    }

    #[test]
    fn channel_set_preserves_order_and_dedupes_first_occurrence() {
        let policy =
            ChannelPolicy::new(&channels(&["conda-forge", "defaults", "conda-forge"]), &[])
                .unwrap();
        let effective = policy.effective_channels(None, &[]).unwrap();
        let urls: Vec<String> = effective
            .set
            .for_platform(Platform::Linux64)
            .into_iter()
            .map(|c| c.base_url.as_str().to_string())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://conda.anaconda.org/conda-forge/",
                "https://repo.anaconda.com/pkgs/main/",
                "https://repo.anaconda.cloud/repo/main-x/",
                "https://repo.anaconda.com/pkgs/r/",
            ]
        );
    }

    // -- effective_channels ---------------------------------------------------

    #[test]
    fn project_channels_replace_the_defaults() {
        let policy = ChannelPolicy::new(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
        )
        .unwrap();
        let effective = policy
            .effective_channels(Some(&channels(&["conda-forge", "bioconda"])), &[])
            .unwrap();
        let urls: Vec<String> = effective
            .set
            .for_platform(Platform::Linux64)
            .into_iter()
            .map(|c| c.base_url.as_str().to_string())
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://conda.anaconda.org/conda-forge/",
                "https://conda.anaconda.org/bioconda/",
            ]
        );
    }

    #[test]
    fn an_authorized_pin_appends_its_channel() {
        let policy =
            ChannelPolicy::new(&channels(&["defaults"]), &channels(&["conda-forge"])).unwrap();
        let overrides = [ChannelOverride {
            channel: Some(&matchspec_channel("conda-forge")),
            url: None,
            context: "runtime",
        }];
        let effective = policy.effective_channels(None, &overrides).unwrap();
        let urls: Vec<String> = effective
            .set
            .for_platform(Platform::Linux64)
            .into_iter()
            .map(|c| c.base_url.as_str().to_string())
            .collect();
        assert!(urls.contains(&"https://conda.anaconda.org/conda-forge/".to_string()));
    }

    #[test]
    fn an_unauthorized_pin_is_a_violation() {
        let policy = ChannelPolicy::new(&channels(&["defaults"]), &[]).unwrap();
        let overrides = [ChannelOverride {
            channel: Some(&matchspec_channel("conda-forge")),
            url: None,
            context: "runtime",
        }];
        let err = policy.effective_channels(None, &overrides).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn every_violation_is_collected_not_just_the_first() {
        let policy = ChannelPolicy::new(&channels(&["defaults"]), &[]).unwrap();
        let overrides = [
            ChannelOverride {
                channel: Some(&matchspec_channel("conda-forge")),
                url: None,
                context: "runtime",
            },
            ChannelOverride {
                channel: Some(&matchspec_channel("bioconda")),
                url: None,
                context: "group:dev",
            },
        ];
        let err = policy.effective_channels(None, &overrides).unwrap_err();
        let Error::ChannelNotAllowed(message) = err else {
            panic!("expected ChannelNotAllowed");
        };
        assert!(message.contains("runtime"), "{message}");
        assert!(message.contains("group:dev"), "{message}");
    }

    #[test]
    fn digest_is_stable_across_spellings_that_normalize_alike() {
        let a = ChannelPolicy::new(&channels(&["main"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        let b = ChannelPolicy::new(&channels(&["https://conda.anaconda.org/main"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        let c = ChannelPolicy::new(&channels(&["https://repo.anaconda.com/pkgs/main"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(b.digest, c.digest);
    }

    #[test]
    fn digest_changes_on_reorder() {
        let a = ChannelPolicy::new(&channels(&["conda-forge", "bioconda"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        let b = ChannelPolicy::new(&channels(&["bioconda", "conda-forge"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn digest_changes_on_addition_and_removal() {
        let a = ChannelPolicy::new(&channels(&["conda-forge"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        let b = ChannelPolicy::new(&channels(&["conda-forge", "bioconda"]), &[])
            .unwrap()
            .effective_channels(None, &[])
            .unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn a_malformed_default_channels_entry_is_an_invalid_channel_error() {
        let err = ChannelPolicy::new(&channels(&["./not-a-real-channel"]), &[]).unwrap_err();
        assert!(matches!(err, Error::InvalidChannel { .. }));
    }

    #[test]
    fn a_file_scheme_allowed_channel_is_rejected() {
        let err = ChannelPolicy::new(&[], &channels(&["file:///tmp/local-channel"])).unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_url_override_falls_under_a_prefix_rule() {
        let policy =
            ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main/*"])).unwrap();
        let url =
            Url::parse("https://example.com/pkgs/main/dev/linux-64/numpy-1.0-0.conda").unwrap();
        let overrides = [ChannelOverride {
            channel: None,
            url: Some(&url),
            context: "runtime",
        }];
        let effective = policy.effective_channels(None, &overrides).unwrap();
        assert!(!effective.set.for_platform(Platform::Linux64).is_empty());
    }

    #[test]
    fn a_bare_url_override_outside_any_rule_is_a_violation() {
        let policy = ChannelPolicy::new(&channels(&["defaults"]), &[]).unwrap();
        let url = Url::parse("https://packages.evil.example/x/linux-64/numpy-1.0-0.conda").unwrap();
        let overrides = [ChannelOverride {
            channel: None,
            url: Some(&url),
            context: "runtime",
        }];
        let err = policy.effective_channels(None, &overrides).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn a_bare_url_override_without_a_subdir_layout_is_a_violation() {
        // No `<subdir>/<filename>` tail: the url names no channel at
        // all, even though it sits under the prefix string-wise.
        let policy =
            ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main/*"])).unwrap();
        let url = Url::parse("https://example.com/pkgs/main/dev/numpy-1.0-0.conda").unwrap();
        let overrides = [ChannelOverride {
            channel: None,
            url: Some(&url),
            context: "runtime",
        }];
        let err = policy.effective_channels(None, &overrides).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn defaults_channels_is_never_checked_against_the_allow_list() {
        // `default_channels` is trusted unconditionally when
        // `project_channels` is `None` -- it is itself the source of
        // rules, not something checked against them.
        let policy =
            ChannelPolicy::new(&channels(&["conda-forge"]), &channels(&["bioconda"])).unwrap();
        let effective = policy.effective_channels(None, &[]).unwrap();
        let urls: Vec<String> = effective
            .set
            .for_platform(Platform::Linux64)
            .into_iter()
            .map(|c| c.base_url.as_str().to_string())
            .collect();
        assert_eq!(urls, vec!["https://conda.anaconda.org/conda-forge/"]);
    }

    #[test]
    fn disallowed_project_channels_fails_before_any_solver_call() {
        let policy = ChannelPolicy::new(&channels(&["defaults"]), &[]).unwrap();
        let err = policy
            .effective_channels(Some(&channels(&["conda-forge"])), &[])
            .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    // -- validate_channel_entry -----------------------------------------

    #[test]
    fn validate_channel_entry_accepts_a_wildcard_in_the_allow_list() {
        assert!(validate_channel_entry(
            ChannelListPosition::AllowList,
            "https://example.com/pkgs/main/*"
        )
        .is_ok());
    }

    #[test]
    fn validate_channel_entry_rejects_a_wildcard_in_a_search_list() {
        let err = validate_channel_entry(
            ChannelListPosition::SearchList,
            "https://example.com/pkgs/main/*",
        )
        .unwrap_err();
        assert!(matches!(err, Error::WildcardNotAllowedHere { .. }));
    }

    #[test]
    fn validate_channel_entry_rejects_a_file_channel_in_either_position() {
        assert!(matches!(
            validate_channel_entry(ChannelListPosition::SearchList, "file:///tmp/x"),
            Err(Error::LocalChannelNotSupported { .. })
        ));
        assert!(matches!(
            validate_channel_entry(ChannelListPosition::AllowList, "file:///tmp/x"),
            Err(Error::LocalChannelNotSupported { .. })
        ));
    }

    #[test]
    fn validate_channel_entry_rejects_a_credentialed_url() {
        let err = validate_channel_entry(
            ChannelListPosition::AllowList,
            "https://user:pass@example.com/channel",
        )
        .unwrap_err();
        assert!(matches!(err, Error::CredentialedChannelNotSupported { .. }));
    }

    #[test]
    fn validate_channel_entry_accepts_an_ordinary_channel_in_either_position() {
        assert!(validate_channel_entry(ChannelListPosition::SearchList, "conda-forge").is_ok());
        assert!(validate_channel_entry(ChannelListPosition::AllowList, "conda-forge").is_ok());
    }
}
