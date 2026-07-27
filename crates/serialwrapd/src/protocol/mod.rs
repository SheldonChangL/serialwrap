//! UDS client protocol (`TASKS.md` T1.4, issue #6): handshake, request
//! dispatch, peer credentials, and session/client management, wired on top
//! of [`crate::query`]'s line assembly and either `port.rs`'s real
//! `HotplugDetector` or a test double — see [`backend::DeviceBackend`].
//!
//! This is the module M2 (CLI), M3 (MCP), and M5 (GUI) all become clients
//! of; see the [Client protocol
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
//! for the authoritative request set, error codes, and handshake this
//! implements.
//!
//! # Module map
//!
//! - [`backend`]: [`backend::DeviceBackend`] — the seam to "wherever
//!   devices come from" (production `HotplugDetector`, or a test double).
//! - [`peer_cred`]: kernel-reported peer pid (`SO_PEERCRED` /
//!   `LOCAL_PEERPID`).
//! - [`registry`]: per-device query-state cache + the `list_clients`/
//!   `kick`/`demote` session table.
//! - [`server`]: socket bind (`0600`) and the accept loop.
//! - [`session`]: per-connection handshake and request dispatch. See its
//!   module docs for why every request is its own spawned task (so a
//!   long-running `wait_for` never blocks other requests on the same
//!   connection) and how a `kick` takes effect immediately.

pub mod backend;
pub mod peer_cred;
pub mod registry;
pub mod server;
pub mod session;

pub use server::{default_socket_path, Shared};
