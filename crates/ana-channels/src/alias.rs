//! Static compiled data: the channel-alias table
//! ([`normalize_channel`](crate::normalize_channel)'s alias-table hit) and
//! the meta-channel table (what `"defaults"` expands to). Naming is kept
//! separate from grouping, so a name ([`AliasEntry`]) is addressable
//! without being part of any [`META_CHANNELS`] group.

/// One channel [`normalize_channel`](crate::normalize_channel) resolves a
/// bare name (or the equivalent generic `conda.anaconda.org` URL) to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AliasEntry {
    pub name: &'static str,
    pub url: &'static str,
}

const MAIN: AliasEntry = AliasEntry {
    name: "main",
    url: "https://repo.anaconda.com/pkgs/main",
};
const R: AliasEntry = AliasEntry {
    name: "r",
    url: "https://repo.anaconda.com/pkgs/r",
};
const MSYS2: AliasEntry = AliasEntry {
    name: "msys2",
    url: "https://repo.anaconda.com/pkgs/msys2",
};
/// Not in [`ALIASES`]: `conda.anaconda.org/main-x` is an unrelated
/// anaconda.org channel, so only the explicit URL (or `"defaults"`
/// membership) ever resolves here.
const MAIN_X: AliasEntry = AliasEntry {
    name: "main-x",
    url: "https://repo.anaconda.cloud/repo/main-x",
};

/// name -> canonical location. The only channels
/// [`normalize_channel`](crate::normalize_channel) rewrites a bare name or
/// generic-alias URL to; anything else (e.g. `conda-forge`) passes through
/// to `conda.anaconda.org` unchanged.
pub(crate) const ALIASES: &[AliasEntry] = &[MAIN, R, MSYS2];

/// One member of a meta-channel ([`META_CHANNELS`]): an [`ALIASES`] entry,
/// plus the platforms it applies to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MetaMember {
    pub alias: AliasEntry,
    /// Whether this member applies only on Windows (`msys2`) or on every
    /// platform (`main`, `main-x`, `r`).
    pub windows_only: bool,
}

/// meta-channel -> member names, with the platforms each applies to.
/// `"defaults"` is Anaconda's own classic meta-channel: `main`, `main-x`,
/// and `r` on every platform, plus `msys2` on Windows only, in conda's own
/// priority order.
pub(crate) const META_CHANNELS: &[(&str, &[MetaMember])] = &[(
    "defaults",
    &[
        MetaMember {
            alias: MAIN,
            windows_only: false,
        },
        MetaMember {
            alias: MAIN_X,
            windows_only: false,
        },
        MetaMember {
            alias: R,
            windows_only: false,
        },
        MetaMember {
            alias: MSYS2,
            windows_only: true,
        },
    ],
)];

/// The members `name` expands to, if it names a meta-channel.
pub(crate) fn meta_channel_members(name: &str) -> Option<&'static [MetaMember]> {
    META_CHANNELS
        .iter()
        .find(|(meta_name, _)| *meta_name == name)
        .map(|(_, members)| *members)
}
