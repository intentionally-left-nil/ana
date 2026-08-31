//! [`KeyringBackend`]: a read-only
//! [`rattler_networking::authentication_storage::StorageBackend`] backed
//! by the parsed `~/.anaconda/keyring` file. `store`/`delete` are no-ops
//! -- `ana` never writes this file back.

use rattler_networking::authentication_storage::authentication::Authentication;
use rattler_networking::authentication_storage::{AuthenticationStorageError, StorageBackend};

use crate::keyring::ParsedKeyring;
use crate::resolve::resolve_domain;

pub struct KeyringBackend {
    keyring: ParsedKeyring,
}

/// Redacted via [`ParsedKeyring`]'s own `Debug`: domains only, never
/// the API keys.
impl std::fmt::Debug for KeyringBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringBackend")
            .field("keyring", &self.keyring)
            .finish()
    }
}

impl KeyringBackend {
    pub fn new(keyring: ParsedKeyring) -> Self {
        Self { keyring }
    }
}

impl StorageBackend for KeyringBackend {
    fn name(&self) -> String {
        "anaconda keyring".to_string()
    }

    /// No-op: `ana` only reads `~/.anaconda/keyring`, never writes it.
    fn store(
        &self,
        _host: &str,
        _authentication: &Authentication,
    ) -> Result<(), AuthenticationStorageError> {
        Ok(())
    }

    fn get(&self, host: &str) -> Result<Option<Authentication>, AuthenticationStorageError> {
        let domain = resolve_domain(host);
        Ok(self
            .keyring
            .api_key(domain)
            .map(|api_key| Authentication::BearerToken(api_key.to_string())))
    }

    /// No-op: `ana` only reads `~/.anaconda/keyring`, never writes it.
    fn delete(&self, _host: &str) -> Result<(), AuthenticationStorageError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashMap;

    use super::*;

    fn backend(entries: &[(&str, &str)]) -> KeyringBackend {
        // `ParsedKeyring` has no public constructor from a map (only
        // `keyring::load` produces one from real file bytes), so this
        // reaches into the crate's own module directly -- valid since
        // this test lives in the same crate.
        let mut api_keys = HashMap::new();
        for (domain, key) in entries {
            api_keys.insert(domain.to_string(), key.to_string());
        }
        KeyringBackend::new(crate::keyring::test_support::from_map(api_keys))
    }

    #[test]
    fn legacy_alias_host_resolves_to_the_shared_domain_entry() {
        let backend = backend(&[("anaconda.com", "secret-key")]);
        assert_eq!(
            backend.get("repo.anaconda.cloud").unwrap(),
            Some(Authentication::BearerToken("secret-key".to_string()))
        );
        assert_eq!(
            backend.get("repo.anaconda.com").unwrap(),
            Some(Authentication::BearerToken("secret-key".to_string()))
        );
        assert_eq!(
            backend.get("repo.continuum.io").unwrap(),
            Some(Authentication::BearerToken("secret-key".to_string()))
        );
    }

    #[test]
    fn a_host_with_no_alias_and_no_matching_entry_has_no_credential() {
        let backend = backend(&[("anaconda.com", "secret-key")]);
        assert_eq!(backend.get("conda.anaconda.org").unwrap(), None);
    }

    #[test]
    fn a_host_with_no_alias_but_a_direct_entry_resolves_to_it() {
        let backend = backend(&[("repo.mycompany.com", "custom-key")]);
        assert_eq!(
            backend.get("repo.mycompany.com").unwrap(),
            Some(Authentication::BearerToken("custom-key".to_string()))
        );
    }

    #[test]
    fn store_and_delete_are_no_ops() {
        let backend = backend(&[("anaconda.com", "secret-key")]);
        backend
            .store("anaconda.com", &Authentication::BearerToken("x".into()))
            .unwrap();
        backend.delete("anaconda.com").unwrap();
        // Unaffected -- `store`/`delete` never touch the in-memory map.
        assert_eq!(
            backend.get("anaconda.com").unwrap(),
            Some(Authentication::BearerToken("secret-key".to_string()))
        );
    }
}
