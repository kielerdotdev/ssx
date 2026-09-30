//! [`HotkeyId`]: a stable, human-meaningful name for an action bound to a hotkey.

use std::{fmt, sync::Arc};

/// Names an action such as `capture-region`. Ids double as identifiers in the
/// GlobalShortcuts portal and as file/schema-path names for the generated GNOME and KDE
/// bindings, so they are restricted to `[a-z0-9._-]`, 1..=64 characters.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HotkeyId(Arc<str>);

/// A rejected [`HotkeyId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid hotkey id {0:?}: use 1-64 characters from a-z, 0-9, '.', '_' and '-'")]
pub struct InvalidHotkeyId(pub String);

impl HotkeyId {
    /// Validates and wraps `id`.
    pub fn new(id: impl AsRef<str>) -> Result<Self, InvalidHotkeyId> {
        let id = id.as_ref();
        let ok = !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            });
        if ok { Ok(Self(Arc::from(id))) } else { Err(InvalidHotkeyId(id.to_owned())) }
    }

    /// The id text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HotkeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for HotkeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HotkeyId({:?})", &*self.0)
    }
}

impl std::str::FromStr for HotkeyId {
    type Err = InvalidHotkeyId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_ids() {
        assert!(HotkeyId::new("capture-region").is_ok());
        assert!(HotkeyId::new("a.b_c-9").is_ok());
        assert!(HotkeyId::new("x".repeat(64)).is_ok());
        for bad in ["", "Upper", "has space", "slash/es", "ünï", "semi;colon", &"x".repeat(65)] {
            assert!(HotkeyId::new(bad).is_err(), "{bad:?}");
        }
        assert_eq!(HotkeyId::new("a").unwrap().to_string(), "a");
        assert_eq!("x".parse::<HotkeyId>().unwrap(), HotkeyId::new("x").unwrap());
    }
}
