//! `serialwrap audit [device] [--today] [--actor NAME] [--export jsonl]
//! [--context SEQ] [--lines N]` (`TASKS.md` T4.3, issue #16).
//!
//! # Audit is a query view, not a second store
//!
//! Every row this command prints is one record already sitting in the same
//! append-only event stream `tail`/`export` read from — a `tx` (a write
//! that actually reached the device), a `gate` (a request/allow/deny/
//! approve decision), or an audit-relevant `event` (a lease, a
//! config/control-line change, a `dtr_pulse`, a client kick, or the
//! `write_request` event `serialwrapd::gate::Gate::submit_write` appends
//! for every pending/denied write — see that module's doc comment for why
//! that's how a denied write's full payload survives at all). This module
//! never talks to a `Recorder` directly and adds no daemon-side storage: it
//! only *queries* the existing stream, over the exact same wire ops
//! `export`/`tail` already use (`Request::Export`, `Request::ReadSince`),
//! and filters/renders a subset of what comes back. That's also why
//! `--export jsonl`'s output is guaranteed to match `serialwrap export
//! --format jsonl`'s own format byte-for-byte (T4.3 acceptance criterion
//! 3) — it's the *same lines*, verbatim, from the *same*
//! `serialwrapd::export::export_range` renderer, merely a filtered subset
//! of them, never independently re-serialized.
//!
//! # Context (±N lines)
//!
//! `--context SEQ` answers "what was the device doing right around this
//! record" for *any* seq — a `gate` decision, a `tx`, a `write_request`, or
//! any other record's own `seq`. Implemented via `Request::ReadSince`
//! (exactly what `tail`/the MCP bridge's `read_since` tool already use):
//! fetch every line/event the daemon currently has, merge them into one
//! seq-ordered sequence, and slice `--lines` entries immediately before and
//! after the target seq. No new daemon-side query primitive, no
//! reimplemented line assembly — this is `serialwrapd::query`'s existing
//! line-assembly output, sliced client-side.

use std::io::{self, Write as _};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{Local, TimeZone};
use clap::Args;
use serde_json::Value;

use wrap_proto::{ExportBound, ExportFormat, Record, Request};

use super::client::{resolve_device, resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

/// `Record::Event`'s `event` names considered audit-relevant — see the
/// module docs' `kind in [tx, gate, event(lease/config/kick)]`.
/// `Record::Tx`/`Record::Gate` are always audit-relevant regardless of
/// name; `Record::Rx` (plain device output) never is.
const AUDIT_EVENT_NAMES: &[&str] = &[
    "write_request",
    "lease_start",
    "lease_end",
    "config_change",
    "control_line_change",
    "dtr_pulse",
    "client_kicked",
];

fn is_audit_relevant(record: &Record) -> bool {
    match record {
        Record::Tx { .. } | Record::Gate { .. } => true,
        Record::Event { event, .. } => AUDIT_EVENT_NAMES.contains(&event.as_str()),
        Record::Rx { .. } => false,
    }
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum AuditExportFormat {
    Jsonl,
}

#[derive(Args, Debug)]
pub struct AuditArgs {
    /// Device id (see `serialwrap devices`). Omit only when exactly one
    /// device is known to the daemon.
    pub device: Option<String>,

    /// Only records from today (local wall clock, matching `t_wall`'s own
    /// local-time convention — see `serialwrapd::recorder`).
    #[arg(long)]
    pub today: bool,

    /// Only records mentioning this actor — a plain substring match against
    /// each record's raw JSON, deliberately not a structured per-field
    /// lookup: the same identity can appear as a `tx` record's `client`, a
    /// `write_request`/`config_change`/`dtr_pulse` event's
    /// `requester_name`/`changed_by`, or a `gate` decision's own
    /// `approved_by:`/`denied_by_operator:`-prefixed `reason`.
    #[arg(long)]
    pub actor: Option<String>,

    /// Emit matching records verbatim as jsonl, one per line — byte-for-byte
    /// the same per-record shape `serialwrap export --format jsonl`
    /// produces, since this reuses that exact daemon-side renderer rather
    /// than re-serializing anything.
    #[arg(long, value_enum)]
    pub export: Option<AuditExportFormat>,

    /// Show `--lines` records of context immediately before and after the
    /// record at this `seq` — any record's own seq (a `tx`, a `gate`
    /// decision, an audit-relevant `event`, or a plain device log line) —
    /// instead of the normal listing. `--today`/`--actor`/`--export` are
    /// ignored when this is given.
    #[arg(long)]
    pub context: Option<u64>,

    /// Context window size for `--context`: this many records immediately
    /// before, and this many immediately after. Defaults to the same
    /// 20-line window the write gate's own approval payload already shows
    /// an operator — see `serialwrapd::gate::DEFAULT_LOG_CONTEXT_LINES`.
    #[arg(long, default_value_t = serialwrapd::gate::DEFAULT_LOG_CONTEXT_LINES)]
    pub lines: usize,
}

pub async fn run(args: AuditArgs) -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-audit", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let device = resolve_device(&mut client, args.device.as_deref()).await?;

    if let Some(seq) = args.context {
        return show_context(&mut client, &device, seq, args.lines).await;
    }

    list(
        &mut client,
        &device,
        args.today,
        args.actor.as_deref(),
        args.export.is_some(),
    )
    .await
}

/// Local midnight, today, as an RFC 3339 timestamp — `Request::Export`'s
/// own `--from` wall-clock bound (see `cli::export`'s `--last`/`--from`
/// handling for the same `ExportBound::Wall` shape).
fn start_of_today_rfc3339() -> String {
    let now = Local::now();
    let Some(midnight_naive) = now.date_naive().and_hms_opt(0, 0, 0) else {
        return now.to_rfc3339();
    };
    match Local.from_local_datetime(&midnight_naive) {
        chrono::LocalResult::Single(dt) => dt.to_rfc3339(),
        chrono::LocalResult::Ambiguous(dt, _) => dt.to_rfc3339(),
        chrono::LocalResult::None => now.to_rfc3339(),
    }
}

async fn list(
    client: &mut DaemonClient,
    device: &str,
    today: bool,
    actor: Option<&str>,
    export_jsonl: bool,
) -> io::Result<()> {
    let from = if today {
        Some(ExportBound::Wall(start_of_today_rfc3339()))
    } else {
        None
    };
    let reply = client
        .call(Request::Export {
            device: device.to_string(),
            format: ExportFormat::Jsonl,
            from,
            to: None,
            filter: None,
        })
        .await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(io::Error::other(describe_wire_error(
            &reply["error"],
            Some(device),
        )));
    }
    let data_b64 = reply["data_b64"].as_str().unwrap_or("");
    let bytes = BASE64.decode(data_b64).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed data_b64 in export reply: {e}"),
        )
    })?;
    let text = String::from_utf8_lossy(&bytes);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut printed = 0usize;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        // Every line here came from the daemon's own `export_range`
        // renderer (see the module docs) — defensively skip (rather than
        // fail the whole command over) a line that somehow doesn't parse,
        // matching this crate's general no-panic-on-unexpected-input
        // stance.
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        if !is_audit_relevant(&record) {
            continue;
        }
        if let Some(actor) = actor {
            if !line.contains(actor) {
                continue;
            }
        }

        if export_jsonl {
            // Printed verbatim — the same bytes the daemon sent, never
            // re-serialized — so this is guaranteed byte-identical to what
            // `serialwrap export --format jsonl` would print for this same
            // record.
            writeln!(out, "{line}")?;
        } else {
            let value = serde_json::to_value(&record)
                .expect("Record always serializes back to the same wire shape it was parsed from");
            writeln!(out, "seq={} {}", record.seq(), render_audit_row(&value))?;
        }
        printed += 1;
    }
    if !export_jsonl && printed == 0 {
        writeln!(out, "no audit records match")?;
    }
    out.flush()
}

/// Render one audit-relevant record for the human-readable listing —
/// reusing `cli::render`'s already-established `tail`/`event` rendering
/// verbatim: a `Record`'s own serde shape (`{"kind":"tx",...}`/
/// `{"kind":"gate",...}`/`{"kind":"event",...}`) is field-for-field
/// identical to the wire's `oob_json` shape those functions already
/// render, so no bespoke formatting logic is needed here at all.
fn render_audit_row(value: &Value) -> String {
    let t_wall = value["t_wall"].as_str().unwrap_or("");
    super::render::render_event_line(t_wall, value)
}

async fn show_context(
    client: &mut DaemonClient,
    device: &str,
    seq: u64,
    lines_n: usize,
) -> io::Result<()> {
    let reply = read_since_resilient(client, device, 0).await?;

    let mut rows: Vec<(u64, bool, Value)> = Vec::new();
    for l in reply["lines"].as_array().cloned().unwrap_or_default() {
        let s = l["seq"].as_u64().unwrap_or(0);
        rows.push((s, true, l));
    }
    for e in reply["events"].as_array().cloned().unwrap_or_default() {
        let s = e["seq"].as_u64().unwrap_or(0);
        rows.push((s, false, e));
    }
    rows.sort_by_key(|(s, _, _)| *s);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    let before: Vec<&(u64, bool, Value)> = rows.iter().filter(|(s, _, _)| *s < seq).collect();
    let at_or_after: Vec<&(u64, bool, Value)> = rows.iter().filter(|(s, _, _)| *s >= seq).collect();

    let before_start = before.len().saturating_sub(lines_n);
    for (s, is_line, v) in &before[before_start..] {
        print_context_row(&mut out, *s, *is_line, v, false)?;
    }

    let mut found_target = false;
    for (i, (s, is_line, v)) in at_or_after.iter().enumerate() {
        if i > lines_n {
            break;
        }
        let is_target = *s == seq;
        if is_target {
            found_target = true;
        }
        print_context_row(&mut out, *s, *is_line, v, is_target)?;
    }
    if !found_target {
        writeln!(
            out,
            "(no record found at seq {seq} in this device's currently retained history — \
             showing nearby context only)"
        )?;
    }
    out.flush()
}

fn print_context_row(
    out: &mut impl io::Write,
    seq: u64,
    is_line: bool,
    v: &Value,
    is_target: bool,
) -> io::Result<()> {
    let t_wall = v["t_wall"].as_str().unwrap_or("");
    let rendered = if is_line {
        super::render::render_data_line(t_wall, v)
    } else {
        super::render::render_event_line(t_wall, v)
    };
    let marker = if is_target { ">>" } else { "  " };
    writeln!(out, "{marker} seq={seq} {rendered}")
}

/// [`Request::ReadSince`] from `cursor`, resyncing once to the reported
/// floor if `cursor` has already aged out of the ring — same "clamp
/// forward, never fail outright" stance `serialwrapd::export`'s own
/// `collect_range` takes for the identical situation.
async fn read_since_resilient(
    client: &mut DaemonClient,
    device: &str,
    cursor: u64,
) -> io::Result<Value> {
    let reply = client
        .call(Request::ReadSince {
            device: device.to_string(),
            cursor,
            max_bytes: None,
            filter: None,
        })
        .await?;
    if reply["ok"].as_bool() == Some(true) {
        return Ok(reply);
    }
    if reply["error"]["code"].as_str() == Some("data_aged_out") {
        let oldest = reply["error"]["oldest_available_seq"].as_u64().unwrap_or(0);
        let retried = client
            .call(Request::ReadSince {
                device: device.to_string(),
                cursor: oldest,
                max_bytes: None,
                filter: None,
            })
            .await?;
        if retried["ok"].as_bool() == Some(true) {
            return Ok(retried);
        }
        return Err(io::Error::other(describe_wire_error(
            &retried["error"],
            Some(device),
        )));
    }
    Err(io::Error::other(describe_wire_error(
        &reply["error"],
        Some(device),
    )))
}
