//! Minimal hand-rolled JSON-RPC 2.0 + MCP message framing over stdio.
//!
//! See `mod.rs`'s module docs for why this is hand-rolled rather than built
//! on an SDK crate. The surface implemented here is deliberately narrow —
//! exactly what a read-only MCP server needs: `initialize`, the
//! `notifications/initialized` notification, `ping`, `tools/list`, and
//! `tools/call`. Anything else gets a clean JSON-RPC "method not found"
//! rather than a hang or a panic.
//!
//! # stdout is the wire, not a log
//!
//! Every line this module writes to stdout is a complete, newline-
//! terminated JSON-RPC message — nothing else ever touches stdout. Any
//! diagnostic this module (or anything it calls) wants to record goes to
//! stderr via `eprintln!`. This is the single most common way to break an
//! MCP stdio server (a stray `println!` from a dependency or a debug print
//! left in by mistake corrupts every message after it), so the discipline
//! is enforced structurally here: [`serve`]'s stdin-reading loop and the
//! one dedicated writer task below are the *only* things that ever call
//! into `tokio::io::stdout()` in this whole bridge.
//!
//! # Concurrency
//!
//! Each incoming line is dispatched on its own spawned task, exactly like
//! `serialwrapd::protocol::session::reader_loop` dispatches daemon
//! requests — so a slow `wait_for` tool call never blocks `tools/list` (or
//! any other concurrent call) from being read and answered. Every task's
//! reply is funneled through one `mpsc` channel to a single writer task,
//! which is what guarantees two concurrently-completing replies can never
//! interleave their bytes on stdout — the same pattern
//! `serialwrapd::protocol::session::writer_loop` uses for the daemon's own
//! socket.

use std::io;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::tools::ToolRegistry;

const JSONRPC_VERSION: &str = "2.0";
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Fallback MCP protocol version when an `initialize` request omits
/// `protocolVersion` (non-conformant, but a clean fallback beats refusing
/// the connection). Whenever the client *does* send one, [`initialize_result`]
/// echoes it back instead — this server has no version-gated behavior of
/// its own, so matching whatever the host asked for is the safest way to
/// stay compatible across spec revisions without hardcoding one.
const FALLBACK_PROTOCOL_VERSION: &str = "2025-06-18";

/// Run the stdio JSON-RPC loop until stdin closes (the host disconnecting
/// is the normal way this returns).
pub async fn serve(registry: Arc<ToolRegistry>) -> io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            if stdout.write_all(b"\n").await.is_err() {
                return;
            }
            if stdout.flush().await.is_err() {
                return;
            }
        }
    });

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF: the host closed our stdin.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let owned = trimmed.to_string();
        let tx = tx.clone();
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            if let Some(reply) = handle_line(&owned, &registry).await {
                let _ = tx.send(reply.to_string());
            }
        });
    }

    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// Handle one incoming line. Returns `None` for JSON-RPC notifications
/// (no `id`, or a `null` one) — those never get a reply, success or
/// failure, per the JSON-RPC 2.0 spec.
async fn handle_line(line: &str, registry: &ToolRegistry) -> Option<Value> {
    let value: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("serialwrap: mcp: malformed JSON-RPC request line, ignoring: {e}: {line:?}");
            return Some(error_response(
                Value::Null,
                PARSE_ERROR,
                &format!("parse error: {e}"),
            ));
        }
    };

    let id = value.get("id").cloned();
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    let outcome = dispatch_method(method, params, registry).await;

    let id = match id {
        Some(id) if !id.is_null() => id,
        _ => {
            if let Err((_, message)) = &outcome {
                eprintln!("serialwrap: mcp: notification {method:?} failed (no reply sent, per JSON-RPC): {message}");
            }
            return None;
        }
    };

    Some(match outcome {
        Ok(result) => json!({"jsonrpc": JSONRPC_VERSION, "id": id, "result": result}),
        Err((code, message)) => error_response(id, code, &message),
    })
}

async fn dispatch_method(
    method: &str,
    params: Value,
    registry: &ToolRegistry,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(initialize_result(&params)),
        // The client's post-initialize notification and a bare liveness
        // check — both trivially succeed, nothing to do.
        "notifications/initialized" | "initialized" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": registry.list_tools_json() })),
        "tools/call" => tools_call(params, registry).await,
        other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

fn initialize_result(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(FALLBACK_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "serialwrap", "version": env!("CARGO_PKG_VERSION") },
    })
}

async fn tools_call(params: Value, registry: &ToolRegistry) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (INVALID_PARAMS, "tools/call missing `name`".to_string()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // A rejected/invalid tool call (bad device id, missing argument,
    // daemon unreachable, ...) is a normal outcome the agent should be able
    // to read and react to — it becomes the tool result's own `isError:
    // true`, never a JSON-RPC protocol-level error (that's reserved for
    // malformed *transport*-level requests, e.g. an unknown method).
    match registry.call(name, arguments).await {
        Ok(result) => Ok(json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default()}],
            "structuredContent": result,
            "isError": false,
        })),
        Err(message) => Ok(json!({
            "content": [{"type": "text", "text": message}],
            "isError": true,
        })),
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": JSONRPC_VERSION, "id": id, "error": {"code": code, "message": message}})
}
