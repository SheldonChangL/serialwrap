//! `serialwrap devices` (issue #7 / `TASKS.md` T1.5): device id, path,
//! connection state, and current config — exactly `list_devices`'s wire
//! reply, rendered as plain text. No flags: the spec (issue #7, the
//! [Client protocol
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol))
//! doesn't call for any on this subcommand.

use std::io::{self, Write as _};

use wrap_proto::Request;

use super::client::{resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

pub async fn run() -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-devices", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let reply = client.call(Request::ListDevices).await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(io::Error::other(describe_wire_error(&reply["error"], None)));
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let devices = reply["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        writeln!(
            out,
            "no devices known yet — plug one in, then run this again"
        )?;
        return out.flush();
    }

    for d in &devices {
        let id = d["id"].as_str().unwrap_or("?");
        let path = d["path"].as_str().unwrap_or("-");
        let connected = if d["connected"].as_bool().unwrap_or(false) {
            "connected"
        } else {
            "disconnected"
        };
        let config = &d["config"];
        let baud = config["baud"]
            .as_u64()
            .map(|b| b.to_string())
            .unwrap_or_else(|| "-".to_string());
        let data_bits = config["data_bits"].as_str().unwrap_or("-");
        let parity = config["parity"].as_str().unwrap_or("-");
        let stop_bits = config["stop_bits"].as_str().unwrap_or("-");
        let flow = config["flow_control"].as_str().unwrap_or("-");
        writeln!(
            out,
            "{id}\t{connected}\t{path}\tbaud={baud} data_bits={data_bits} parity={parity} \
             stop_bits={stop_bits} flow={flow}"
        )?;
    }
    out.flush()
}
