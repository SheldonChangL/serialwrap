//! Minimal UDS client for this crate's own subcommands (issue #7 /
//! `TASKS.md` T1.5).
//!
//! Deliberately as small as `crates/serialwrapd/tests/protocol.rs`'s own
//! hand-rolled test client: connect, send `hello`, then one
//! newline-delimited JSON request per call, read exactly the next line
//! back as its reply. That simplification is safe here specifically
//! because `devices`/`tail` never have more than one request in flight on
//! a connection at a time (no `subscribe`, no pipelining) — unlike the
//! daemon's own session handling, which must cope with genuinely
//! out-of-order replies (see `serialwrapd::protocol::session`'s module
//! docs on why every request is its own spawned task). A future client
//! that pipelines requests (e.g. T3.1's MCP bridge, if it ever needs to)
//! would need to match replies by `id` instead of assuming strict
//! request/reply ordering the way this one does.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

use wrap_proto::Request;

/// Environment variable that overrides the daemon socket path. Used by
/// this crate's own integration tests (which can't safely bind to the
/// production `XDG_RUNTIME_DIR`/`~/.serialwrap` location — concurrent test
/// runs would collide on it) and, incidentally, by anyone who wants to
/// point this CLI at a non-default daemon instance.
pub const SOCKET_ENV_VAR: &str = "SERIALWRAP_SOCKET";

/// Resolve the daemon socket path: [`SOCKET_ENV_VAR`] overrides; otherwise
/// the production default (`serialwrapd::protocol::default_socket_path`,
/// the same path `serialwrap daemon` itself binds to).
pub fn resolve_socket_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(SOCKET_ENV_VAR) {
        return Ok(PathBuf::from(path));
    }
    serialwrapd::protocol::default_socket_path()
}

/// A connected, handshaken session with the daemon.
pub struct DaemonClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl DaemonClient {
    /// Connect to the daemon listening at `path` and complete the `hello`
    /// handshake, identifying this process as `name`/`client_type` (the
    /// wire's literal `"human"`/`"agent"`/`"tool"` strings — see the
    /// [Client protocol
    /// wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)).
    /// Returns the client plus the daemon's raw `HelloAck` reply.
    pub async fn connect(path: &Path, name: &str, client_type: &str) -> io::Result<(Self, Value)> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 1,
        };
        let hello = serde_json::json!({
            "op": "hello",
            "name": name,
            "type": client_type,
            "version": env!("CARGO_PKG_VERSION"),
        });
        client.write_line(&hello.to_string()).await?;
        let ack = client.read_reply().await?;
        Ok((client, ack))
    }

    /// Send `request` with a freshly allocated `id` and return the raw
    /// reply value — still either `{"ok": true, ...}` or `{"ok": false,
    /// "error": {...}}`; callers branch on `ok` themselves (see
    /// `cli::error::describe_wire_error` for turning a `false` reply into
    /// an actionable message).
    pub async fn call(&mut self, request: Request) -> io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut body = serde_json::to_value(&request).expect("Request always serializes");
        body.as_object_mut()
            .expect("Request always serializes to a JSON object")
            .insert("id".to_string(), id.into());
        self.write_line(&body.to_string()).await?;
        self.read_reply().await
    }

    async fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await
    }

    async fn read_reply(&mut self) -> io::Result<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the daemon closed the connection",
            ));
        }
        serde_json::from_str(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed reply from daemon: {e}: {line:?}"),
            )
        })
    }
}
