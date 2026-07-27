//! `serialwrap mcp` (`TASKS.md` T3.1, issue #12): stdio MCP server bridging
//! to the daemon's UDS protocol, registering as `client_type=agent`. This is
//! the read-only slice — `list_devices`, `get_config`, `tail`, `read_since`,
//! `wait_for`. `write`/`set_config`/`dtr_pulse`/`export` land in T4.4/T2.4
//! (see `tools::RESERVED_WRITE_TOOL_NAMES`).
//!
//! Independent module tree, wired into `main.rs` with a single match arm —
//! nothing here touches `crates/serialwrap/src/cli/`'s existing modules
//! (their own docs already earmark this: "T3.1's MCP bridge lands its own
//! dispatch arms in `main.rs` right after this task").
//!
//! # Why hand-rolled JSON-RPC/MCP instead of an SDK crate
//!
//! Checked before writing a line of protocol code (`cargo search mcp`, then
//! `cargo info` on the top hits):
//!
//! - **`rmcp`** (`modelcontextprotocol/rust-sdk`, the project's own official
//!   Rust SDK) is at **3.0.0-beta.2** — still pre-1.0 after two prior major
//!   version lines, i.e. actively churning its API. Pulling in a beta
//!   dependency for a bridge whose correctness this task's acceptance
//!   criteria hold to a high bar (structured timeouts, stdout purity, exact
//!   byte fidelity) means inheriting whatever that crate's own
//!   in-progress-ness does to compile times, MSRV, and transitive deps
//!   (`schemars`, `uuid`, `pastey`, ...) for a stdio surface this task only
//!   needs a narrow slice of.
//! - **`rust-mcp-sdk`** (community, `rust-mcp-stack`) is at a stable-looking
//!   **1.0.1**, but it's a large framework (client+server+auth+SSE+
//!   streamable-http feature set) for what this task needs to be three
//!   methods (`initialize`/`tools/list`/`tools/call`) over newline-delimited
//!   JSON — adopting it would mean auditing a much bigger surface than this
//!   bridge actually exercises, for a project whose own protocol layer
//!   (`serialwrapd::protocol`) already hand-rolls exactly this kind of
//!   framed-JSON-over-a-stream server, successfully, with its own tests.
//!
//! MCP's stdio transport itself is not complex — newline-delimited JSON-RPC
//! 2.0 messages, no embedded newlines, nothing else on stdout (see
//! `rpc.rs`'s docs) — and this project already has the exact discipline
//! that requires baked into `serialwrapd::protocol::session` (framed NDJSON
//! read/write, one writer task funneling replies so they can't interleave,
//! id-based reply matching so a slow request never blocks others on the
//! same connection). Hand-rolling here means: zero new dependencies (this
//! module adds none — see `crates/serialwrap/Cargo.toml`'s `io-std` tokio
//! feature, the only Cargo.toml change this task makes), the whole bridge
//! auditable in one small module tree, and the exact same
//! concurrency/framing discipline this codebase already trusts elsewhere,
//! rather than a second, differently-shaped one imported from outside.
//!
//! # Module map
//!
//! - [`daemon_client`]: id-multiplexed async client for the daemon's UDS
//!   protocol — a separate implementation from `cli::client::DaemonClient`,
//!   which explicitly documents why it can't be reused for this (see that
//!   module's docs).
//! - [`rpc`]: the JSON-RPC 2.0 / MCP stdio transport loop.
//! - [`tools`]: the five read tools' schemas, descriptions (including the
//!   data-not-instruction injection defense), and daemon-request dispatch.
//! - [`events`]: per-device out-of-band event watermarks — what makes every
//!   read tool's result (not just `tail`/`read_since`) carry any
//!   disconnect/lease/config-change events since the agent last looked.
//! - [`line`]: the raw_b64 rule for a line's exact original bytes.

mod daemon_client;
mod events;
mod line;
mod rpc;
mod tools;

use std::io;
use std::sync::Arc;

use tools::ToolRegistry;

/// Entry point for `serialwrap mcp`. Never writes anything but MCP
/// protocol messages to stdout — see `rpc`'s module docs.
pub async fn run() -> io::Result<()> {
    eprintln!(
        "serialwrap: mcp: starting stdio bridge (daemon connection is established lazily, on \
         the first tool call that needs it)"
    );
    let socket_path = daemon_client::resolve_socket_path()?;
    let registry = Arc::new(ToolRegistry::new(socket_path));
    rpc::serve(registry).await
}
