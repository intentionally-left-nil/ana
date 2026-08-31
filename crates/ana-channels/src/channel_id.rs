//! [`ChannelId`]: a channel's identity resolved once, from its
//! declared spelling (a bare alias, a `pkgs/<name>` shorthand, or a full
//! URL) to the single, unredacted [`Url`] that spelling denotes.
//!
//! Two spellings are the same channel exactly when they resolve to the
//! same `ChannelId` -- see [`ChannelId`]'s own docs for the equality
//! rule. This is the module every other piece of channel policy
//! (allow-listing, per-spec restriction, locked-package validation)
//! builds on, so that "is this channel allowed" is asked and answered
//! about the same identity everywhere, rather than each site
//! canonicalizing on its own.
//!
//! # The alias table
//!
//! Forward (name -> URL), applied case-sensitively, byte-exact -- no
//! entry here is configurable:
//!
//! | Name | Expands to |
//! |---|---|
//! | `defaults` | `pkgs/main`, `pkgs/r`, and `pkgs/msys2` **on Windows only** -- list-position only, see [`expand_list_entry`] |
//! | `main`, `pkgs/main` | `https://repo.anaconda.com/pkgs/main/` |
//! | `r`, `pkgs/r` | `https://repo.anaconda.com/pkgs/r/` |
//! | `msys2`, `pkgs/msys2` | `https://repo.anaconda.com/pkgs/msys2/` |
//! | any other bare name | `https://conda.anaconda.org/<name>/` |
//! | anything URL-shaped | itself, normalized to a trailing `/` |
//!
//! Reverse (URL -> display name), in [`display`]: the short name **only
//! if** `forward(name) == this_url`; otherwise the redacted URL. This
//! round-trip check is required for injectivity --
//! `https://conda.anaconda.org/main/` must render as the full URL, never
//! as `main`, or an error message would read "`main` is not allowed"
//! when `main` *is* allowed.
#![allow(clippy::redundant_clone)] // `Url` clones below are all structural, not perf-sensitive.

use std::sync::LazyLock;

use rattler_conda_types::{Channel, ChannelConfig, Platform};
use rattler_redaction::Redact;
use url::Url;

use crate::error::Error;

/// One channel's identity: the single, unredacted [`Url`] its declared
/// spelling resolves to, always ending in `/`.
///
/// Equality and [`std::hash::Hash`] compare the full URL byte-for-byte,
/// unredacted: two `/t/<token>/` channels differing only in token are
/// **distinct** channels (each is its own real, independently
/// authorizable location), and comparison never depends on how a
/// caller happened to spell the channel.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ChannelId(Url);

impl ChannelId {
    /// Wraps an already-resolved base URL, normalizing it to end in `/`
    /// and rejecting `file://` (local filesystem channels are not
    /// supported, regardless of source -- see [`Error::LocalChannelNotSupported`]).
    fn new(mut url: Url, name: &str) -> Result<Self, Error> {
        if url.scheme() == "file" {
            return Err(Error::LocalChannelNotSupported {
                name: name.to_string(),
            });
        }
        if !url.path().ends_with('/') {
            let path = format!("{}/", url.path());
            url.set_path(&path);
        }
        Ok(Self(url))
    }

    /// The unredacted URL this channel identifies -- e.g. for
    /// `SolverTask::excluded_candidates`' prefix check against a
    /// fetched record's own `url`.
    pub fn as_url(&self) -> &Url {
        &self.0
    }

    /// Whether `url` falls under this channel's base URL -- a plain
    /// prefix check on the unredacted URL, since [`ChannelId::new`]
    /// guarantees a trailing `/` on `self`, so `conda-forge`'s base URL
    /// is never a string prefix of `conda-forge-extra`'s.
    pub fn contains_url(&self, url: &Url) -> bool {
        url.as_str().starts_with(self.0.as_str())
    }
}

/// The literal channel-alias string [`expand_list_entry`] treats as its
/// own opaque list-position token -- legal only in a *channel list*
/// (`default_channels`, `allowed_channels`, project `conda-channels`),
/// never as a matchspec qualifier (see [`resolve_qualifier`]).
pub const DEFAULTS_ALIAS: &str = "defaults";

static MAIN_URL: LazyLock<Url> =
    LazyLock::new(|| must_parse("https://repo.anaconda.com/pkgs/main/"));
static R_URL: LazyLock<Url> = LazyLock::new(|| must_parse("https://repo.anaconda.com/pkgs/r/"));
static MSYS2_URL: LazyLock<Url> =
    LazyLock::new(|| must_parse("https://repo.anaconda.com/pkgs/msys2/"));

/// Parses a URL this module hardcodes itself (never external input).
/// Panicking here would mean a typo in this file's own constant, not a
/// runtime failure any caller could hit -- the one place in this module
/// exempt from the "no unwrap/expect outside tests" rule, since there is
/// no `Result` to propagate to before `main` even starts.
#[allow(clippy::unwrap_used)]
fn must_parse(url: &str) -> Url {
    Url::parse(url).unwrap()
}

/// One list entry's expansion (`default_channels`/`allowed_channels`/
/// project `conda-channels`): `defaults` legal, expanding in `conda`'s
/// own priority order to `main` and `r` unconditionally, plus `msys2`
/// last, only on `platform`'s Windows subdirs. Every other entry
/// expands to exactly one [`ChannelId`] (see the module docs' alias
/// table).
pub fn expand_list_entry(name: &str, platform: Platform) -> Result<Vec<ChannelId>, Error> {
    if name == DEFAULTS_ALIAS {
        let mut ids = vec![
            ChannelId::new(MAIN_URL.clone(), name)?,
            ChannelId::new(R_URL.clone(), name)?,
        ];
        if platform.is_windows() {
            ids.push(ChannelId::new(MSYS2_URL.clone(), name)?);
        }
        return Ok(ids);
    }
    Ok(vec![ChannelId::new(forward(name)?, name)?])
}

/// One matchspec qualifier's resolution (a `channel::`/`channel=`
/// override): unlike [`expand_list_entry`], `defaults` is **illegal**
/// here -- a matchspec qualifier names exactly one channel, and
/// `defaults` names a set -- so it is rejected as
/// [`Error::DefaultsQualifier`], a dedicated error carrying the
/// `main::`/`r::` suggestion, rather than falling through to the
/// generic "not allowed" a caller would otherwise report.
pub fn resolve_qualifier(name: &str) -> Result<ChannelId, Error> {
    if name == DEFAULTS_ALIAS {
        return Err(Error::DefaultsQualifier);
    }
    ChannelId::new(forward(name)?, name)
}

/// The display name for `id`: the short alias name if (and only if) it
/// round-trips back to `id`'s own URL through [`forward`], otherwise the
/// redacted URL -- see the module docs' reverse-alias rule. Never the
/// literal `"defaults"` token: a `ChannelId` is always one concrete
/// channel, and `["defaults"]` displays as `main, r` (its constituents,
/// each rendered by this function), never as `defaults` itself.
pub fn display(id: &ChannelId) -> String {
    if !is_token_prefixed(&id.0) {
        for candidate in reverse_candidates(&id.0) {
            if forward(&candidate).is_ok_and(|url| url == id.0) {
                return candidate;
            }
        }
    }
    id.0.clone().redact().to_string()
}

/// Whether `url`'s path starts with a `/t/<token>/` segment pair -- the
/// same shape `rattler_redaction` itself masks. A tokened URL is never a
/// bare alias name a caller would type (the token is only ever embedded
/// in a full URL), so it is excluded from [`reverse_candidates`]
/// entirely rather than risk stripping the alias prefix and
/// coincidentally round-tripping the token straight into a "display
/// name".
fn is_token_prefixed(url: &Url) -> bool {
    url.path_segments().is_some_and(|mut segments| {
        matches!((segments.next(), segments.next()), (Some("t"), Some(_)))
    })
}

/// Every short name whose forward expansion *might* equal `url`, cheapest
/// first -- [`display`] still round-trips each one through [`forward`]
/// before trusting it, so a false candidate here only costs one extra
/// comparison, never a wrong answer.
fn reverse_candidates(url: &Url) -> Vec<String> {
    let mut candidates = Vec::new();
    if url == &*MAIN_URL {
        candidates.push("main".to_string());
    }
    if url == &*R_URL {
        candidates.push("r".to_string());
    }
    if url == &*MSYS2_URL {
        candidates.push("msys2".to_string());
    }
    const ALIAS_PREFIX: &str = "https://conda.anaconda.org/";
    if let Some(rest) = url.as_str().strip_prefix(ALIAS_PREFIX) {
        let name = rest.trim_end_matches('/');
        if !name.is_empty() {
            candidates.push(name.to_string());
        }
    }
    candidates
}

/// Resolves one channel spelling to its base [`Url`], per the module
/// docs' alias table: the three hardcoded `repo.anaconda.com/pkgs/*`
/// aliases first (case-sensitive, byte-exact -- a case or lookalike-
/// character variant falls through to the generic branch below and
/// resolves to its own, unrelated URL), then `rattler_conda_types`'s own
/// generic resolution for everything else (a bare name against the
/// `https://conda.anaconda.org` channel alias, or a URL/path parsed
/// directly) -- which already normalizes a trailing `/`, and resolves a
/// bare absolute/`~/` path to a `file://` URL rather than rejecting it
/// outright; [`ChannelId::new`] is what actually rejects any `file://`
/// URL this function returns, whether it came from that path handling or
/// an explicit `file://` spelling.
fn forward(name: &str) -> Result<Url, Error> {
    match name {
        "main" | "pkgs/main" => return Ok(MAIN_URL.clone()),
        "r" | "pkgs/r" => return Ok(R_URL.clone()),
        "msys2" | "pkgs/msys2" => return Ok(MSYS2_URL.clone()),
        _ => {}
    }
    let channel_config = ChannelConfig::default_with_root_dir(std::path::PathBuf::new());
    let channel =
        Channel::from_str(name, &channel_config).map_err(|source| Error::InvalidChannel {
            name: name.to_string(),
            source,
        })?;
    Ok(channel.base_url.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn id(url: &str) -> ChannelId {
        ChannelId::new(Url::parse(url).unwrap(), "test").unwrap()
    }

    #[test]
    fn main_alias_expands_to_the_real_pkgs_main_url() {
        assert_eq!(
            resolve_qualifier("main").unwrap(),
            id("https://repo.anaconda.com/pkgs/main/")
        );
        assert_eq!(
            resolve_qualifier("pkgs/main").unwrap(),
            id("https://repo.anaconda.com/pkgs/main/")
        );
    }

    #[test]
    fn r_alias_expands_to_the_real_pkgs_r_url() {
        assert_eq!(
            resolve_qualifier("r").unwrap(),
            id("https://repo.anaconda.com/pkgs/r/")
        );
        assert_eq!(
            resolve_qualifier("pkgs/r").unwrap(),
            id("https://repo.anaconda.com/pkgs/r/")
        );
    }

    #[test]
    fn msys2_alias_expands_to_the_real_pkgs_msys2_url() {
        assert_eq!(
            resolve_qualifier("msys2").unwrap(),
            id("https://repo.anaconda.com/pkgs/msys2/")
        );
        assert_eq!(
            resolve_qualifier("pkgs/msys2").unwrap(),
            id("https://repo.anaconda.com/pkgs/msys2/")
        );
    }

    #[test]
    fn any_other_bare_name_expands_to_conda_anaconda_org() {
        assert_eq!(
            resolve_qualifier("conda-forge").unwrap(),
            id("https://conda.anaconda.org/conda-forge/")
        );
    }

    #[test]
    fn a_url_shaped_entry_resolves_to_itself_normalized() {
        assert_eq!(
            resolve_qualifier("https://repo.mycompany.com/conda").unwrap(),
            id("https://repo.mycompany.com/conda/")
        );
        assert_eq!(
            resolve_qualifier("https://repo.mycompany.com/conda/").unwrap(),
            id("https://repo.mycompany.com/conda/")
        );
    }

    #[test]
    fn defaults_expands_to_two_entries_off_windows_in_order() {
        let ids = expand_list_entry("defaults", Platform::Linux64).unwrap();
        assert_eq!(
            ids,
            vec![
                id("https://repo.anaconda.com/pkgs/main/"),
                id("https://repo.anaconda.com/pkgs/r/"),
            ]
        );
    }

    #[test]
    fn defaults_expands_to_three_entries_on_windows_in_order() {
        let ids = expand_list_entry("defaults", Platform::Win64).unwrap();
        assert_eq!(
            ids,
            vec![
                id("https://repo.anaconda.com/pkgs/main/"),
                id("https://repo.anaconda.com/pkgs/r/"),
                id("https://repo.anaconda.com/pkgs/msys2/"),
            ]
        );
    }

    #[test]
    fn main_pkgs_main_and_its_full_url_are_the_identical_channel_id() {
        let a = expand_list_entry("main", Platform::Linux64).unwrap();
        let b = expand_list_entry("pkgs/main", Platform::Linux64).unwrap();
        let c =
            expand_list_entry("https://repo.anaconda.com/pkgs/main", Platform::Linux64).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn conda_anaconda_org_main_is_a_different_channel_id_from_main() {
        let main = resolve_qualifier("main").unwrap();
        let conda_org_main = resolve_qualifier("https://conda.anaconda.org/main").unwrap();
        assert_ne!(main, conda_org_main);
    }

    #[test]
    fn conda_anaconda_org_main_displays_as_the_full_url_not_main() {
        let conda_org_main = resolve_qualifier("https://conda.anaconda.org/main").unwrap();
        assert_eq!(display(&conda_org_main), "https://conda.anaconda.org/main/");
    }

    #[test]
    fn main_displays_as_main_not_the_full_url() {
        let main = resolve_qualifier("main").unwrap();
        assert_eq!(display(&main), "main");
    }

    #[test]
    fn a_bare_alias_displays_as_its_short_name() {
        let cf = resolve_qualifier("conda-forge").unwrap();
        assert_eq!(display(&cf), "conda-forge");
    }

    #[test]
    fn resolve_qualifier_rejects_defaults_with_a_dedicated_error() {
        let err = resolve_qualifier("defaults").unwrap_err();
        assert!(matches!(err, Error::DefaultsQualifier), "{err:?}");
    }

    #[test]
    fn file_scheme_is_rejected() {
        let err = resolve_qualifier("file:///tmp/local-channel").unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_absolute_path_is_rejected() {
        let err = resolve_qualifier("/tmp/local-channel").unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn a_bare_tilde_path_is_rejected() {
        let err = resolve_qualifier("~/local-channel").unwrap_err();
        assert!(matches!(err, Error::LocalChannelNotSupported { .. }));
    }

    #[test]
    fn zero_width_space_appended_to_defaults_does_not_match_the_alias() {
        // Falls through to generic resolution and resolves to its own,
        // unrelated channel URL rather than expanding as `defaults`.
        let ids = expand_list_entry("defaults\u{200B}", Platform::Linux64).unwrap();
        assert_eq!(ids.len(), 1);
        assert_ne!(ids[0], id("https://repo.anaconda.com/pkgs/main/"));
    }

    #[test]
    fn zero_width_space_inside_conda_forge_is_a_distinct_channel() {
        let a = resolve_qualifier("conda-forge").unwrap();
        let b = resolve_qualifier("cond\u{200B}a-forge").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn bidi_override_appended_to_conda_forge_does_not_match_it() {
        // The "Trojan Source" trick: a trailing right-to-left-override
        // character makes a terminal/log render the string differently
        // than its actual bytes, but byte-wise equality still treats it
        // as distinct from `"conda-forge"`.
        let a = resolve_qualifier("conda-forge").unwrap();
        let b = resolve_qualifier("conda-forge\u{202E}").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_case_variant_of_an_alias_is_a_distinct_channel() {
        // Neither the hardcoded alias match nor `Channel::from_str` case-
        // folds a bare alias, so `Conda-Forge` resolves to its own,
        // distinct (and almost certainly wrong) URL rather than silently
        // matching `conda-forge`.
        let a = resolve_qualifier("conda-forge").unwrap();
        let b = resolve_qualifier("Conda-Forge").unwrap();
        assert_ne!(a, b);

        let main = resolve_qualifier("main").unwrap();
        let capital_main = resolve_qualifier("Main").unwrap();
        assert_ne!(main, capital_main);
    }

    #[test]
    fn two_tokened_urls_differing_only_in_token_are_distinct_and_display_redacted() {
        let a = resolve_qualifier("https://conda.anaconda.org/t/token-a/conda-forge").unwrap();
        let b = resolve_qualifier("https://conda.anaconda.org/t/token-b/conda-forge").unwrap();
        assert_ne!(a, b);
        assert_eq!(
            display(&a),
            "https://conda.anaconda.org/t/********/conda-forge/"
        );
        assert_eq!(
            display(&b),
            "https://conda.anaconda.org/t/********/conda-forge/"
        );
    }

    #[test]
    fn an_invalid_channel_name_is_a_parse_error() {
        // An empty (non-absolute) configured root dir guarantees this
        // relative-path-shaped string fails to resolve, regardless of
        // host platform.
        let err = resolve_qualifier("./not-a-real-channel").unwrap_err();
        assert!(matches!(err, Error::InvalidChannel { .. }));
    }
}
