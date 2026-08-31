//! Legacy Anaconda-hosted channel aliases: three historical hostnames
//! that share credentials with the `anaconda.com` keyring domain.
//! Compiled in, never user-configurable -- see the plan's "Domain
//! resolution" scope decision for why this is a fixed table rather than
//! `.condarc`/config-driven.

/// `(legacy host, keyring domain)` pairs. Every legacy host below is a
/// distinct hostname historically used for the same Anaconda-hosted
/// channel content that `anaconda.com` credentials authenticate today.
const LEGACY_HOST_ALIASES: &[(&str, &str)] = &[
    ("repo.anaconda.cloud", "anaconda.com"),
    ("repo.anaconda.com", "anaconda.com"),
    ("repo.continuum.io", "anaconda.com"),
];

/// The keyring domain a request `host` should be looked up under: the
/// shared alias target if `host` is one of the three historical
/// Anaconda-hosted repo hostnames, otherwise `host` itself (the "custom
/// channel == its own domain" case that covers every other real-world
/// channel, PSM cloud/on-prem included).
pub fn resolve_domain(host: &str) -> &str {
    LEGACY_HOST_ALIASES
        .iter()
        .find(|(alias, _)| *alias == host)
        .map_or(host, |(_, domain)| domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_aliases_resolve_to_the_shared_anaconda_com_domain() {
        assert_eq!(resolve_domain("repo.anaconda.cloud"), "anaconda.com");
        assert_eq!(resolve_domain("repo.anaconda.com"), "anaconda.com");
        assert_eq!(resolve_domain("repo.continuum.io"), "anaconda.com");
    }

    #[test]
    fn a_host_with_no_alias_resolves_to_itself() {
        assert_eq!(
            resolve_domain("repo.mycompany.com"),
            "repo.mycompany.com",
            "a custom channel's own host is its own keyring domain"
        );
        assert_eq!(resolve_domain("conda.anaconda.org"), "conda.anaconda.org");
    }
}
