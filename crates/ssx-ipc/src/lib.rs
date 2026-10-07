//! Local IPC transport and single-instance guard for ssx.
//!
//! This crate only moves `\n`-terminated UTF-8 lines between two processes of the same user;
//! the message schema (JSON commands) lives in `ssx-core`. Keeping it schema-free lets the
//! tiny file-manager shim and the full app share the transport without sharing types.
//!
//! * [`Instance::acquire`] decides atomically whether this process is the [`Acquired::Primary`]
//!   (owns a [`Server`]) or a [`Acquired::Secondary`] (gets a [`Client`]).
//! * [`Client::request`] / [`Client::send_or_spawn`] send one line and read one line back.
//! * Transport: Unix domain socket (mode 0600 in a 0700 per-user directory) on Linux/macOS,
//!   per-user named pipe (owner-only ACL) on Windows, via the `interprocess` crate.
//!
//! Security model: same-user only. Enforced by directory/socket permissions, by verifying the
//! peer uid on both ends (Unix), and by the pipe ACL (Windows).

#![forbid(unsafe_code)]

mod client;
mod endpoint;
mod error;
mod framing;
mod instance;
#[cfg(test)]
mod pipe_tests;
mod server;
mod threaded;

pub use client::{Client, ClientConfig};
pub use endpoint::Location;
pub use error::{Error, Result, TimeoutKind};
pub use framing::DEFAULT_MAX_LINE;
pub use instance::{Acquired, Instance, Options};
pub use server::{
    Connection, Incoming, PeerInfo, PeerPolicy, ServeHandle, Server, ServerConfig, ShutdownHandle,
};
