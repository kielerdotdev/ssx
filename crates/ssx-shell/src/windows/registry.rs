//! Registry access behind a trait so the installer logic is testable on any OS.
//!
//! Only `HKEY_CURRENT_USER` is ever touched (no elevation, no per-machine changes), so paths
//! are relative to it, e.g. `Software\Classes\*\shell\ssx.upload`. [`MemoryRegistry`] is a
//! faithful in-memory fake (case-insensitive keys/values, implicit parent keys). The real
//! backend `WindowsRegistry` only exists on `cfg(windows)` and is **compile-checked only**
//! in this repository's Linux CI: it has not been executed against a real registry.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::sync::Mutex;

/// Minimal string-valued registry operations under `HKCU`. The empty value name is the key's
/// `(Default)` value.
pub trait RegistryBackend: Send + Sync + fmt::Debug {
    /// Reads a `REG_SZ` value; `None` if the key or value does not exist.
    fn get_string(&self, key: &str, name: &str) -> io::Result<Option<String>>;
    /// Writes a `REG_SZ` value, creating the key (and parents) if needed.
    fn set_string(&self, key: &str, name: &str, value: &str) -> io::Result<()>;
    /// Whether the key exists.
    fn key_exists(&self, key: &str) -> io::Result<bool>;
    /// Recursively deletes a key. Returns whether it existed.
    fn delete_tree(&self, key: &str) -> io::Result<bool>;
    /// Deletes the key only if it has no values and no subkeys. Returns whether it was deleted.
    fn delete_key_if_empty(&self, key: &str) -> io::Result<bool>;
}

#[derive(Debug, Default, Clone)]
struct MemKey {
    /// Key path as first spelled.
    display: String,
    /// Lower-cased value name to (name, value).
    values: BTreeMap<String, (String, String)>,
}

/// In-memory [`RegistryBackend`] for tests and dry runs.
#[derive(Debug, Default)]
pub struct MemoryRegistry {
    keys: Mutex<BTreeMap<String, MemKey>>,
}

fn norm(path: &str) -> String {
    path.trim_matches('\\').to_lowercase()
}

impl MemoryRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, BTreeMap<String, MemKey>>> {
        self.keys.lock().map_err(|_| io::Error::other("registry lock poisoned"))
    }

    /// Deterministic dump (`key`, then `key|name=value` lines) for equality checks in tests.
    pub fn snapshot(&self) -> Vec<String> {
        let Ok(keys) = self.lock() else { return Vec::new() };
        let mut out = Vec::new();
        for k in keys.values() {
            out.push(k.display.clone());
            for (name, value) in k.values.values() {
                out.push(format!("{}|{}={}", k.display, name, value));
            }
        }
        out
    }
}

impl RegistryBackend for MemoryRegistry {
    fn get_string(&self, key: &str, name: &str) -> io::Result<Option<String>> {
        Ok(self
            .lock()?
            .get(&norm(key))
            .and_then(|k| k.values.get(&name.to_lowercase()))
            .map(|(_, v)| v.clone()))
    }

    fn set_string(&self, key: &str, name: &str, value: &str) -> io::Result<()> {
        let mut keys = self.lock()?;
        let trimmed = key.trim_matches('\\');
        // Create every ancestor, like RegCreateKeyEx.
        let mut prefix = String::new();
        for part in trimmed.split('\\') {
            if !prefix.is_empty() {
                prefix.push('\\');
            }
            prefix.push_str(part);
            keys.entry(norm(&prefix))
                .or_insert_with(|| MemKey { display: prefix.clone(), values: BTreeMap::new() });
        }
        if let Some(k) = keys.get_mut(&norm(trimmed)) {
            k.values.insert(name.to_lowercase(), (name.to_owned(), value.to_owned()));
        }
        Ok(())
    }

    fn key_exists(&self, key: &str) -> io::Result<bool> {
        Ok(self.lock()?.contains_key(&norm(key)))
    }

    fn delete_tree(&self, key: &str) -> io::Result<bool> {
        let mut keys = self.lock()?;
        let root = norm(key);
        let child_prefix = format!("{root}\\");
        let doomed: Vec<String> =
            keys.keys().filter(|k| **k == root || k.starts_with(&child_prefix)).cloned().collect();
        let existed = !doomed.is_empty();
        for k in doomed {
            keys.remove(&k);
        }
        Ok(existed)
    }

    fn delete_key_if_empty(&self, key: &str) -> io::Result<bool> {
        let mut keys = self.lock()?;
        let k = norm(key);
        let child_prefix = format!("{k}\\");
        let has_children = keys.keys().any(|o| o.starts_with(&child_prefix));
        match keys.get(&k) {
            Some(mk) if mk.values.is_empty() && !has_children => {
                keys.remove(&k);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(windows)]
pub use real::WindowsRegistry;

#[cfg(windows)]
mod real {
    use std::io;

    use super::RegistryBackend;
    use windows_registry::CURRENT_USER;

    /// The real `HKEY_CURRENT_USER`. Compile-checked only; not run in CI for this repository.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct WindowsRegistry;

    impl WindowsRegistry {
        /// Creates the backend.
        pub fn new() -> Self {
            Self
        }
    }

    /// Converts a `windows-registry` result, mapping "not found" HRESULTs to
    /// `io::ErrorKind::NotFound` (the crate's error type is not nameable, hence the alias).
    fn conv<T>(r: windows_registry::Result<T>) -> io::Result<T> {
        r.map_err(|e| {
            // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND / ERROR_PATH_NOT_FOUND)
            let code = e.code().0 as u32;
            if code == 0x8007_0002 || code == 0x8007_0003 {
                io::Error::new(io::ErrorKind::NotFound, e.message())
            } else {
                io::Error::other(e.message())
            }
        })
    }

    /// `Ok(None)` for "not found", the value otherwise.
    fn optional<T>(r: windows_registry::Result<T>) -> io::Result<Option<T>> {
        match conv(r) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn split(key: &str) -> (&str, &str) {
        match key.trim_matches('\\').rsplit_once('\\') {
            Some((parent, leaf)) => (parent, leaf),
            None => ("", key.trim_matches('\\')),
        }
    }

    impl RegistryBackend for WindowsRegistry {
        fn get_string(&self, key: &str, name: &str) -> io::Result<Option<String>> {
            let Some(k) = optional(CURRENT_USER.open(key))? else { return Ok(None) };
            optional(k.get_string(name))
        }

        fn set_string(&self, key: &str, name: &str, value: &str) -> io::Result<()> {
            let k = conv(CURRENT_USER.create(key))?;
            conv(k.set_string(name, value))
        }

        fn key_exists(&self, key: &str) -> io::Result<bool> {
            Ok(optional(CURRENT_USER.open(key))?.is_some())
        }

        fn delete_tree(&self, key: &str) -> io::Result<bool> {
            if !self.key_exists(key)? {
                return Ok(false);
            }
            conv(CURRENT_USER.remove_tree(key))?;
            Ok(true)
        }

        fn delete_key_if_empty(&self, key: &str) -> io::Result<bool> {
            let Some(k) = optional(CURRENT_USER.open(key))? else { return Ok(false) };
            let no_subkeys = conv(k.keys())?.next().is_none();
            let no_values = conv(k.values())?.next().is_none();
            drop(k);
            if !(no_subkeys && no_values) {
                return Ok(false);
            }
            let (parent, leaf) = split(key);
            if parent.is_empty() {
                conv(CURRENT_USER.remove_tree(leaf))?;
            } else {
                let p = conv(CURRENT_USER.options().read().write().open(parent))?;
                conv(p.remove_tree(leaf))?;
            }
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_case_insensitive_and_implicit_parents() {
        let r = MemoryRegistry::new();
        r.set_string(r"Software\Classes\*\shell\ssx.upload", "MUIVerb", "Upload").expect("set");
        assert_eq!(
            r.get_string(r"software\classes\*\SHELL\ssx.upload", "muiverb")
                .expect("get")
                .as_deref(),
            Some("Upload")
        );
        assert!(r.key_exists(r"Software\Classes\*").expect("exists"));
        assert_eq!(r.get_string(r"Software\Classes", "").expect("get"), None);
    }

    #[test]
    fn delete_tree_and_delete_if_empty() {
        let r = MemoryRegistry::new();
        r.set_string(r"A\B\C", "", "x").expect("set");
        r.set_string(r"A\B\C\command", "", "y").expect("set");
        assert!(!r.delete_key_if_empty(r"A\B").expect("nonempty"), "has a subkey");
        assert!(r.delete_tree(r"A\B\C").expect("tree"));
        assert!(!r.delete_tree(r"A\B\C").expect("again"));
        assert!(r.delete_key_if_empty(r"A\B").expect("empty"));
        assert!(r.delete_key_if_empty(r"A").expect("empty"));
        assert_eq!(r.snapshot(), Vec::<String>::new());
    }

    #[test]
    fn delete_tree_does_not_touch_siblings_with_common_prefix() {
        let r = MemoryRegistry::new();
        r.set_string(r"A\ssx.upload", "", "1").expect("set");
        r.set_string(r"A\ssx.upload2", "", "2").expect("set");
        r.delete_tree(r"A\ssx.upload").expect("del");
        assert!(r.key_exists(r"A\ssx.upload2").expect("exists"));
    }
}
