//! Secret storage abstraction.
//!
//! Uploaders never hold API keys, OAuth tokens or S3 secrets in their configuration; they
//! hold a *key name* and look the value up here at upload time. The production
//! implementation (OS keyring) lives in another crate; [`InMemorySecretStore`] serves tests
//! and headless use.

use std::collections::HashMap;
use std::sync::Mutex;

/// Failure of the secret backend (locked keyring, D-Bus down...).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("secret store error: {0}")]
pub struct SecretError(pub String);

/// Key/value store for secrets. Synchronous because OS keyrings are; calls are short.
pub trait SecretStore: Send + Sync {
    /// Fetch a secret, `Ok(None)` when absent.
    fn get(&self, key: &str) -> Result<Option<String>, SecretError>;
    /// Store or replace a secret.
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError>;
    /// Remove a secret; removing an absent key is not an error.
    fn delete(&self, key: &str) -> Result<(), SecretError>;
}

/// Process-local store; contents vanish on drop.
#[derive(Debug, Default)]
pub struct InMemorySecretStore {
    map: Mutex<HashMap<String, String>>,
}

impl InMemorySecretStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for InMemorySecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        let map = self.map.lock().map_err(|_| SecretError("poisoned lock".into()))?;
        Ok(map.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        let mut map = self.map.lock().map_err(|_| SecretError("poisoned lock".into()))?;
        map.insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        let mut map = self.map.lock().map_err(|_| SecretError("poisoned lock".into()))?;
        map.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let s = InMemorySecretStore::new();
        assert_eq!(s.get("k").unwrap(), None);
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
        s.set("k", "w").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("w"));
        s.delete("k").unwrap();
        s.delete("k").unwrap();
        assert_eq!(s.get("k").unwrap(), None);
    }
}
