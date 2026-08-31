//! Channel-policy validation: whether a project's `conda-channels`/
//! `# ana-channels:` override, and any per-package `channel::`/`url=`
//! override on a [`MatchspecEntry`], are permitted -- and, if so, the
//! flat, ordered [`ChannelId`] list a solve should run against.
//!
//! The allow-set is `expand(default_channels) ∪ expand(allowed_channels)`
//! (see [`channel_id::expand_list_entry`]), deduplicated on [`ChannelId`];
//! `default_channels` is never itself checked against it. A project's
//! `conda-channels` replaces `default_channels` as the solve's base list
//! rather than merging with it, but every [`ChannelId`] each entry
//! expands to must be in the allow-set or the call fails before any
//! network access. A per-package override is checked the same way,
//! layered on top of whichever base list applies -- a `channel::`/
//! `channel=` qualifier via [`channel_id::resolve_qualifier`] (which
//! rejects `"defaults"` outright: a qualifier names one channel, not a
//! set), a `url=` override via [`ChannelId::contains_url`]'s prefix
//! check.
//!
//! [`resolve_channels`] is the sole constructor of [`AuthorizedChannels`],
//! so "every entry in this list was actually authorized" is a type
//! invariant, not a convention every call site has to uphold on its own.
//! It also computes a `digest`: a fingerprint of the exact ordered
//! [`ChannelId`] list. `ana-lockfile`'s algorithm records and compares
//! this on a later call, so a channel-policy change is detected as
//! staleness even when every already-locked package's `url` still
//! validates against the new list, without two machines whose config
//! differs only in spelling (`main` vs. its full URL) producing
//! different digests for the same effective policy.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod channel_id;
mod error;

use std::collections::HashSet;

use ana_matchspec_convert::MatchspecEntry;
use rattler_conda_types::{Platform, RepoDataRecord};
use sha2::{Digest as _, Sha256};
use url::Url;

pub use channel_id::{display, expand_list_entry, resolve_qualifier, ChannelId, DEFAULTS_ALIAS};
pub use error::Error;

/// Where a project-level violation is attributed, in
/// [`Error::ChannelNotAllowed`]'s message.
const PROJECT_CHANNELS_CONTEXT: &str = "tool.ana.conda-channels";

/// The channel allow-set resolved for one platform's solve: an ordered,
/// deduplicated [`ChannelId`] list every entry of which was actually
/// authorized against `default_channels ∪ allowed_channels`, plus a
/// [`AuthorizedChannels::digest`] fingerprint of that same list.
///
/// Constructible only by [`resolve_channels`] -- there is no public way
/// to build one from an arbitrary channel list, so a caller holding an
/// `AuthorizedChannels` has a type-level guarantee its contents already
/// passed policy, rather than a convention every call site has to
/// remember to uphold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedChannels {
    channels: Vec<ChannelId>,
    platform: Platform,
    digest: String,
}

impl AuthorizedChannels {
    /// The flat, ordered channel list a solve should run against: base
    /// channels first, then per-package overrides in
    /// [`resolve_channels`]'s `matchspec_entries` order, deduplicated.
    pub fn channels(&self) -> &[ChannelId] {
        &self.channels
    }

    /// The platform this list was resolved for -- `defaults`' expansion
    /// (and so the list itself, and its digest) depends on it.
    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// A stable fingerprint of [`Self::channels`], in order. Two
    /// `AuthorizedChannels` with the same digest have the identical
    /// ordered `ChannelId` list, regardless of the literal spelling
    /// (`main` vs. its full URL) that produced each one.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Whether `url` falls under any channel in this list -- the single
    /// check both a `url=` override and a locked package's own `url`
    /// (see [`validate_locked_packages`]) are validated with.
    pub fn contains_url(&self, url: &Url) -> bool {
        self.channels
            .iter()
            .any(|channel| channel.contains_url(url))
    }
}

/// A stable fingerprint of `channels`, in order: each entry contributes
/// its length-prefixed unredacted URL bytes, so no concatenation of two
/// entries can collide with a different split of the same bytes. This is
/// a staleness-detection fingerprint, not a security boundary, so
/// collision resistance beyond SHA-256's own is not required.
fn compute_digest(channels: &[ChannelId]) -> String {
    let mut hasher = Sha256::new();
    for channel in channels {
        let bytes = channel.as_url().as_str().as_bytes();
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

/// Pushes `id` onto `channels` unless it (by [`ChannelId`] equality) is
/// already present. Always an ordered `Vec`, never a `HashSet`: channel
/// order feeds `rattler_solve::ChannelPriority::Flexible`, so reordering
/// it would make solves non-deterministic.
fn push_if_new(channels: &mut Vec<ChannelId>, id: ChannelId) {
    if !channels.contains(&id) {
        channels.push(id);
    }
}

/// Validates `project_channels` (if the project declares an override)
/// and every per-package `channel::`/`url=` override in
/// `matchspec_entries` against
/// `expand(default_channels) ∪ expand(allowed_channels)` for `platform`,
/// then returns the flat, ordered [`AuthorizedChannels`] a solve should
/// run against.
///
/// `matchspec_entries` is `ana_matchspec_convert::matchspec_entries`'s
/// output: every `Dependency::Matchspec` entry the caller selected. Base
/// channels come first, then overrides in `matchspec_entries`'s order,
/// with duplicates dropped (see [`push_if_new`]).
///
/// Every allow-set violation is collected into one
/// [`Error::ChannelNotAllowed`] rather than failing on the first. A
/// malformed channel string, a local-filesystem channel, or a `defaults`
/// matchspec qualifier fails fast instead -- these are typos or
/// programmer errors, not policy questions with more than one answer
/// worth reporting together.
pub fn resolve_channels(
    default_channels: &[String],
    allowed_channels: &[String],
    project_channels: Option<&[String]>,
    matchspec_entries: &[MatchspecEntry],
    platform: Platform,
) -> Result<AuthorizedChannels, Error> {
    let mut allow_set: HashSet<ChannelId> = HashSet::new();
    for name in default_channels.iter().chain(allowed_channels.iter()) {
        for id in channel_id::expand_list_entry(name, platform)? {
            allow_set.insert(id);
        }
    }

    let mut violations: Vec<String> = Vec::new();
    let mut channels: Vec<ChannelId> = Vec::new();

    match project_channels {
        Some(list) => {
            for name in list {
                let expanded = channel_id::expand_list_entry(name, platform)?;
                if expanded.iter().all(|id| allow_set.contains(id)) {
                    for id in expanded {
                        push_if_new(&mut channels, id);
                    }
                } else {
                    violations.push(format!(
                        "  {name:?} (from {PROJECT_CHANNELS_CONTEXT}): not in \
                         default_channels/allowed_channels"
                    ));
                }
            }
        }
        // `default_channels` is trusted unconditionally, never checked
        // against the allow-set; still expanded, so a malformed entry
        // still surfaces as `Error::InvalidChannel`.
        None => {
            for name in default_channels {
                for id in channel_id::expand_list_entry(name, platform)? {
                    push_if_new(&mut channels, id);
                }
            }
        }
    }

    for entry in matchspec_entries {
        if let Some(qualifier) = &entry.qualifier {
            let id = channel_id::resolve_qualifier(qualifier)?;
            if allow_set.contains(&id) {
                push_if_new(&mut channels, id);
            } else {
                violations.push(format!(
                    "  channel {qualifier:?} (from {}, {:?}): not in \
                     default_channels/allowed_channels",
                    entry.source, entry.canonical
                ));
            }
        } else if let Some(url) = &entry.spec.url {
            match allow_set.iter().find(|id| id.contains_url(url)) {
                Some(id) => push_if_new(&mut channels, id.clone()),
                None => violations.push(format!(
                    "  url {:?} (from {}, {:?}): does not fall under any allowed channel",
                    url.as_str(),
                    entry.source,
                    entry.canonical
                )),
            }
        }
    }

    if !violations.is_empty() {
        return Err(Error::ChannelNotAllowed(violations.join("\n")));
    }

    let digest = compute_digest(&channels);
    Ok(AuthorizedChannels {
        channels,
        platform,
        digest,
    })
}

/// Validates that every one of `packages`' `url` falls under one of
/// `channels` -- the exact, already-authorized [`AuthorizedChannels`]
/// [`resolve_channels`] just returned for this same platform.
/// `crate::algorithm`'s `Fresh`/`Valid` fast paths run this before
/// trusting an already-locked `PlatformSection` without a real solve,
/// since [`resolve_channels`] alone only validates *declared* overrides,
/// never what actually ended up in a previous solve's `packages`.
///
/// Checks `url`, never `channel`: `RepoDataRecord::channel` is
/// free-text/informational, never consulted by anything that decides
/// where a package is actually fetched from.
///
/// Every violation is collected into one [`Error::ChannelNotAllowed`],
/// same as [`resolve_channels`].
pub fn validate_locked_packages(
    channels: &AuthorizedChannels,
    packages: &[RepoDataRecord],
) -> Result<(), Error> {
    let violations: Vec<String> = packages
        .iter()
        .filter(|package| !channels.contains_url(&package.url))
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

    use std::str::FromStr;

    use super::*;

    fn channels(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    /// One [`MatchspecEntry`], as `ana_matchspec_convert::matchspec_entries`
    /// produces it, from a literal matchspec string with a `"runtime"`
    /// source.
    fn matchspec_entry(spec_text: &str) -> MatchspecEntry {
        matchspec_entry_with_source(spec_text, "runtime")
    }

    fn matchspec_entry_with_source(spec_text: &str, source: &str) -> MatchspecEntry {
        let dep = ana_dependency::parse_matchspec(spec_text).unwrap();
        let canonical = dep.spec.to_string();
        let name = dep
            .spec
            .name
            .as_exact()
            .map(|n| n.as_normalized().to_string())
            .unwrap_or_else(|| canonical.clone());
        MatchspecEntry {
            name,
            canonical,
            spec: dep.spec,
            qualifier: dep.qualifier,
            source: source.to_string(),
        }
    }

    fn ids(values: &[&str]) -> Vec<ChannelId> {
        values
            .iter()
            .flat_map(|name| expand_list_entry(name, Platform::Linux64).unwrap())
            .collect()
    }

    #[test]
    fn no_overrides_passes_default_channels_through_unchanged() {
        let result = resolve_channels(
            &channels(&["conda-forge", "main"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels(), ids(&["conda-forge", "main"]).as_slice());
    }

    #[test]
    fn default_channels_is_never_checked_against_allowed_channels() {
        let result = resolve_channels(
            &channels(&["conda-forge"]),
            &channels(&["bioconda"]),
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels(), ids(&["conda-forge"]).as_slice());
    }

    #[test]
    fn allowed_channel_override_is_added_to_default_channels() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(
            result.channels(),
            ids(&["defaults", "conda-forge"]).as_slice()
        );
    }

    #[test]
    fn disallowed_channel_override_fails() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn a_defaults_matchspec_qualifier_is_the_dedicated_error_not_a_generic_not_allowed() {
        let entries = [matchspec_entry("defaults::conda")];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::DefaultsQualifier), "{err:?}");
    }

    #[test]
    fn main_qualifier_is_authorized_under_default_channels_defaults() {
        let entries = [matchspec_entry("main::conda")];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels(), ids(&["defaults"]).as_slice());
    }

    #[test]
    fn r_qualifier_is_authorized_under_default_channels_defaults() {
        let entries = [matchspec_entry("r::r-base")];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels(), ids(&["defaults"]).as_slice());
    }

    #[test]
    fn msys2_qualifier_is_authorized_only_on_windows() {
        let entries = [matchspec_entry("msys2::m2-base")];
        let linux = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(linux, Error::ChannelNotAllowed(_)));

        let windows = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Win64,
        )
        .unwrap();
        assert_eq!(
            windows.channels(),
            expand_list_entry("defaults", Platform::Win64)
                .unwrap()
                .as_slice()
        );
    }

    #[test]
    fn default_channels_defaults_plus_main_yields_exactly_two_entries() {
        let result = resolve_channels(
            &channels(&["defaults", "main"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels().len(), 2, "{:?}", result.channels());
        assert_eq!(result.channels(), ids(&["defaults"]).as_slice());
    }

    #[test]
    fn url_override_under_the_defaults_expansion_is_accepted() {
        let entries = [matchspec_entry(
            "https://repo.anaconda.com/pkgs/main/linux-64/x-1.0-0.conda",
        )];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels(), ids(&["defaults"]).as_slice());
    }

    #[test]
    fn url_override_that_only_resembles_a_defaults_constituent_is_rejected() {
        let entries = [matchspec_entry(
            "https://repo.anaconda.com/pkgs/main-evil/linux-64/x-1.0-0.conda",
        )];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn a_conda_forge_qualifier_appends_after_the_defaults_constituents() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        let mut expected = ids(&["defaults"]);
        expected.extend(ids(&["conda-forge"]));
        assert_eq!(result.channels(), expected.as_slice());
    }

    #[test]
    fn a_url_under_an_unrelated_channels_prefix_does_not_false_positive_match() {
        let entries = [matchspec_entry(
            "https://conda.anaconda.org/conda-forge-extra/linux-64/mypkg-1.0-0.conda",
        )];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn allowed_project_channels_replaces_default_channels() {
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            Some(&channels(&["conda-forge", "bioconda"])),
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(
            result.channels(),
            ids(&["conda-forge", "bioconda"]).as_slice(),
            "the project's own list replaces default_channels entirely, it does not merge"
        );
    }

    #[test]
    fn disallowed_project_channels_fails_before_any_solver_call() {
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            Some(&channels(&["conda-forge"])),
            &[],
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn project_channels_combines_with_a_further_per_package_override() {
        let entries = [matchspec_entry("bioconda::samtools")];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            Some(&channels(&["conda-forge"])),
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        let mut expected = ids(&["conda-forge"]);
        expected.extend(ids(&["bioconda"]));
        assert_eq!(result.channels(), expected.as_slice());
    }

    #[test]
    fn a_bare_alias_and_its_equivalent_url_are_the_same_allow_list_entry() {
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["https://conda.anaconda.org/conda-forge"]),
            Some(&channels(&["conda-forge"])),
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(result.channels(), ids(&["conda-forge"]).as_slice());
    }

    #[test]
    fn base_channels_come_first_then_overrides_in_spec_order_with_no_duplicates() {
        let entries = [
            // Duplicates a base entry -- must not be repeated or reordered.
            matchspec_entry("conda-forge::pkg-a"),
            matchspec_entry_with_source("bioconda::pkg-b", "group:dev"),
        ];
        let result = resolve_channels(
            &channels(&["conda-forge", "defaults"]),
            &channels(&["bioconda"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        let mut expected = ids(&["conda-forge", "defaults"]);
        expected.extend(ids(&["bioconda"]));
        assert_eq!(result.channels(), expected.as_slice());
    }

    #[test]
    fn two_overrides_naming_the_same_new_channel_contribute_one_entry() {
        let entries = [
            matchspec_entry("bioconda::pkg-a"),
            matchspec_entry_with_source("bioconda::pkg-b", "group:dev"),
        ];
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["bioconda"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        let mut expected = ids(&["defaults"]);
        expected.extend(ids(&["bioconda"]));
        assert_eq!(result.channels(), expected.as_slice());
    }

    #[test]
    fn every_violation_is_collected_not_just_the_first() {
        let entries = [
            matchspec_entry_with_source("conda-forge::pkg-a", "runtime"),
            matchspec_entry_with_source("bioconda::pkg-b", "group:dev"),
        ];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
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
        let err = resolve_channels(
            &channels(&["./not-a-real-channel"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidChannel { .. }));
    }

    #[test]
    fn a_file_scheme_project_channel_is_rejected_as_a_local_channel() {
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            Some(&channels(&["file:///tmp/local-channel"])),
            &[],
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_absolute_path_project_channel_is_rejected_as_a_local_channel() {
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            Some(&channels(&["/tmp/local-channel"])),
            &[],
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_absolute_path_default_channel_is_rejected_as_a_local_channel() {
        let err = resolve_channels(
            &channels(&["/tmp/local-channel"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn zero_width_space_appended_to_defaults_does_not_match_the_defaults_alias() {
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            Some(&["defaults\u{200B}".to_string()]),
            &[],
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)), "{err:?}");
    }

    #[test]
    fn zero_width_space_inside_an_otherwise_allowed_alias_is_a_distinct_channel() {
        let entries = [matchspec_entry("cond\u{200B}a-forge::numpy")];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)), "{err:?}");
    }

    #[test]
    fn bidi_override_appended_to_an_allowed_alias_does_not_match_it() {
        let entries = [matchspec_entry("conda-forge\u{202E}::numpy")];
        let err = resolve_channels(
            &channels(&["defaults"]),
            &[],
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)), "{err:?}");
    }

    #[test]
    fn a_case_variant_of_an_allowed_channel_is_not_silently_accepted() {
        let result = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            Some(&channels(&["Conda-Forge"])),
            &[],
            Platform::Linux64,
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
        let authorized = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let packages = [locked_package(
            "numpy",
            "https://conda.anaconda.org/conda-forge/linux-64/numpy-1.0.0-0.conda",
        )];
        assert!(validate_locked_packages(&authorized, &packages).is_ok());
    }

    #[test]
    fn a_package_url_under_a_disallowed_channel_fails() {
        let authorized = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let packages = [locked_package(
            "numpy",
            "https://packages.evil-corp.example/channel/linux-64/numpy-1.0.0-0.conda",
        )];
        let err = validate_locked_packages(&authorized, &packages).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn every_violating_package_is_collected_not_just_the_first() {
        let authorized = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
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
        let err = validate_locked_packages(&authorized, &packages).unwrap_err();
        let Error::ChannelNotAllowed(message) = err else {
            panic!("expected ChannelNotAllowed");
        };
        assert!(message.contains("numpy"), "{message}");
        assert!(message.contains("scipy"), "{message}");
    }

    #[test]
    fn the_literal_defaults_token_expands_to_its_real_url_constituents() {
        let authorized =
            resolve_channels(&channels(&["defaults"]), &[], None, &[], Platform::Linux64).unwrap();
        let packages = [locked_package(
            "numpy",
            "https://repo.anaconda.com/pkgs/main/linux-64/numpy-1.0.0-0.conda",
        )];
        assert!(validate_locked_packages(&authorized, &packages).is_ok());
    }

    #[test]
    fn a_url_that_only_resembles_a_defaults_constituent_still_fails() {
        let authorized =
            resolve_channels(&channels(&["defaults"]), &[], None, &[], Platform::Linux64).unwrap();
        let packages = [locked_package(
            "numpy",
            "https://repo.anaconda.com.evil-corp.example/pkgs/main/linux-64/numpy-1.0.0-0.conda",
        )];
        let err = validate_locked_packages(&authorized, &packages).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn an_unrelated_channel_prefix_does_not_false_positive_match() {
        let authorized = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let packages = [locked_package(
            "numpy",
            "https://conda.anaconda.org/conda-forge-extra/linux-64/numpy-1.0.0-0.conda",
        )];
        let err = validate_locked_packages(&authorized, &packages).unwrap_err();
        assert!(matches!(err, Error::ChannelNotAllowed(_)));
    }

    #[test]
    fn an_empty_package_list_always_passes() {
        let authorized = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert!(validate_locked_packages(&authorized, &[]).is_ok());
    }

    #[test]
    fn digest_is_deterministic_for_the_same_inputs() {
        let a = resolve_channels(
            &channels(&["conda-forge", "bioconda"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let b = resolve_channels(
            &channels(&["conda-forge", "bioconda"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn digest_changes_when_default_channels_is_reordered() {
        let a = resolve_channels(
            &channels(&["conda-forge", "bioconda"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let b = resolve_channels(
            &channels(&["bioconda", "conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn digest_changes_when_default_channels_gains_a_channel() {
        let a = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let b = resolve_channels(
            &channels(&["conda-forge", "bioconda"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn digest_changes_when_default_channels_loses_a_channel() {
        let a = resolve_channels(
            &channels(&["conda-forge", "bioconda"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let b = resolve_channels(
            &channels(&["conda-forge"]),
            &[],
            None,
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn digest_differs_for_a_genuinely_different_per_package_override_channel() {
        let via_conda_forge = [matchspec_entry("conda-forge::numpy")];
        let via_bioconda = [matchspec_entry("bioconda::numpy")];
        let a = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            None,
            &via_conda_forge,
            Platform::Linux64,
        )
        .unwrap();
        let b = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge", "bioconda"]),
            None,
            &via_bioconda,
            Platform::Linux64,
        )
        .unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn digest_differs_between_windows_and_linux_for_defaults() {
        let linux =
            resolve_channels(&channels(&["defaults"]), &[], None, &[], Platform::Linux64).unwrap();
        let windows =
            resolve_channels(&channels(&["defaults"]), &[], None, &[], Platform::Win64).unwrap();
        assert_ne!(linux.digest(), windows.digest());
    }

    #[test]
    fn pinning_project_conda_channels_makes_the_digest_independent_of_default_channels() {
        let dev_a = resolve_channels(
            &channels(&["defaults", "conda-forge", "bioconda"]),
            &[],
            Some(&channels(&["conda-forge", "bioconda"])),
            &[],
            Platform::Linux64,
        )
        .unwrap();
        let dev_b = resolve_channels(
            &channels(&["bioconda"]),
            &channels(&["conda-forge", "defaults", "some-internal-mirror"]),
            Some(&channels(&["conda-forge", "bioconda"])),
            &[],
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(dev_a.channels(), dev_b.channels());
        assert_eq!(dev_a.digest(), dev_b.digest());
    }

    #[test]
    fn a_per_package_overrides_digest_is_independent_of_the_admin_configs_literal_spelling() {
        let entries = [matchspec_entry("conda-forge::numpy")];
        let dev_a = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["conda-forge"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        let dev_b = resolve_channels(
            &channels(&["defaults"]),
            &channels(&["https://conda.anaconda.org/conda-forge"]),
            None,
            &entries,
            Platform::Linux64,
        )
        .unwrap();
        assert_eq!(
            dev_a.digest(),
            dev_b.digest(),
            "same canonical channel, different literal spelling: must not look like drift"
        );
    }
}
