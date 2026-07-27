//! `serialwrap clients` / `clients kick <id>` / `clients demote <id> <perm>`
//! (issue #10 / `TASKS.md` T2.3): session management — who's connected,
//! with what identity (self-reported name plus the daemon's
//! kernel-verified pid) and permission, and the two operator-only actions
//! the [Security-model
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Security-model)
//! reserves for a human: "kick 他們只會讓人關掉 gate".

use std::io::{self, Write as _};

use clap::Subcommand;
use serde_json::Value;

use wrap_proto::{Permission, Request};

use super::client::{resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

#[derive(Subcommand, Debug)]
pub enum ClientsCommand {
    /// Close a connected client's connection.
    Kick {
        /// The daemon-assigned `client_id` from a plain `serialwrap
        /// clients` listing (not the client's own pid).
        id: u64,
    },
    /// Change a connected client's permission level in place.
    Demote {
        id: u64,
        /// One of the wire's own permission strings, verbatim: `read+write`,
        /// `read+gated_write`, or `lease_only` (see the Client protocol
        /// wiki's handshake example).
        permission: String,
    },
}

#[derive(clap::Args, Debug)]
pub struct ClientsArgs {
    #[command(subcommand)]
    pub command: Option<ClientsCommand>,
}

pub async fn run(args: ClientsArgs) -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-clients", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    match args.command {
        None => list(&mut client).await,
        Some(ClientsCommand::Kick { id }) => kick(&mut client, id).await,
        Some(ClientsCommand::Demote { id, permission }) => {
            demote(&mut client, id, &permission).await
        }
    }
}

fn parse_permission(raw: &str) -> io::Result<Permission> {
    serde_json::from_value(Value::String(raw.to_string())).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid permission {raw:?} — expected one of: read+write, read+gated_write, \
                 lease_only"
            ),
        )
    })
}

async fn list(client: &mut DaemonClient) -> io::Result<()> {
    let reply = client.call(Request::ListClients).await?;
    check_ok(&reply)?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let clients = reply["clients"].as_array().cloned().unwrap_or_default();
    if clients.is_empty() {
        writeln!(out, "no clients connected")?;
        return out.flush();
    }
    for c in &clients {
        let id = c["client_id"].as_u64().unwrap_or(0);
        let name = c["name"].as_str().unwrap_or("?");
        let pid = c["pid"].as_u64().unwrap_or(0);
        let client_type = c["type"].as_str().unwrap_or("?");
        let permission = c["permission"].as_str().unwrap_or("?");
        let bytes_in = c["bytes_in"].as_u64().unwrap_or(0);
        let bytes_out = c["bytes_out"].as_u64().unwrap_or(0);
        let activity = describe_activity(&c["activity"]);
        writeln!(
            out,
            "{id}\t{name}\tpid={pid}\t{client_type}\t{permission}\tin={bytes_in} \
             out={bytes_out}\t{activity}"
        )?;
    }
    out.flush()
}

fn describe_activity(activity: &Value) -> String {
    match activity["state"].as_str() {
        Some("waiting_for") => format!(
            "waiting_for(device={}, pattern={:?}, remaining_s={:.1})",
            activity["device"].as_str().unwrap_or("?"),
            activity["pattern"].as_str().unwrap_or("?"),
            activity["remaining_s"].as_f64().unwrap_or(0.0),
        ),
        _ => "idle".to_string(),
    }
}

async fn kick(client: &mut DaemonClient, id: u64) -> io::Result<()> {
    let reply = client.call(Request::Kick { client_id: id }).await?;
    check_ok(&reply)?;
    println!("kicked client {id}");
    Ok(())
}

async fn demote(client: &mut DaemonClient, id: u64, permission_raw: &str) -> io::Result<()> {
    let permission = parse_permission(permission_raw)?;
    let reply = client
        .call(Request::Demote {
            client_id: id,
            permission,
        })
        .await?;
    check_ok(&reply)?;
    println!("demoted client {id} to {permission_raw}");
    Ok(())
}

fn check_ok(reply: &Value) -> io::Result<()> {
    if reply["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    Err(io::Error::other(describe_wire_error(&reply["error"], None)))
}
