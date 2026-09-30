//! Uploader secrets as the window sees them: names and presence, never values.
//!
//! `settings.toml` only ever holds `keyring:<name>` references; the values live in the OS
//! credential store (or, when there is none, in this process's memory, where they are lost
//! when the window closes). The window can *write* a value the user typed and *delete* one,
//! and can ask whether a name has a value. It cannot read a value back for display: the
//! [`SecretVault`] trait has no such method on purpose.
//!
//! The real vault wraps `ssx_services::LayeredSecretStore`, which also honours the
//! `SSX_SECRET_<NAME>` environment override. Opening it probes D-Bus / the credential
//! manager, which can take a while, so [`SystemVault`] opens it on a background thread and
//! [`SecretVault::location`] answers [`SecretLocation::Probing`] until that is done.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex, OnceLock, PoisonError},
};

use ssx_core::settings::KEYRING_PREFIX;
use ssx_services::{LayeredSecretStore, secrets::env_var_name};
use ssx_upload::{InMemorySecretStore, SecretStore};

/// Where new secrets go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretLocation {
    /// Still asking the OS.
    Probing,
    /// A persistent OS store (`Secret Service`, `Windows Credential Manager`, ...).
    Keyring(String),
    /// No OS store: secrets live in memory until the window closes.
    MemoryOnly(String),
}

impl SecretLocation {
    /// The sentence shown next to a secret field once it has a value.
    pub fn stored_text(&self) -> String {
        match self {
            SecretLocation::Probing => "stored (checking where...)".to_owned(),
            SecretLocation::Keyring(name) => format!("stored in the keyring ({name})"),
            SecretLocation::MemoryOnly(_) => {
                "stored in memory only (no keyring); it is lost when this window closes".to_owned()
            }
        }
    }

    /// The sentence shown when the field has no value.
    pub fn empty_text(&self) -> String {
        match self {
            SecretLocation::MemoryOnly(why) => {
                format!("not set; there is no keyring here ({why}), so a value would only be kept in memory")
            }
            _ => "not set".to_owned(),
        }
    }

    /// `true` if secrets survive the process.
    pub fn is_persistent(&self) -> bool {
        matches!(self, SecretLocation::Keyring(_))
    }
}

/// The extracted name of a `keyring:<name>` reference.
pub fn reference_name(value: &str) -> Option<&str> {
    value.strip_prefix(KEYRING_PREFIX).map(str::trim).filter(|n| !n.is_empty())
}

/// The reference to store in settings for secret `name`.
pub fn reference_for(name: &str) -> String {
    format!("{KEYRING_PREFIX}{name}")
}

/// A suggested secret name: `<uploader>-<field>` (`my-s3-secret-access-key`).
pub fn default_secret_name(uploader: &str, field: &str) -> String {
    format!("{uploader}-{}", field.replace('_', "-"))
}

/// Writes and checks secrets without ever returning one.
pub trait SecretVault: Send + Sync + fmt::Debug {
    /// Whether `name` has a value (in the environment, the OS store or memory).
    fn exists(&self, name: &str) -> Result<bool, String>;
    /// `true` if the value comes from the `SSX_SECRET_<NAME>` environment variable (which the
    /// window cannot change).
    fn from_environment(&self, name: &str) -> bool;
    /// Stores `value` under `name`.
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    /// Removes the value stored under `name`.
    fn delete(&self, name: &str) -> Result<(), String>;
    /// Where new secrets go.
    fn location(&self) -> SecretLocation;
    /// The store the uploaders read from (for test uploads).
    fn store(&self) -> Arc<dyn SecretStore>;
}

/// The real thing: `LayeredSecretStore`, opened in the background.
pub struct SystemVault {
    cell: Arc<OnceLock<Arc<LayeredSecretStore>>>,
}

impl fmt::Debug for SystemVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemVault").field("ready", &self.cell.get().is_some()).finish()
    }
}

impl SystemVault {
    /// Starts probing the OS store on a background thread.
    pub fn open() -> Self {
        let cell = Arc::new(OnceLock::new());
        let c = cell.clone();
        let _ = std::thread::Builder::new().name("ssx-keyring-probe".to_owned()).spawn(move || {
            let _ = c.get_or_init(|| Arc::new(LayeredSecretStore::open()));
        });
        Self { cell }
    }

    fn store_blocking(&self) -> Arc<LayeredSecretStore> {
        self.cell.get_or_init(|| Arc::new(LayeredSecretStore::open())).clone()
    }
}

/// A handle to the store that opens it (waiting for the probe) only when a secret is read or
/// written, so merely building the uploaders never waits for D-Bus.
struct LazyHandle(Arc<OnceLock<Arc<LayeredSecretStore>>>);

impl LazyHandle {
    fn store(&self) -> &Arc<LayeredSecretStore> {
        self.0.get_or_init(|| Arc::new(LayeredSecretStore::open()))
    }
}

impl SecretStore for LazyHandle {
    fn get(&self, key: &str) -> Result<Option<String>, ssx_upload::SecretError> {
        self.store().get(key)
    }
    fn set(&self, key: &str, value: &str) -> Result<(), ssx_upload::SecretError> {
        self.store().set(key, value)
    }
    fn delete(&self, key: &str) -> Result<(), ssx_upload::SecretError> {
        self.store().delete(key)
    }
}

impl SecretVault for SystemVault {
    fn exists(&self, name: &str) -> Result<bool, String> {
        self.store_blocking().get(name).map(|v| v.is_some()).map_err(|e| e.to_string())
    }
    fn from_environment(&self, name: &str) -> bool {
        std::env::var(env_var_name(name)).is_ok_and(|v| !v.is_empty())
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.store_blocking().set(name, value).map_err(|e| e.to_string())
    }
    fn delete(&self, name: &str) -> Result<(), String> {
        self.store_blocking().delete(name).map_err(|e| e.to_string())
    }
    fn location(&self) -> SecretLocation {
        match self.cell.get() {
            None => SecretLocation::Probing,
            Some(s) => {
                let st = s.status();
                match st.backend {
                    Some(b) if st.persistent => SecretLocation::Keyring(b.to_owned()),
                    _ => SecretLocation::MemoryOnly(
                        st.unavailable_reason.unwrap_or_else(|| "unavailable".to_owned()),
                    ),
                }
            }
        }
    }
    fn store(&self) -> Arc<dyn SecretStore> {
        Arc::new(LazyHandle(self.cell.clone()))
    }
}

/// An in-memory vault for tests and screenshots, with a chosen [`SecretLocation`].
#[derive(Debug)]
pub struct MemoryVault {
    values: Mutex<HashMap<String, String>>,
    env: Mutex<Vec<String>>,
    location: SecretLocation,
    store: Arc<InMemorySecretStore>,
    fail: Mutex<Option<String>>,
}

impl MemoryVault {
    /// A vault that reports `location`.
    pub fn new(location: SecretLocation) -> Self {
        Self {
            values: Mutex::default(),
            env: Mutex::default(),
            location,
            store: Arc::new(InMemorySecretStore::new()),
            fail: Mutex::default(),
        }
    }

    /// A vault that behaves like a working keyring.
    pub fn keyring() -> Self {
        Self::new(SecretLocation::Keyring("Secret Service".to_owned()))
    }

    /// A vault with no keyring.
    pub fn memory_only() -> Self {
        Self::new(SecretLocation::MemoryOnly("no Secret Service on the session bus".to_owned()))
    }

    /// Pretends `name` is provided by the environment.
    pub fn with_env(self, name: &str) -> Self {
        self.env.lock().unwrap_or_else(PoisonError::into_inner).push(name.to_owned());
        let _ = self.store.set(name, "from-env");
        self
    }

    /// Makes every write fail.
    pub fn fail_with(&self, why: &str) {
        *self.fail.lock().unwrap_or_else(PoisonError::into_inner) = Some(why.to_owned());
    }

    /// The names that have a value (never the values).
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> =
            self.values.lock().unwrap_or_else(PoisonError::into_inner).keys().cloned().collect();
        v.sort();
        v
    }

    /// The stored value, for asserting in tests only (the trait cannot do this).
    pub fn peek(&self, name: &str) -> Option<String> {
        self.values.lock().unwrap_or_else(PoisonError::into_inner).get(name).cloned()
    }
}

impl SecretVault for MemoryVault {
    fn exists(&self, name: &str) -> Result<bool, String> {
        Ok(self.from_environment(name)
            || self.values.lock().unwrap_or_else(PoisonError::into_inner).contains_key(name))
    }
    fn from_environment(&self, name: &str) -> bool {
        self.env.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|n| n == name)
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        if let Some(e) = self.fail.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return Err(e);
        }
        self.values.lock().unwrap_or_else(PoisonError::into_inner).insert(name.into(), value.into());
        self.store.set(name, value).map_err(|e| e.to_string())
    }
    fn delete(&self, name: &str) -> Result<(), String> {
        if let Some(e) = self.fail.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return Err(e);
        }
        self.values.lock().unwrap_or_else(PoisonError::into_inner).remove(name);
        self.store.delete(name).map_err(|e| e.to_string())
    }
    fn location(&self) -> SecretLocation {
        self.location.clone()
    }
    fn store(&self) -> Arc<dyn SecretStore> {
        self.store.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_round_trip() {
        assert_eq!(reference_for("my-s3-key"), "keyring:my-s3-key");
        assert_eq!(reference_name("keyring:my-s3-key"), Some("my-s3-key"));
        assert_eq!(reference_name("keyring: spaced "), Some("spaced"));
        assert_eq!(reference_name("keyring:"), None);
        assert_eq!(reference_name("plain"), None);
        assert_eq!(default_secret_name("my-s3", "secret_access_key"), "my-s3-secret-access-key");
    }

    #[test]
    fn texts_say_where_the_secret_is() {
        let k = SecretLocation::Keyring("Secret Service".into());
        assert!(k.stored_text().contains("keyring") && k.is_persistent());
        assert_eq!(k.empty_text(), "not set");
        let m = SecretLocation::MemoryOnly("no bus".into());
        assert!(m.stored_text().contains("memory only") && m.stored_text().contains("closes"));
        assert!(m.empty_text().contains("no bus") && !m.is_persistent());
        assert!(SecretLocation::Probing.stored_text().contains("checking"));
    }

    #[test]
    fn memory_vault_stores_checks_and_deletes() {
        let v = MemoryVault::keyring();
        assert_eq!(v.exists("a"), Ok(false));
        v.set("a", "hunter2").unwrap();
        assert_eq!(v.exists("a"), Ok(true));
        assert_eq!(v.names(), ["a"]);
        assert_eq!(v.store().get("a").unwrap().as_deref(), Some("hunter2"));
        v.delete("a").unwrap();
        assert_eq!(v.exists("a"), Ok(false));
        assert!(v.store().get("a").unwrap().is_none());
    }

    #[test]
    fn environment_secrets_count_and_are_flagged() {
        let v = MemoryVault::memory_only().with_env("imgur-token");
        assert_eq!(v.exists("imgur-token"), Ok(true));
        assert!(v.from_environment("imgur-token"));
        assert!(!v.from_environment("other"));
    }

    #[test]
    fn failures_surface_as_messages() {
        let v = MemoryVault::keyring();
        v.fail_with("keyring is locked");
        assert_eq!(v.set("a", "x").unwrap_err(), "keyring is locked");
        assert_eq!(v.delete("a").unwrap_err(), "keyring is locked");
    }

    #[test]
    fn the_trait_has_no_way_to_read_a_value() {
        // Compile-time documentation: everything a page can call returns bool / () / location.
        fn takes(v: &dyn SecretVault) -> (Result<bool, String>, bool, SecretLocation) {
            (v.exists("x"), v.from_environment("x"), v.location())
        }
        let v = MemoryVault::keyring();
        let _ = takes(&v);
    }

    #[test]
    fn system_vault_reports_probing_or_a_location_and_never_panics() {
        let v = SystemVault::open();
        // The probe may or may not have finished; both answers are valid.
        let _ = v.location();
        let _ = v.from_environment("definitely-not-set-anywhere");
    }
}
