//! Secret storage for uploader credentials.
//!
//! `settings.toml` never contains secrets, only `keyring:<name>` references (see
//! `ssx-core`'s validator). The referenced values live in the operating system's credential
//! store: Secret Service on Linux (GNOME Keyring, `KWallet`), Credential Manager on Windows,
//! Keychain on macOS, all through the `keyring` crate.
//!
//! Headless machines, containers, CI and minimal window managers often have **no** credential
//! daemon, and that must never crash an upload or make the CLI unusable. [`LayeredSecretStore`]
//! therefore reads from three places in order:
//!
//! 1. the environment variable `SSX_SECRET_<NAME>` (upper-cased, every character outside
//!    `A-Z0-9` becomes `_`), the documented way to feed secrets to scripts and servers;
//! 2. the OS credential store, when one is reachable;
//! 3. a process-local in-memory map, which is also where writes land when there is no
//!    credential store (with a one-time warning that they will not survive the process).

use std::{
    collections::HashMap,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use ssx_upload::{InMemorySecretStore, SecretError, SecretStore};

/// Service name entries are filed under in the OS store.
pub const KEYRING_SERVICE: &str = "ssx";

/// Prefix of the environment override.
pub const ENV_PREFIX: &str = "SSX_SECRET_";

/// A persistent secret store. Implemented for the OS keyring; tests supply fakes.
pub trait SecretBackend: Send + Sync {
    /// Short name for diagnostics.
    fn name(&self) -> &'static str;
    /// Reads `key`; a missing entry is `Ok(None)`.
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    /// Writes `key`.
    fn set(&self, key: &str, value: &str) -> Result<(), String>;
    /// Removes `key`; a missing entry is not an error.
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// The operating system's credential store, via the `keyring` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeyring;

impl OsKeyring {
    /// Returns the store if it is reachable *now*. The probe reads a key that does not exist;
    /// "no such entry" means the store works, anything else (no D-Bus session, no Secret
    /// Service, locked and refused) means it does not.
    pub fn probe() -> Result<Self, String> {
        if let Err(e) = keyring::Entry::store_status() {
            return Err(format!("the credential store could not be initialised: {e}"));
        }
        match Self.get("__ssx_probe__") {
            Ok(_) => Ok(Self),
            Err(e) => Err(e),
        }
    }

    fn entry(key: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(KEYRING_SERVICE, key).map_err(|e| e.to_string())
    }
}

impl SecretBackend for OsKeyring {
    fn name(&self) -> &'static str {
        if cfg!(windows) {
            "Windows Credential Manager"
        } else if cfg!(target_os = "macos") {
            "macOS Keychain"
        } else {
            "Secret Service"
        }
    }

    fn get(&self, key: &str) -> Result<Option<String>, String> {
        match Self::entry(key)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        Self::entry(key)?.set_password(value).map_err(|e| e.to_string())
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Where secrets currently go, for `ssx doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretStatus {
    /// Name of the persistent backend, if any.
    pub backend: Option<&'static str>,
    /// `true` when secrets survive the process.
    pub persistent: bool,
    /// Why there is no persistent backend.
    pub unavailable_reason: Option<String>,
}

/// Environment variable that overrides the secret called `key`.
pub fn env_var_name(key: &str) -> String {
    let mut s = String::from(ENV_PREFIX);
    s.extend(
        key.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }),
    );
    s
}

/// Looks up an environment variable by name.
type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The store handed to the uploaders. See the [module docs](self).
pub struct LayeredSecretStore {
    primary: Option<Box<dyn SecretBackend>>,
    unavailable: Option<String>,
    memory: InMemorySecretStore,
    env: EnvLookup,
    warned: AtomicBool,
}

impl std::fmt::Debug for LayeredSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayeredSecretStore").field("status", &self.status()).finish_non_exhaustive()
    }
}

impl LayeredSecretStore {
    /// Probes the OS credential store and falls back to memory (with a warning) if it is not
    /// reachable. Never fails.
    pub fn open() -> Self {
        match OsKeyring::probe() {
            Ok(k) => Self::with_backend(Box::new(k)),
            Err(reason) => {
                tracing::warn!(
                    "no OS credential store available ({reason}); uploader secrets can only come \
                     from SSX_SECRET_<NAME> environment variables or live in memory for this run"
                );
                Self::without_backend(reason)
            }
        }
    }

    /// Where secrets would go, found by probing the OS credential store *without* creating a
    /// store or logging anything (for diagnostics such as `ssx doctor`).
    pub fn probe_status() -> SecretStatus {
        match OsKeyring::probe() {
            Ok(k) => SecretStatus { backend: Some(k.name()), persistent: true, unavailable_reason: None },
            Err(reason) => SecretStatus { backend: None, persistent: false, unavailable_reason: Some(reason) },
        }
    }

    /// Uses `backend` as the persistent store.
    pub fn with_backend(backend: Box<dyn SecretBackend>) -> Self {
        Self::new(Some(backend), None)
    }

    /// No persistent store (`reason` says why).
    pub fn without_backend(reason: impl Into<String>) -> Self {
        Self::new(None, Some(reason.into()))
    }

    fn new(primary: Option<Box<dyn SecretBackend>>, unavailable: Option<String>) -> Self {
        Self {
            primary,
            unavailable,
            memory: InMemorySecretStore::new(),
            env: Box::new(|name| std::env::var(name).ok()),
            warned: AtomicBool::new(false),
        }
    }

    /// Replaces the environment lookup (tests).
    #[must_use]
    pub fn with_env(
        mut self,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.env = Box::new(env);
        self
    }

    /// Where secrets go.
    pub fn status(&self) -> SecretStatus {
        SecretStatus {
            backend: self.primary.as_ref().map(|b| b.name()),
            persistent: self.primary.is_some(),
            unavailable_reason: self.unavailable.clone(),
        }
    }

    fn warn_once(&self, why: &str) {
        if !self.warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                "{why}; the secret is kept in memory only and will be lost when ssx exits"
            );
        }
    }
}

impl SecretStore for LayeredSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        if let Some(v) = (self.env)(&env_var_name(key)).filter(|v| !v.is_empty()) {
            return Ok(Some(v));
        }
        if let Some(p) = &self.primary {
            match p.get(key) {
                Ok(Some(v)) => return Ok(Some(v)),
                Ok(None) => {}
                Err(e) => tracing::warn!("reading secret {key:?} from the {}: {e}", p.name()),
            }
        }
        self.memory.get(key)
    }

    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        if let Some(p) = &self.primary {
            match p.set(key, value) {
                Ok(()) => return Ok(()),
                Err(e) => self.warn_once(&format!("the {} refused the secret ({e})", p.name())),
            }
        } else {
            self.warn_once("there is no OS credential store");
        }
        self.memory.set(key, value)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        if let Some(p) = &self.primary
            && let Err(e) = p.delete(key)
        {
            return Err(SecretError(format!("the {} refused to delete {key:?}: {e}", p.name())));
        }
        self.memory.delete(key)
    }
}

/// A [`LayeredSecretStore`] that probes the OS credential store on first use, so building the
/// service bundle never talks to D-Bus or the keychain (most commands never need a secret).
#[derive(Default)]
pub struct LazySecrets(std::sync::OnceLock<LayeredSecretStore>);

impl std::fmt::Debug for LazySecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazySecrets").field("opened", &self.0.get().is_some()).finish()
    }
}

impl LazySecrets {
    /// A store that has not probed anything yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The underlying store, opening it now if necessary.
    pub fn store(&self) -> &LayeredSecretStore {
        self.0.get_or_init(LayeredSecretStore::open)
    }

    /// Where secrets go (opens the store).
    pub fn status(&self) -> SecretStatus {
        self.store().status()
    }
}

impl SecretStore for LazySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        self.store().get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        self.store().set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), SecretError> {
        self.store().delete(key)
    }
}

/// A `HashMap` backend for tests and embedders that want a fake "OS" store.
#[derive(Debug, Default)]
pub struct MemoryBackend(Mutex<HashMap<String, String>>);

impl SecretBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory backend"
    }
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap_or_else(PoisonError::into_inner).get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<(), String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct Shared(Arc<MemoryBackend>);
    impl SecretBackend for Shared {
        fn name(&self) -> &'static str {
            "shared"
        }
        fn get(&self, k: &str) -> Result<Option<String>, String> {
            self.0.get(k)
        }
        fn set(&self, k: &str, v: &str) -> Result<(), String> {
            self.0.set(k, v)
        }
        fn delete(&self, k: &str) -> Result<(), String> {
            self.0.delete(k)
        }
    }

    /// A backend whose every call fails, like a keyring daemon that went away.
    struct Broken;
    impl SecretBackend for Broken {
        fn name(&self) -> &'static str {
            "broken backend"
        }
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Err("no session bus".into())
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Err("no session bus".into())
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            Err("no session bus".into())
        }
    }

    #[test]
    fn env_names_are_shouty_and_sanitised() {
        assert_eq!(env_var_name("imgur-token"), "SSX_SECRET_IMGUR_TOKEN");
        assert_eq!(env_var_name("my.s3/key"), "SSX_SECRET_MY_S3_KEY");
    }

    #[test]
    fn without_a_credential_store_secrets_live_in_memory_and_never_error() {
        let s = LayeredSecretStore::without_backend("no D-Bus").with_env(|_| None);
        assert_eq!(s.get("k").unwrap(), None);
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
        s.delete("k").unwrap();
        assert_eq!(s.get("k").unwrap(), None);
        let st = s.status();
        assert!(!st.persistent && st.backend.is_none());
        assert_eq!(st.unavailable_reason.as_deref(), Some("no D-Bus"));
    }

    #[test]
    fn a_backend_that_fails_at_runtime_degrades_to_memory() {
        let s = LayeredSecretStore::with_backend(Box::new(Broken)).with_env(|_| None);
        assert_eq!(s.get("k").unwrap(), None, "read errors are logged, not propagated");
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
        // Deleting is the one operation that must not pretend to succeed.
        assert!(s.delete("k").is_err());
    }

    #[test]
    fn backend_wins_over_memory_and_environment_wins_over_both() {
        let backend = Arc::new(MemoryBackend::default());
        let s = LayeredSecretStore::with_backend(Box::new(Shared(backend.clone())))
            .with_env(|n| (n == "SSX_SECRET_FROM_ENV").then(|| "env-value".to_owned()));
        s.set("k", "stored").unwrap();
        assert_eq!(
            backend.get("k").unwrap().as_deref(),
            Some("stored"),
            "persisted in the backend"
        );
        assert_eq!(s.get("k").unwrap().as_deref(), Some("stored"));
        s.set("from-env", "stored").unwrap();
        assert_eq!(s.get("from-env").unwrap().as_deref(), Some("env-value"));
        assert!(s.status().persistent);
        s.delete("k").unwrap();
        assert_eq!(s.get("k").unwrap(), None);
    }

    #[test]
    fn empty_environment_values_do_not_mask_real_secrets() {
        let s = LayeredSecretStore::without_backend("x").with_env(|_| Some(String::new()));
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap().as_deref(), Some("v"));
    }

    #[test]
    fn open_never_panics_whatever_the_environment_looks_like() {
        // With a session bus this is the real keyring, without one the memory fallback; both
        // must yield a store whose status is self-consistent. (Nothing is written, so the test
        // never touches a developer's real keyring.)
        let s = LayeredSecretStore::open();
        let st = s.status();
        assert_eq!(st.persistent, st.backend.is_some());
        assert_eq!(st.persistent, st.unavailable_reason.is_none());
    }
}
