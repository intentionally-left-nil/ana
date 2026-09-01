//! Channel identity for `ana`: exactly three places in the workspace know
//! anything about channel identity, and all three live here.
//!
//! 1. [`normalize_channel`] -- the only code that produces a canonical
//!    channel URL. Every [`rattler_conda_types::Channel`] anywhere in
//!    `ana` has been through it.
//! 2. [`ChannelPolicy`] -- the only code that compares a channel URL
//!    against the configured channel set.
//! 3. [`trusted_channel`] and [`artifact_channel`] -- the only code that
//!    gives an already-solved/locked package a channel identity: the
//!    former decides whether a package's own `channel` field can be
//!    trusted at all (by cross-checking it against that same package's
//!    `url`), the latter derives a channel from a bare artifact URL's
//!    `<channel>/<subdir>/<filename>` layout.
//!
//! Everything else holds already-normalized values and asks the policy
//! yes/no questions: `ana-dependency` calls [`normalize_channel`] inside
//! `parse_matchspec`, the sole constructor of a `MatchSpec` in `ana`;
//! `ana-lockfile` and `ana::sandbox` both hold a `&ChannelPolicy` and
//! resolve a locked package's channel via [`trusted_channel`] (or
//! [`artifact_channel`], for a package `trusted_channel` doesn't vouch
//! for) before asking it a yes/no question; nothing else in the
//! workspace constructs a [`rattler_conda_types::Channel`] from a
//! name/URL string or reaches into `rattler_redaction` directly
//! (enforced by this crate's own `tests::guardrail` module).
//!
//! A channel's identity is its [`rattler_conda_types::ChannelUrl`] -- see
//! [`normalize_channel`]'s module docs for why that's sound.
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod alias;
mod error;
mod normalize;
mod policy;

pub use error::{CredentialOffense, Error};
pub use normalize::normalize_channel;
pub use policy::{
    artifact_channel, trusted_channel, validate_channel_entry, ChannelListPosition,
    ChannelOverride, ChannelPolicy, ChannelSet, EffectiveChannels,
};

#[cfg(test)]
mod guardrail {
    //! Keeps the two pinch points ([`crate::normalize_channel`] and
    //! [`crate::ChannelPolicy`]) from multiplying: no crate other than
    //! this one may construct a [`rattler_conda_types::Channel`] from a
    //! name/URL string, or reach into `rattler_redaction` directly.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::Path;

    /// Symbols only this crate is allowed to mention. A hit inside this
    /// crate's own `src/` is expected and skipped; a hit anywhere else in
    /// the workspace fails the test.
    const FORBIDDEN: &[&str] = &[
        "ChannelConfig",
        "Channel::from_str",
        "Channel::from_url",
        "rattler_redaction",
    ];

    #[test]
    fn only_ana_channels_constructs_channels_or_touches_rattler_redaction() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crates/ana-channels has a workspace root two levels up")
            .to_path_buf();
        let crates_dir = workspace_root.join("crates");
        let this_crate_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

        let mut violations = Vec::new();
        for entry in walkdir::WalkDir::new(&crates_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            if path.starts_with(&this_crate_src) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for (line_no, line) in text.lines().enumerate() {
                for symbol in FORBIDDEN {
                    if line.contains(symbol) {
                        violations.push(format!(
                            "{}:{}: mentions {symbol:?}",
                            path.display(),
                            line_no + 1
                        ));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "only ana-channels may construct a Channel from a name/URL or touch \
             rattler_redaction directly:\n{}",
            violations.join("\n")
        );
    }
}
