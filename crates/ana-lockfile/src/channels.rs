//! Channel-policy validation: whether a project's `conda-channels`/
//! `# ana-channels:` override, and any per-package `channel::`/`url=`
//! override on a `Dependency::Matchspec` entry, are permitted -- and, if
//! so, the flat channel list a solve should run against.
//!
//! The allow-list is `default_channels ∪ allowed_channels`, canonicalized
//! and deduplicated (see [`canonicalize`]); `default_channels` is never
//! itself checked against it. A project's `conda-channels` replaces
//! `default_channels` as the solve's base list rather than merging with
//! it, but every entry must resolve into the allow-set or the call fails
//! before any network access. A per-package override is checked the same
//! way, layered on top of whichever base list applies.
//!
//! Two entries are the same channel if they canonicalize to the same
//! [`rattler_conda_types::Channel::canonical_name`], so a bare alias
//! (`conda-forge`) and its equivalent full URL are one allow-list entry.
//! The literal string `"defaults"` is compared only against other literal
//! `"defaults"` entries, never expanded to its real URL constituents.
//!
//! Every channel string is resolved with a [`ChannelConfig`] whose
//! `root_dir` is an unused placeholder: a local filesystem channel is
//! rejected outright, as [`Error::LocalChannelNotSupported`], the moment
//! it resolves to a `file://` base URL, regardless of source.
//!
//! A per-package `channel::` override needs no `ChannelConfig`:
//! `MatchSpec::from_str` already resolves it into a real `Channel` at
//! parse time, so [`spec_channel_identity`] reads it straight off
//! `spec.channel`.
//!
//! [`effective_channels`] also returns a `digest`: a fingerprint of the
//! exact ordered channel list, over each entry's *canonical* identity
//! ([`Canonical`]) rather than its literal spelling. `crate::algorithm`
//! records and compares this on a later call, so a channel-policy change
//! is detected as staleness even when every already-locked package's
//! `url` still validates against the new list, without two machines
//! whose config differs only in spelling producing different digests for
//! the same effective policy.

use std::path::PathBuf;

use rattler_conda_types::{Channel, ChannelConfig, MatchSpec, Platform, RepoDataRecord};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::error::Error;

/// The literal channel-alias string this module treats as its own
/// opaque token -- see the module docs. Re-exported so `ana_solver`
/// (which depends on this crate) can reuse it instead of hardcoding its
/// own copy.
pub const DEFAULTS_ALIAS: &str = "defaults";

/// Where a project-level violation is attributed, in
/// [`Error::ChannelNotAllowed`]'s message.
const PROJECT_CHANNELS_CONTEXT: &str = "tool.ana.conda-channels";

/// One allow-list entry's canonical identity: `"defaults"` compared
/// literally, or a resolved channel's `canonical_name()` plus its
/// `base_url` (for matching a package-URL override's prefix). Identity
/// equality ([`Canonical::identity_eq`]) is narrower than deriving
/// `PartialEq`: two `Named` entries are the same channel by
/// `canonical_name` alone, ignoring `base_url`.
#[derive(Debug, Clone)]
enum Canonical {
    Defaults,
    Named {
        canonical_name: String,
        base_url: String,
    },
}

impl Canonical {
    /// Whether `self` and `other` name the same channel: `Defaults` only
    /// matches `Defaults`; a `Named` pair matches by `canonical_name`
    /// alone.
    fn identity_eq(&self, other: &Canonical) -> bool {
        match (self, other) {
            (Canonical::Defaults, Canonical::Defaults) => true,
            (
                Canonical::Named {
                    canonical_name: a, ..
                },
                Canonical::Named {
                    canonical_name: b, ..
                },
            ) => a == b,
            _ => false,
        }
    }

    /// Whether `url` falls under this entry's base URL. Always `false`
    /// for `Defaults`, since `"defaults"` is never expanded to real URLs
    /// here.
    fn url_starts_with(&self, url: &Url) -> bool {
        match self {
            Canonical::Defaults => false,
            Canonical::Named { base_url, .. } => url.as_str().starts_with(base_url.as_str()),
        }
    }
}

/// One entry of the running result (the allow-set, or the base/result
/// channel list being built): the literal string kept in the returned
/// channel list, plus its canonical identity for de-duplication and
/// override matching.
struct ChannelEntry {
    channel: String,
    canonical: Canonical,
}

/// [`effective_channels`]'s result: the flat, ordered channel list a
/// solve should run against, plus a [`digest_of`] fingerprint of that
/// same ordered list's canonical identities -- see the module docs for
/// why the digest exists and why it is not simply the channel list
/// itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectiveChannels {
    pub(crate) channels: Vec<String>,
    pub(crate) digest: String,
}

/// A stable fingerprint of `entries`' canonical identities, in order --
/// never their literal spelling. Two entries with the same
/// [`Canonical::identity_eq`] result always feed the same bytes into the
/// hash, so a spelling difference between machines that still resolve to
/// the same channel produces the same digest.
///
/// Each field is length-prefixed and each entry tagged with a variant
/// byte, so no concatenation of two entries can collide with a different
/// split of the same bytes. This is a staleness-detection fingerprint,
/// not a security boundary, so collision resistance beyond SHA-256's own
/// is not required.
fn digest_of(entries: &[ChannelEntry]) -> String {
    let mut hasher = Sha256::new();
    for entry in entries {
        match &entry.canonical {
            Canonical::Defaults => {
                hasher.update([0u8]);
            }
            Canonical::Named {
                canonical_name,
                base_url,
            } => {
                hasher.update([1u8]);
                hasher.update((canonical_name.len() as u64).to_le_bytes());
                hasher.update(canonical_name.as_bytes());
                hasher.update((base_url.len() as u64).to_le_bytes());
                hasher.update(base_url.as_bytes());
            }
        }
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

/// Resolves one `default_channels`/`allowed_channels`/`conda-channels`
/// string to its canonical identity. `"defaults"` is recognized before
/// attempting to parse it as a channel; any other string that resolves
/// to a `file://` base URL -- a `file://` URL, or a bare absolute/`~/`
/// path -- is rejected as [`Error::LocalChannelNotSupported`], regardless
/// of which caller supplied it.
fn canonicalize(name: &str, channel_config: &ChannelConfig) -> Result<Canonical, Error> {
    if name == DEFAULTS_ALIAS {
        return Ok(Canonical::Defaults);
    }
    let channel =
        Channel::from_str(name, channel_config).map_err(|source| Error::InvalidChannel {
            name: name.to_string(),
            source,
        })?;
    if channel.base_url.as_ref().scheme() == "file" {
        return Err(Error::LocalChannelNotSupported {
            name: name.to_string(),
        });
    }
    Ok(Canonical::Named {
        canonical_name: channel.canonical_name(),
        base_url: channel.base_url.as_str().to_string(),
    })
}

/// The canonical identity of an already-resolved `channel::` override.
/// `channel.name` retains the alias text `MatchSpec::from_str` resolved
/// it from, so a `defaults::<pkg>` override is recognized as the literal
/// `"defaults"` token without re-parsing anything.
fn spec_channel_identity(channel: &Channel) -> Canonical {
    if channel.name.as_deref() == Some(DEFAULTS_ALIAS) {
        Canonical::Defaults
    } else {
        Canonical::Named {
            canonical_name: channel.canonical_name(),
            base_url: channel.base_url.as_str().to_string(),
        }
    }
}

/// Pushes `channel`/`canonical` onto `entries` unless an entry with the
/// same canonical identity is already present. Always an ordered `Vec`,
/// never a `HashSet`: channel order feeds
/// `rattler_solve::ChannelPriority::Strict`, so reordering it would make
/// solves non-deterministic.
fn push_if_new(entries: &mut Vec<ChannelEntry>, channel: &str, canonical: Canonical) {
    if !entries
        .iter()
        .any(|entry| entry.canonical.identity_eq(&canonical))
    {
        entries.push(ChannelEntry {
            channel: channel.to_string(),
            canonical,
        });
    }
}

/// Validates `project_channels` (if the project declares an override)
/// and every per-package `channel::`/`url=` override in
/// `matchspec_entries` against `default_channels ∪ allowed_channels`,
/// then returns the flat, ordered channel list a solve should run
/// against, plus its [`digest_of`] fingerprint (see [`EffectiveChannels`]).
///
/// `matchspec_entries` is `crate::matchspec::matchspec_entries`'s output:
/// every `Dependency::Matchspec` entry the caller selected. Base channels
/// come first, then overrides in `matchspec_entries`'s order, with
/// duplicates dropped (see [`push_if_new`]).
///
/// Every violation is collected into one [`Error::ChannelNotAllowed`]
/// rather than failing on the first. A malformed channel string in
/// `default_channels`/`allowed_channels`/`conda-channels` fails fast
/// instead, as [`Error::InvalidChannel`].
pub(crate) fn effective_channels(
    default_channels: &[String],
    allowed_channels: &[String],
    project_channels: Option<&[String]>,
    matchspec_entries: &[(String, String, MatchSpec, String)],
) -> Result<EffectiveChannels, Error> {
    // `root_dir` is never consulted for any channel this module actually
    // canonicalizes -- see the module docs.
    let channel_config = ChannelConfig::default_with_root_dir(PathBuf::new());

    let mut allow_set: Vec<ChannelEntry> =
        Vec::with_capacity(default_channels.len() + allowed_channels.len());
    for name in default_channels.iter().chain(allowed_channels.iter()) {
        let canonical = canonicalize(name, &channel_config)?;
        push_if_new(&mut allow_set, name, canonical);
    }

    let mut violations: Vec<String> = Vec::new();
    let mut result: Vec<ChannelEntry> = Vec::new();

    match project_channels {
        Some(list) => {
            for name in list {
                let canonical = canonicalize(name, &channel_config)?;
                if allow_set
                    .iter()
                    .any(|entry| entry.canonical.identity_eq(&canonical))
                {
                    push_if_new(&mut result, name, canonical);
                } else {
                    violations.push(format!(
                        "  {name:?} (from {PROJECT_CHANNELS_CONTEXT}): not in \
                         default_channels/allowed_channels"
                    ));
                }
            }
        }
        // `default_channels` is trusted unconditionally, never checked
        // against the allow-set; still canonicalized, so a malformed
        // entry still surfaces as `Error::InvalidChannel`.
        None => {
            for name in default_channels {
                let canonical = canonicalize(name, &channel_config)?;
                push_if_new(&mut result, name, canonical);
            }
        }
    }

    for (_, canonical_spec, spec, source) in matchspec_entries {
        if let Some(channel) = &spec.channel {
            let override_identity = spec_channel_identity(channel);
            match allow_set
                .iter()
                .find(|entry| entry.canonical.identity_eq(&override_identity))
            {
                Some(matched) => {
                    push_if_new(&mut result, &matched.channel, matched.canonical.clone());
                }
                None => violations.push(format!(
                    "  channel {:?} (from {source}, {canonical_spec:?}): not in \
                     default_channels/allowed_channels",
                    channel.canonical_name()
                )),
            }
        } else if let Some(url) = &spec.url {
            match allow_set
                .iter()
                .find(|entry| entry.canonical.url_starts_with(url))
            {
                Some(matched) => {
                    push_if_new(&mut result, &matched.channel, matched.canonical.clone());
                }
                None => violations.push(format!(
                    "  url {:?} (from {source}, {canonical_spec:?}): does not fall under any \
                     allowed channel",
                    url.as_str()
                )),
            }
        }
    }

    if !violations.is_empty() {
        return Err(Error::ChannelNotAllowed(violations.join("\n")));
    }

    let digest = digest_of(&result);
    Ok(EffectiveChannels {
        channels: result.into_iter().map(|entry| entry.channel).collect(),
        digest,
    })
}

/// The base URL Anaconda's own `defaults` meta-channel expands to --
/// `https://repo.anaconda.com/pkgs/<name>`, one real channel per
/// constituent named by [`defaults_subchannels`]. Re-exported so
/// `ana_solver::channels` shares this as the single source of truth --
/// see [`DEFAULTS_ALIAS`].
pub const DEFAULTS_BASE_URL: &str = "https://repo.anaconda.com/pkgs";

/// The constituent channel names `"defaults"` expands to for `platform`,
/// in `conda`'s own default priority order: `main` and `r`
/// unconditionally, plus `msys2` last, only on Windows. Re-exported for
/// `ana_solver::channels::resolve` (an actual solve's per-platform
/// expansion) to share -- see [`DEFAULTS_ALIAS`].
///
/// [`validate_locked_packages`] has no `Platform` to check against, so
/// it deliberately calls this with [`Platform::Win64`] to get the full
/// superset of constituents regardless of platform -- sound either way,
/// since a non-Windows section's packages were never fetched from
/// `pkgs/msys2` in practice.
pub fn defaults_subchannels(platform: Platform) -> &'static [&'static str] {
    if platform.is_windows() {
        &["main", "r", "msys2"]
    } else {
        &["main", "r"]
    }
}

/// Validates that every one of `packages`' `url` falls under one of
/// `channels` -- the exact, already-authorized list [`effective_channels`]
/// just returned for this same call. `crate::algorithm`'s `Fresh`/`Valid`
/// fast paths run this before trusting an already-locked
/// [`crate::lock_file::PlatformSection`] without a real solve, since
/// `effective_channels` alone only validates *declared* overrides, never
/// what actually ended up in a previous solve's `packages`.
///
/// Unlike `effective_channels`'s declaration-level check, the literal
/// `"defaults"` token is expanded to its real constituent URLs here (see
/// [`defaults_subchannels`]): a locked package's `url` is a concrete
/// fetch location, not a channel-name string.
///
/// Checks `url`, never `channel`: `RepoDataRecord::channel` is
/// free-text/informational, never consulted by anything that decides
/// where a package is actually fetched from.
///
/// Every violation is collected into one [`Error::ChannelNotAllowed`],
/// same as [`effective_channels`].
pub(crate) fn validate_locked_packages(
    channels: &[String],
    packages: &[RepoDataRecord],
) -> Result<(), Error> {
    let channel_config = ChannelConfig::default_with_root_dir(PathBuf::new());

    let mut base_urls: Vec<String> = Vec::with_capacity(channels.len());
    for name in channels {
        if name == DEFAULTS_ALIAS {
            for subchannel in defaults_subchannels(Platform::Win64) {
                base_urls.push(format!("{DEFAULTS_BASE_URL}/{subchannel}/"));
            }
        } else {
            let channel = Channel::from_str(name, &channel_config).map_err(|source| {
                Error::InvalidChannel {
                    name: name.to_string(),
                    source,
                }
            })?;
            base_urls.push(channel.base_url.as_str().to_string());
        }
    }

    let violations: Vec<String> = packages
        .iter()
        .filter(|package| {
            !base_urls
                .iter()
                .any(|base| package.url.as_str().starts_with(base.as_str()))
        })
        .map(|package| {
            format!(
                "  {:?} (locked package {:?}): url does not fall under any allowed channel",
                package.url.as_str(),
                package.package_record.name.as_normalized(),
            )
        })
        .collect();

    if !violations.is_empty() {
        return Err(Error::ChannelNotAllowed(violations.join("\n")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use rattler_conda_types::ParseMatchSpecOptions;
    use std::str::FromStr;

    use super::*;

    fn channels(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    /// One `(name, canonical, spec, source)` entry, as
    /// `crate::matchspec::matchspec_entries` produces it, from a literal
    /// matchspec string with a `"runtime"` source.
    fn matchspec_entry(spec_text: &str) -> (String, String, MatchSpec, String) {
        matchspec_entry_with_source(spec_text, "runtime")
    }

    fn matchspec_entry_with_source(
        spec_text: &str,
        source: &str,
    ) -> (String, String, MatchSpec, String) {
        let spec = MatchSpec::from_str(
            spec_text,
            ParseMatchSpecOptions::lenient().with_extras(true),
        )
        .unwrap();
        let canonical = spec.to_string();
        let name = spec
            .name
            .as_exact()
            .map(|n| n.as_normalized().to_string())
            .unwrap_or_else(|| canonical.clone());
        (name, canonical, spec, source.to_string())
    }

    #[test]
    fn no_overrides_passes_default_channels_through_unchanged() {
        let result =
            effective_channels(&channels(&["conda-forge", "defaults"]), &[], None, &[]).unwrap();
        assert_eq!(result.channels, channels(&["conda-forge", "defaults"]));
    }

    #[test]
    fn default_channels_is_never_checked_against_allowed_channels() {
        let result = effective_channels(
            &channels(&["conda-forge"]),
            &channels(&["bioconda"]),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(result.channels, channels(&["conda-forge"]));
    }

    #[test]
    fn allowed_channel_override_is_added_to_default_channels() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
        )
        .unwrap();
        assert_eq!(result.channels, channels(&["defaults", "conda-forge"]));
    }

    #[test]
    fn disallowed_channel_override_fails() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let err = effective_channels(&channels(&["defaults"]), &[], None, &entries).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn allowed_bare_url_override_adds_its_matched_channel_not_the_raw_url() {
        let entries = [matchspec_entry(
            "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.26.0-py311h1234567_0.conda",
        )];
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
        )
        .unwrap();
        assert_eq!(
            result.channels,
            channels(&["defaults", "conda-forge"]),
            "the allow-set's own spelling is pushed, not the raw package URL"
        );
    }

    #[test]
    fn disallowed_bare_url_override_fails() {
        let entries = [matchspec_entry(
            "https://repo.mycompany.com/conda/linux-64/mypkg-1.0-0.conda",
        )];
        let err = effective_channels(&channels(&["defaults"]), &[], None, &entries).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn a_url_under_an_unrelated_channels_prefix_does_not_false_positive_match() {
        // `ChannelUrl::as_str()` always ends in `/`, so `conda-forge`'s
        // base URL is not a string prefix of `conda-forge-extra`'s.
        let entries = [matchspec_entry(
            "https://conda.anaconda.org/conda-forge-extra/linux-64/mypkg-1.0-0.conda",
        )];
        let err = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn allowed_project_channels_replaces_default_channels() {
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            Some(&channels(&["conda-forge", "bioconda"])),
            &[],
        )
        .unwrap();
        assert_eq!(
            result.channels,
            channels(&["conda-forge", "bioconda"]),
            "the project's own list replaces default_channels entirely, it does not merge"
        );
    }

    #[test]
    fn disallowed_project_channels_fails_before_any_solver_call() {
        let err = effective_channels(
            &channels(&["defaults"]),
            &[],
            Some(&channels(&["conda-forge"])),
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn project_channels_combines_with_a_further_per_package_override() {
        let entries = [matchspec_entry("bioconda::samtools")];
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            Some(&channels(&["conda-forge"])),
            &entries,
        )
        .unwrap();
        assert_eq!(result.channels, channels(&["conda-forge", "bioconda"]));
    }

    #[test]
    fn a_bare_alias_and_its_equivalent_url_are_the_same_allow_list_entry() {
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["https://conda.anaconda.org/conda-forge"]),
            Some(&channels(&["conda-forge"])),
            &[],
        )
        .unwrap();
        assert_eq!(result.channels, channels(&["conda-forge"]));
    }

    #[test]
    fn base_channels_come_first_then_overrides_in_spec_order_with_no_duplicates() {
        let entries = [
            // Duplicates a base entry -- must not be repeated or reordered.
            matchspec_entry("conda-forge::pkg-a"),
            matchspec_entry_with_source("bioconda::pkg-b", "group:dev"),
        ];
        let result = effective_channels(
            &channels(&["conda-forge", "defaults"]),
            &channels(&["bioconda"]),
            None,
            &entries,
        )
        .unwrap();
        assert_eq!(
            result.channels,
            channels(&["conda-forge", "defaults", "bioconda"])
        );
    }

    #[test]
    fn two_overrides_naming_the_same_new_channel_contribute_one_entry() {
        let entries = [
            matchspec_entry("bioconda::pkg-a"),
            matchspec_entry_with_source("bioconda::pkg-b", "group:dev"),
        ];
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["bioconda"]),
            None,
            &entries,
        )
        .unwrap();
        assert_eq!(result.channels, channels(&["defaults", "bioconda"]));
    }

    #[test]
    fn every_violation_is_collected_not_just_the_first() {
        let entries = [
            matchspec_entry_with_source("conda-forge::pkg-a", "runtime"),
            matchspec_entry_with_source("bioconda::pkg-b", "group:dev"),
        ];
        let err = effective_channels(&channels(&["defaults"]), &[], None, &entries).unwrap_err();
        let Error::ChannelNotAllowed(message) = err else {
            panic!("expected ChannelNotAllowed");
        };
        assert!(message.contains("runtime"), "{message}");
        assert!(message.contains("group:dev"), "{message}");
    }

    #[test]
    fn a_malformed_default_channels_entry_is_an_invalid_channel_error() {
        // An empty (non-absolute) configured root dir guarantees this
        // relative-path-shaped string fails to resolve, regardless of
        // host platform.
        let err =
            effective_channels(&channels(&["./not-a-real-channel"]), &[], None, &[]).unwrap_err();
        assert!(matches!(err, Error::InvalidChannel { .. }));
    }

    #[test]
    fn a_file_scheme_project_channel_is_rejected_as_a_local_channel() {
        let err = effective_channels(
            &channels(&["defaults"]),
            &[],
            Some(&channels(&["file:///tmp/local-channel"])),
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_absolute_path_project_channel_is_rejected_as_a_local_channel() {
        // A bare absolute path (no `file://` scheme) resolves to the
        // same `file://` base URL rattler would use for an explicit
        // `file://` URL.
        let err = effective_channels(
            &channels(&["defaults"]),
            &[],
            Some(&channels(&["/tmp/local-channel"])),
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_absolute_path_default_channel_is_rejected_as_a_local_channel() {
        let err =
            effective_channels(&channels(&["/tmp/local-channel"]), &[], None, &[]).unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn zero_width_space_appended_to_defaults_does_not_match_the_defaults_alias() {
        // Falls through to `Channel::from_str` (not the literal
        // `DEFAULTS_ALIAS` token) and resolves to its own, unrelated
        // channel URL.
        let err = effective_channels(
            &channels(&["defaults"]),
            &[],
            Some(&["defaults\u{200B}".to_string()]),
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)), "{err:?}");
    }

    #[test]
    fn zero_width_space_inside_an_otherwise_allowed_alias_is_a_distinct_channel() {
        let entries = [matchspec_entry("cond\u{200B}a-forge::numpy")];
        let err = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)), "{err:?}");
    }

    #[test]
    fn bidi_override_appended_to_an_allowed_alias_does_not_match_it() {
        // The "Trojan Source" trick: a trailing right-to-left-override
        // character makes a terminal/log render the string differently
        // than its actual bytes, but byte-wise equality still treats it
        // as distinct from `"conda-forge"`.
        let entries = [matchspec_entry("conda-forge\u{202E}::numpy")];
        let err = effective_channels(&channels(&["defaults"]), &[], None, &entries).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)), "{err:?}");
    }

    #[test]
    fn a_case_variant_of_an_allowed_channel_is_not_silently_accepted() {
        // Neither this module nor `Channel::from_str`/`canonical_name`
        // case-folds a bare alias, so this is a correctness footgun for
        // an operator who *means* the same channel, never a bypass: the
        // failure mode is always "rejected", never "silently let through".
        let result = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            Some(&channels(&["Conda-Forge"])),
            &[],
        );
        assert!(
            matches!(result, Err(Error::ChannelNotAllowed(_))),
            "{result:?}"
        );
    }

    fn locked_package(name: &str, url: &str) -> RepoDataRecord {
        let record = rattler_conda_types::PackageRecord::new(
            rattler_conda_types::PackageName::new_unchecked(name),
            rattler_conda_types::Version::from_str("1.0.0").unwrap(),
            "0".to_string(),
        );
        let identifier = rattler_conda_types::package::DistArchiveIdentifier::try_from_filename(
            &format!("{name}-1.0.0-0.conda"),
        )
        .unwrap();
        RepoDataRecord {
            package_record: record,
            identifier,
            url: Url::parse(url).unwrap(),
            channel: None,
        }
    }

    #[test]
    fn a_package_url_under_an_allowed_channel_passes() {
        let packages = [locked_package(
            "numpy",
            "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0.0-0.conda",
        )];
        assert!(validate_locked_packages(&channels(&["conda-forge"]), &packages).is_ok());
    }

    #[test]
    fn a_package_url_under_a_disallowed_channel_fails() {
        let packages = [locked_package(
            "numpy",
            "https://packages.evil-corp.example/channel/linux-64/numpy-1.0.0-0.conda",
        )];
        let err = validate_locked_packages(&channels(&["conda-forge"]), &packages).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn every_violating_package_is_collected_not_just_the_first() {
        let packages = [
            locked_package(
                "numpy",
                "https://packages.evil-corp.example/channel/linux-64/numpy-1.0.0-0.conda",
            ),
            locked_package(
                "scipy",
                "https://packages.also-evil.example/channel/linux-64/scipy-1.0.0-0.conda",
            ),
        ];
        let err = validate_locked_packages(&channels(&["conda-forge"]), &packages).unwrap_err();
        let Error::ChannelNotAllowed(message) = err else {
            panic!("expected ChannelNotAllowed");
        };
        assert!(message.contains("numpy"), "{message}");
        assert!(message.contains("scipy"), "{message}");
    }

    #[test]
    fn the_literal_defaults_token_expands_to_its_real_url_constituents() {
        // A real solve using `"defaults"` fetches from exactly this host
        // (see `ana_solver::channels`), so a package legitimately solved
        // against it must not be flagged as a violation.
        let packages = [locked_package(
            "numpy",
            "https://repo.anaconda.com/pkgs/main/linux-64/numpy-1.0.0-0.conda",
        )];
        assert!(validate_locked_packages(&channels(&["defaults"]), &packages).is_ok());
    }

    #[test]
    fn a_url_that_only_resembles_a_defaults_constituent_still_fails() {
        let packages = [locked_package(
            "numpy",
            "https://repo.anaconda.com.evil-corp.example/pkgs/main/linux-64/numpy-1.0.0-0.conda",
        )];
        let err = validate_locked_packages(&channels(&["defaults"]), &packages).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn an_unrelated_channel_prefix_does_not_false_positive_match() {
        let packages = [locked_package(
            "numpy",
            "https://conda.anaconda.org/conda-forge-extra/linux-64/numpy-1.0.0-0.conda",
        )];
        let err = validate_locked_packages(&channels(&["conda-forge"]), &packages).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn an_empty_package_list_always_passes() {
        assert!(validate_locked_packages(&channels(&["conda-forge"]), &[]).is_ok());
    }

    #[test]
    fn digest_is_deterministic_for_the_same_inputs() {
        let a =
            effective_channels(&channels(&["conda-forge", "bioconda"]), &[], None, &[]).unwrap();
        let b =
            effective_channels(&channels(&["conda-forge", "bioconda"]), &[], None, &[]).unwrap();
        assert_eq!(a.digest, b.digest);
    }

    #[test]
    fn digest_changes_when_default_channels_is_reordered() {
        let a =
            effective_channels(&channels(&["conda-forge", "bioconda"]), &[], None, &[]).unwrap();
        let b =
            effective_channels(&channels(&["bioconda", "conda-forge"]), &[], None, &[]).unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn digest_changes_when_default_channels_gains_a_channel() {
        let a = effective_channels(&channels(&["conda-forge"]), &[], None, &[]).unwrap();
        let b =
            effective_channels(&channels(&["conda-forge", "bioconda"]), &[], None, &[]).unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn digest_changes_when_default_channels_loses_a_channel() {
        let a =
            effective_channels(&channels(&["conda-forge", "bioconda"]), &[], None, &[]).unwrap();
        let b = effective_channels(&channels(&["conda-forge"]), &[], None, &[]).unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn digest_differs_for_a_genuinely_different_per_package_override_channel() {
        let via_conda_forge = [matchspec_entry("conda-forge::numpy")];
        let via_bioconda = [matchspec_entry("bioconda::numpy")];
        let a = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            None,
            &via_conda_forge,
        )
        .unwrap();
        let b = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            None,
            &via_bioconda,
        )
        .unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn pinning_project_conda_channels_makes_the_digest_independent_of_default_channels() {
        let dev_a = effective_channels(
            &channels(&["defaults", "conda-forge", "bioconda"]),
            &[],
            Some(&channels(&["conda-forge", "bioconda"])),
            &[],
        )
        .unwrap();
        let dev_b = effective_channels(
            &channels(&["bioconda"]),
            &channels(&["conda-forge", "defaults", "some-internal-mirror"]),
            Some(&channels(&["conda-forge", "bioconda"])),
            &[],
        )
        .unwrap();
        assert_eq!(dev_a.channels, dev_b.channels);
        assert_eq!(dev_a.digest, dev_b.digest);
    }

    #[test]
    fn a_per_package_overrides_digest_is_independent_of_the_admin_configs_literal_spelling() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let dev_a = effective_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
        )
        .unwrap();
        let dev_b = effective_channels(
            &channels(&["defaults"]),
            &channels(&["https://conda.anaconda.org/conda-forge"]),
            None,
            &entries,
        )
        .unwrap();
        assert_ne!(
            dev_a.channels, dev_b.channels,
            "the literal spelling pushed into the returned channel list still differs"
        );
        assert_eq!(
            dev_a.digest, dev_b.digest,
            "same canonical channel, different literal spelling: must not look like drift"
        );
    }
}
