//! Windows integrations: classic registry verbs, Send To, and the Windows 11 plan.
//!
//! Everything here compiles and is unit-tested on every OS (the registry goes through
//! [`registry::RegistryBackend`]); only the real registry backend is `cfg(windows)`.

pub mod manifest;
pub mod registry;
mod sendto;
mod verbs;

pub use sendto::SendTo;
pub use verbs::ClassicVerbs;
