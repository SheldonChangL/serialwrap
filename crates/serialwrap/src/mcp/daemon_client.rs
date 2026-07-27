//! Async client for the daemon's UDS protocol, purpose-built for the MCP
//! bridge's own concurrency shape.
//!
//! This is a separate, independent implementation from `cli::client`'s
//! `DaemonClient` — deliberately: that one's own docs call out exactly why
//! it can't be reused here: "Deliberately as small as
//! `crates/serialwrapd/tests/protocol.rs`'s own hand-rolled test client:
//! connect, send `hello`, then one newline-delimited JSON request per call,
//! read exactly the next line back as its reply. [...] A future client
//! that pipelines requests (e.g. T3.1's MCP bridge, if it ever needs to)
//! would need to match replies by `id` instead of assuming strict
//! request/reply ordering the way this one does."
//!
//! This bridge does need that: an MCP host can have a long-running
//! `wait_for` tool call in flight while another tool call (e.g.
//! `list_devices`) arrives concurrently, and per
//! `serialwrapd::protocol::session`'s own docs, the daemon dispatches every
//! request as its own independently-ordered task specifically so a slow
//! `wait_for` never blocks anything else on the same connection — a client
//! that only ever reads "the next line" as "the reply to what I just sent"
//! would break that guarantee by serializing itself. So: one physical UDS
//! connection, shared by every tool call this bridge process ever makes,
//! multiplexed by matching each reply's `id` back to the call that sent it
//! — the same discipline `serialwrapd::protocol::session`'s own
//! reader/writer split uses for the daemon's side of the same connection.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use wrap_proto::Request;

/// Environment variable that overrides the daemon socket path — the same
/// name (and same override behavior) `cli::client::SOCKET_ENV_VAR` uses,
/// duplicated here rather than imported so this module has zero dependency
/// on the `cli` module (see this crate's `mcp` module docs for why: T3.1's
/// scope is "an independent module", and reaching into `cli` for a
/// five-line function would couple the bridge's socket resolution to
/// whatever `cli::client` does next, for no real benefit).
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

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A connected, handshaken, id-multiplexed session with the daemon.
pub struct DaemonClient {
    next_id: AtomicU64,
    writer_tx: mpsc::UnboundedSender<String>,
    pending: PendingMap,
    // Kept alive for as long as this client is; never polled directly.
    _reader_task: tokio::task::JoinHandle<()>,
    _writer_task: tokio::task::JoinHandle<()>,
}

impl DaemonClient {
    /// Connect to the daemon at `path` and complete the `hello` handshake,
    /// identifying this process as `name`/`client_type`. Returns the client
    /// plus the daemon's raw `HelloAck` reply (the caller checks `ok`
    /// itself, same convention `cli::client::DaemonClient::connect` uses).
    pub async fn connect(
        path: &std::path::Path,
        name: &str,
        client_type: &str,
    ) -> io::Result<(Self, Value)> {
        let stream = UnixStream::connect(path).await?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // `hello` carries no `id` of its own (see `wrap_proto::HelloRequest`'s
        // docs) — send and read it synchronously, before the id-matching
        // reader/writer tasks below ever start.
        let hello = serde_json::json!({
            "op": "hello",
            "name": name,
            "type": client_type,
            "version": env!("CARGO_PKG_VERSION"),
        });
        write_half.write_all(hello.to_string().as_bytes()).await?;
        write_half.write_all(b"\n").await?;

        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "daemon closed the connection during the hello handshake",
            ));
        }
        let ack: Value = serde_json::from_str(&line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed hello ack from daemon: {e}: {line:?}"),
            )
        })?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<String>();

        let reader_task = tokio::spawn(reader_loop(reader, Arc::clone(&pending)));
        let writer_task = tokio::spawn(writer_loop(write_half, writer_rx, Arc::clone(&pending)));

        Ok((
            Self {
                next_id: AtomicU64::new(1),
                writer_tx,
                pending,
                _reader_task: reader_task,
                _writer_task: writer_task,
            },
            ack,
        ))
    }

    /// Send `request` with a freshly allocated `id` and await exactly its
    /// reply — regardless of how many other calls are concurrently
    /// in-flight on this same connection, or how long any of them (e.g. a
    /// `wait_for`) takes to resolve.
    pub async fn call(&self, request: Request) -> io::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut body = serde_json::to_value(&request).expect("Request always serializes");
        body.as_object_mut()
            .expect("Request always serializes to a JSON object")
            .insert("id".to_string(), id.into());

        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        if self.writer_tx.send(body.to_string()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the daemon connection's writer task is gone",
            ));
        }

        rx.await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the daemon connection closed before a reply for this request arrived",
            )
        })
    }
}

/// Fail every still-pending call with a synthetic `{"ok": false, ...}`
/// reply instead of leaving its caller awaiting a oneshot that will now
/// never resolve. Called from *both* [`reader_loop`] and [`writer_loop`] on
/// their own exit — whichever of the two notices the connection is dead
/// first — so a call inserted into `pending` in the narrow window after one
/// of them has already exited is still guaranteed to be resolved by the
/// other, rather than only ever being reader_loop's responsibility (which
/// would leave a real, if narrow, hang window: a call arriving after
/// `reader_loop` exits but before `writer_loop` independently notices the
/// same dead connection).
fn fail_all_pending(pending: &PendingMap) {
    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
    for (_, tx) in map.drain() {
        let _ = tx.send(serde_json::json!({
            "ok": false,
            "error": {"code": "internal", "message": "daemon connection closed"},
        }));
    }
}

async fn writer_loop(
    mut write_half: OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<String>,
    pending: PendingMap,
) {
    while let Some(line) = rx.recv().await {
        if write_half.write_all(line.as_bytes()).await.is_err() {
            fail_all_pending(&pending);
            return;
        }
        if write_half.write_all(b"\n").await.is_err() {
            fail_all_pending(&pending);
            return;
        }
    }
    // `rx` closed (every `DaemonClient`/writer_tx clone dropped) without a
    // write ever failing — not itself an error, but still worth failing
    // any call that's somehow still pending rather than leaving it hung.
    fail_all_pending(&pending);
}

async fn reader_loop(mut reader: BufReader<OwnedReadHalf>, pending: PendingMap) {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => {
                // Connection closed or errored: fail every still-pending
                // call instead of leaving its caller awaiting a oneshot
                // that will now never resolve.
                fail_all_pending(&pending);
                return;
            }
            Ok(_) => {
                let value: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "serialwrap: mcp: malformed reply from daemon, dropping: {e}: {line:?}"
                        );
                        continue;
                    }
                };
                let Some(id) = value.get("id").and_then(Value::as_u64) else {
                    eprintln!(
                        "serialwrap: mcp: daemon reply missing numeric `id`, dropping: {value}"
                    );
                    continue;
                };
                let sender = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                match sender {
                    Some(tx) => {
                        let _ = tx.send(value);
                    }
                    None => {
                        eprintln!("serialwrap: mcp: daemon reply for unknown/already-answered id {id}, dropping");
                    }
                }
            }
        }
    }
}
