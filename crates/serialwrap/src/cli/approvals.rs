//! `serialwrap approvals` / `approvals approve <id>` / `approvals deny <id>`
//! (issue #15 / `TASKS.md` T4.2): the CLI's view onto
//! `serialwrapd::gate::Gate`'s pending-approval queue — the exact same
//! `approvals_list`/`approval_approve`/`approval_deny` wire ops the future
//! GUI approval card (T5.4) will call, so nothing about this CLI's
//! decisions is special-cased on the daemon side (see `crate::gate`'s
//! module docs).
//!
//! Connects as `human` — same identity convention `clients.rs`'s
//! kick/demote already uses: approving or denying a pending write is an
//! operator-only action, and the daemon records *which* operator via its
//! own kernel-verified `changed_by`, never a value this CLI sends.

use std::io::{self, Write as _};

use clap::Subcommand;
use serde_json::Value;

use wrap_proto::Request;

use super::client::{resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

#[derive(Subcommand, Debug)]
pub enum ApprovalsCommand {
    /// Allow a pending write to proceed.
    Approve {
        /// The gate-assigned pending id from a plain `serialwrap approvals`
        /// listing (not a `client_id`, not a recorder `seq`).
        id: u64,
    },
    /// Refuse a pending write.
    Deny {
        id: u64,
        /// Optional human-readable reason recorded in the audit trail and
        /// returned to the requester's structured `write_denied` reply. A
        /// generic operator-denied label is used if omitted.
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct ApprovalsArgs {
    #[command(subcommand)]
    pub command: Option<ApprovalsCommand>,
}

pub async fn run(args: ApprovalsArgs) -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-approvals", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    match args.command {
        None => list(&mut client).await,
        Some(ApprovalsCommand::Approve { id }) => approve(&mut client, id).await,
        Some(ApprovalsCommand::Deny { id, reason }) => deny(&mut client, id, reason).await,
    }
}

async fn list(client: &mut DaemonClient) -> io::Result<()> {
    let reply = client.call(Request::ApprovalsList).await?;
    check_ok(&reply)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let approvals = reply["approvals"].as_array().cloned().unwrap_or_default();
    if approvals.is_empty() {
        writeln!(out, "no pending approvals")?;
        return out.flush();
    }
    for a in &approvals {
        let id = a["id"].as_u64().unwrap_or(0);
        let device = a["device"].as_str().unwrap_or("?");
        let name = a["requester_name"].as_str().unwrap_or("?");
        let pid = a["requester_pid"].as_u64().unwrap_or(0);
        let requester_type = a["requester_type"].as_str().unwrap_or("?");
        let req_no = a["session_request_no"].as_u64().unwrap_or(0);
        let matched_rule = a["matched_rule"].as_str().unwrap_or("-");
        let age_s = a["age_s"].as_f64().unwrap_or(0.0);
        writeln!(
            out,
            "{id}\t{device}\trequester={name}(pid={pid},{requester_type})\treq#{req_no}\t\
             rule={matched_rule}\tage={age_s:.1}s"
        )?;
        if let Some(reason) = a["danger_reason"].as_str() {
            writeln!(out, "  why dangerous: {reason}")?;
        }
        if let Some(text) = a["bytes_text"].as_str() {
            writeln!(out, "  text: {text:?}")?;
        }
        if let Some(hex) = a["bytes_hex"].as_str() {
            writeln!(out, "  hex:  {hex}")?;
        }
        let context = a["log_context"].as_array().cloned().unwrap_or_default();
        if !context.is_empty() {
            writeln!(out, "  log context (before this request):")?;
            for line in &context {
                writeln!(out, "    | {}", line.as_str().unwrap_or(""))?;
            }
        }
    }
    out.flush()
}

async fn approve(client: &mut DaemonClient, id: u64) -> io::Result<()> {
    let reply = client
        .call(Request::ApprovalApprove { approval_id: id })
        .await?;
    check_ok(&reply)?;
    println!("approved pending write {id}");
    Ok(())
}

async fn deny(client: &mut DaemonClient, id: u64, reason: Option<String>) -> io::Result<()> {
    let reply = client
        .call(Request::ApprovalDeny {
            approval_id: id,
            reason,
        })
        .await?;
    check_ok(&reply)?;
    println!("denied pending write {id}");
    Ok(())
}

fn check_ok(reply: &Value) -> io::Result<()> {
    if reply["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    Err(io::Error::other(describe_wire_error(&reply["error"], None)))
}
