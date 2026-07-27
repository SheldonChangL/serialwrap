//! `serialwrap tail [-f] [-n N] [--since T] [device]` (issue #7 /
//! `TASKS.md` T1.5) — the read-only "floor" tool: print a device's
//! recorded stream exactly as the daemon has it. Data rows and broker
//! events are visually distinguishable (see `cli::render`'s module docs
//! for why that's a correctness requirement, not decoration) and nothing
//! here embellishes or guesses at daemon state — it only ever prints what
//! `tail`/`read_since`/`subscribe` actually returned.

use std::io::{self, Write};

use clap::Args;
use serde_json::Value;

use wrap_proto::Request;

use super::client::{resolve_socket_path, DaemonClient};
use super::error::describe_connect_error;
use super::render::{render_data_line, render_event_line};
use super::time::{parse_since, passes_since};
use chrono::{DateTime, Utc};

#[derive(Args, Debug)]
pub struct TailArgs {
    /// Device id to tail (see `serialwrap devices`). Omit only when
    /// exactly one device is known to the daemon.
    pub device: Option<String>,

    /// Keep following: after printing history, subscribe to new records
    /// until interrupted with Ctrl-C. Exiting never affects the daemon or
    /// any other client — this process only ever closes its own
    /// connection.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Number of most recent lines to print. Ignored when `--since` is
    /// given (that flag selects a time window instead — see its own
    /// docs).
    #[arg(short = 'n', long, default_value_t = 20)]
    pub n: usize,

    /// Only print records at or after this point: an absolute RFC 3339
    /// timestamp (e.g. `2026-07-27T10:00:00+08:00`) or a relative duration
    /// suffixed `s`/`m`/`h`/`d` (e.g. `10m`, `2h`). Overrides `-n`.
    #[arg(long)]
    pub since: Option<String>,
}

pub async fn run(args: TailArgs) -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-tail", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let device = resolve_device(&mut client, args.device.as_deref()).await?;

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let cursor = if let Some(raw) = &args.since {
        let threshold = parse_since(raw).map_err(|msg| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("--since: {msg}"))
        })?;
        print_history_since(&mut client, &device, threshold, &mut out).await?
    } else {
        print_tail(&mut client, &device, args.n, &mut out).await?
    };

    if args.follow {
        follow(&mut client, &device, cursor, &mut out).await?;
    }
    Ok(())
}

/// Resolve which device to tail: the explicit argument if given, otherwise
/// the sole device the daemon currently knows about. Zero or multiple
/// devices without an explicit choice is an actionable error, not a guess.
async fn resolve_device(client: &mut DaemonClient, requested: Option<&str>) -> io::Result<String> {
    if let Some(device) = requested {
        return Ok(device.to_string());
    }
    let reply = client.call(Request::ListDevices).await?;
    check_ok(&reply, None)?;
    let devices = reply["devices"].as_array().cloned().unwrap_or_default();
    match devices.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no devices known yet — plug one in, then check `serialwrap devices`",
        )),
        1 => Ok(devices[0]["id"].as_str().unwrap_or_default().to_string()),
        _ => {
            let ids: Vec<&str> = devices.iter().filter_map(|d| d["id"].as_str()).collect();
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "multiple devices known ({}); specify one: `serialwrap tail <device>` (see \
                     `serialwrap devices`)",
                    ids.join(", ")
                ),
            ))
        }
    }
}

async fn print_tail(
    client: &mut DaemonClient,
    device: &str,
    n: usize,
    out: &mut impl Write,
) -> io::Result<u64> {
    let reply = client
        .call(Request::Tail {
            device: device.to_string(),
            n,
            filter: None,
        })
        .await?;
    check_ok(&reply, Some(device))?;
    print_records(out, &reply, None)?;
    Ok(reply["cursor"].as_u64().unwrap_or(0))
}

/// Fetch and print every record with `t_wall >= threshold`, paging through
/// the full history via `read_since` from cursor 0. Returns the final
/// cursor so `-f` (if also given) can continue exactly where this left
/// off.
async fn print_history_since(
    client: &mut DaemonClient,
    device: &str,
    threshold: DateTime<Utc>,
    out: &mut impl Write,
) -> io::Result<u64> {
    let mut cursor = 0u64;
    loop {
        let reply = client
            .call(Request::ReadSince {
                device: device.to_string(),
                cursor,
                max_bytes: Some(64 * 1024),
                filter: None,
            })
            .await?;
        check_ok(&reply, Some(device))?;
        let lines_empty = reply["lines"].as_array().is_none_or(Vec::is_empty);
        let events_empty = reply["events"].as_array().is_none_or(Vec::is_empty);
        if lines_empty && events_empty {
            break;
        }
        print_records(out, &reply, Some(threshold))?;
        let next = reply["cursor"].as_u64().unwrap_or(cursor);
        if next <= cursor {
            // Defensive: the daemon's own contract is "cursor always
            // advances", but never spin forever if that's ever violated.
            break;
        }
        cursor = next;
    }
    Ok(cursor)
}

/// `subscribe(since_cursor=cursor)` and print every push as it arrives,
/// until interrupted with Ctrl-C. Returning here only ever drops this
/// process's own `DaemonClient` connection — the daemon's
/// `reader_loop`/`writer_loop` (`serialwrapd::protocol::session`) treat
/// that exactly like any other client disconnecting: it unregisters this
/// client and nothing else, never touching the daemon or any other
/// connection.
///
/// This used to poll `read_since(cursor)` on a fixed interval instead,
/// because `subscribe` had no way to say "start from seq C" and so could
/// not be chained onto an initial `tail`/history fetch without a gap
/// between the two calls. Issue #32 added `since_cursor` with exactly
/// `read_since`'s own cursor semantics, which closes that gap — the first
/// thing this subscription ever pushes is exactly what
/// `read_since(cursor)` would have returned at that same instant (see
/// `serialwrapd::query::DeviceQueryState::cursor_from_seq`'s docs) — so
/// there is no longer a correctness reason to prefer polling, and push
/// removes the up-to-one-poll-interval latency the old approach paid on
/// every new line.
async fn follow(
    client: &mut DaemonClient,
    device: &str,
    cursor: u64,
    out: &mut impl Write,
) -> io::Result<()> {
    client
        .send(Request::Subscribe {
            device: device.to_string(),
            filter: None,
            since_cursor: Some(cursor),
        })
        .await?;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let reply = tokio::select! {
            _ = &mut ctrl_c => return Ok(()),
            reply = client.read_push() => reply?,
        };
        check_ok(&reply, Some(device))?;
        print_records(out, &reply, None)?;
    }
}

fn check_ok(reply: &Value, device: Option<&str>) -> io::Result<()> {
    if reply["ok"].as_bool() == Some(true) {
        return Ok(());
    }
    Err(io::Error::other(super::error::describe_wire_error(
        &reply["error"],
        device,
    )))
}

/// Print every line/event in a `tail`/`read_since` reply, merged into one
/// chronological stream by `seq` (matching how a human reading a combined
/// log would expect data and events to interleave), optionally dropping
/// anything older than `since`.
fn print_records(
    out: &mut impl Write,
    reply: &Value,
    since: Option<DateTime<Utc>>,
) -> io::Result<()> {
    enum Item<'a> {
        Line(&'a Value),
        Event(&'a Value),
    }
    impl Item<'_> {
        fn seq(&self) -> u64 {
            let value = match self {
                Item::Line(v) | Item::Event(v) => v,
            };
            value["seq"].as_u64().unwrap_or(0)
        }
        fn t_wall(&self) -> &str {
            let value = match self {
                Item::Line(v) | Item::Event(v) => v,
            };
            value["t_wall"].as_str().unwrap_or("")
        }
    }

    let mut items: Vec<Item> = Vec::new();
    if let Some(lines) = reply["lines"].as_array() {
        items.extend(lines.iter().map(Item::Line));
    }
    if let Some(events) = reply["events"].as_array() {
        items.extend(events.iter().map(Item::Event));
    }
    items.sort_by_key(Item::seq);

    for item in &items {
        let t_wall = item.t_wall();
        if let Some(threshold) = since {
            if !passes_since(t_wall, threshold) {
                continue;
            }
        }
        let rendered = match item {
            Item::Line(v) => render_data_line(t_wall, v),
            Item::Event(v) => render_event_line(t_wall, v),
        };
        writeln!(out, "{rendered}")?;
    }
    out.flush()
}
