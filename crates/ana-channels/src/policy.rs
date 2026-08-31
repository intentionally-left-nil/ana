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

use rattler_conda_types::{Channel, ChannelConfig, ChannelUrl, Platform};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::alias::meta_channel_members;
use crate::error::Error;
use crate::normalize::{normalize_channel, parse_alias_url};

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

    fn authorizes_artifact(&self, url: &Url) -> bool {
        match self {
            Rule::Exact(exact) => url.as_str().starts_with(exact.as_str()),
            Rule::Prefix(prefix) => url.as_str().starts_with(prefix.as_str()),
        }
    }

    /// The channel to add to a solve's channel list when an artifact `url`
    /// falls under this rule -- the allow-set's own channel, never the raw
    /// artifact URL. `None` when `url` does not fall under this rule.
    fn matched_channel_for_artifact(&self, url: &Url) -> Option<Channel> {
        if !self.authorizes_artifact(url) {
            return None;
        }
        match self {
            Rule::Exact(exact) => Some(Channel::from_url(exact.clone())),
            // A prefix rule authorizes a whole family of channels, not
            // one -- the closest real channel identity is the artifact's
            // own conventional `<channel>/<subdir>/<filename>` layout,
            // with the last two path segments (subdir and filename)
            // stripped back off.
            Rule::Prefix(_) => Some(Channel::from_url(artifact_channel_base(url))),
        }
    }
}

/// The channel base URL implied by a repodata-style artifact URL
/// (`<channel>/<subdir>/<filename>`): `url` with its query, fragment, and
/// last two path segments removed.
fn artifact_channel_base(url: &Url) -> Url {
    let mut base = url.clone();
    base.set_query(None);
    base.set_fragment(None);
    if let Some(segments) = base.path_segments() {
        let mut kept: Vec<&str> = segments.collect();
        kept.truncate(kept.len().saturating_sub(2));
        let path = format!("/{}/", kept.join("/"));
        base.set_path(&path);
    }
    base
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
    pub fn authorizes_channel(&self, url: &ChannelUrl) -> bool {
        self.rules.iter().any(|rule| rule.authorizes_channel(url))
    }

    /// Whether artifact `url` is authorized: it must lie under an exact
    /// rule's channel URL or under a prefix rule's prefix. Used for a bare
    /// package-URL dependency, which names an artifact rather than a
    /// channel.
    pub fn authorizes_artifact(&self, url: &Url) -> bool {
        self.rules.iter().any(|rule| rule.authorizes_artifact(url))
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
                match self
                    .rules
                    .iter()
                    .find_map(|rule| rule.matched_channel_for_artifact(url))
                {
                    Some(matched) => set.push_if_new(matched, false),
                    None => violations.push(format!(
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

    use rattler_conda_types::Platform;

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

    // -- authorizes_channel / authorizes_artifact -------------------------

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
    fn authorizes_artifact_for_an_exact_rule() {
        let policy = ChannelPolicy::new(&[], &channels(&["conda-forge"])).unwrap();
        let url = Url::parse("https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0-0.conda")
            .unwrap();
        assert!(policy.authorizes_artifact(&url));
        let other =
            Url::parse("https://packages.evil.example/x/linux-64/numpy-1.0-0.conda").unwrap();
        assert!(!policy.authorizes_artifact(&other));
    }

    #[test]
    fn authorizes_artifact_for_a_prefix_rule() {
        let policy =
            ChannelPolicy::new(&[], &channels(&["https://example.com/pkgs/main/*"])).unwrap();
        let url =
            Url::parse("https://example.com/pkgs/main/dev/linux-64/numpy-1.0-0.conda").unwrap();
        assert!(policy.authorizes_artifact(&url));
        let outside =
            Url::parse("https://example.com/pkgs/mainline/linux-64/numpy-1.0-0.conda").unwrap();
        assert!(!policy.authorizes_artifact(&outside));
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
