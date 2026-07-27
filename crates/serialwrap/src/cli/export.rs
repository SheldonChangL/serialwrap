//! `serialwrap export [device] --format <jsonl|txt|bin> [--from ...]
//! [--to ...] [--last DURATION] [--boot] [--filter REGEX] [-o FILE]`
//! (`TASKS.md` T2.4, issue #11).
//!
//! Deliberately thin: every guarantee this command makes (`bin`
//! byte-exactness, `jsonl` losslessness, `txt`'s exact shape, filter
//! semantics, aged-out warnings) is implemented once, daemon-side, in
//! [`serialwrapd::export`] — see that module's docs for why (T5.5's future
//! GUI export dialog must produce byte-identical output to this CLI for
//! the same parameters, so the logic can only live in one place: the
//! daemon, behind `wrap_proto::Request::Export`). This module only:
//!
//! - parses/validates arguments and their mutual exclusivity,
//! - resolves `--boot` (this device's most recent `connect` event — see
//!   [`resolve_boot_marker`] for why that's the chosen marker) and `--last`
//!   (a relative duration, reusing [`super::time::parse_since`]) into the
//!   wire's `ExportBound` shape,
//! - sends one `Request::Export` and decodes the reply, and
//! - handles the one concern that has no daemon-side analogue at all:
//!   refusing to write `bin` to a terminal (see [`run`]'s tty check) —
//!   the same "never let binary reach a terminal" stance `cli::render`
//!   already established for `tail`.

use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use clap::{Args, ValueEnum};

use wrap_proto::{ExportBound, ExportFormat as WireExportFormat, Filter, Request};

use super::client::{resolve_device, resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};
use super::time::parse_since;

/// CLI-facing spelling of [`wrap_proto::ExportFormat`] — a separate type so
/// `clap`'s `ValueEnum` derive (which needs to live in this crate) doesn't
/// have to be implemented on `wrap-proto`'s wire type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormatArg {
    Jsonl,
    Txt,
    Bin,
}

impl From<ExportFormatArg> for WireExportFormat {
    fn from(f: ExportFormatArg) -> Self {
        match f {
            ExportFormatArg::Jsonl => WireExportFormat::Jsonl,
            ExportFormatArg::Txt => WireExportFormat::Txt,
            ExportFormatArg::Bin => WireExportFormat::Bin,
        }
    }
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Device id to export (see `serialwrap devices`). Omit only when
    /// exactly one device is known to the daemon.
    pub device: Option<String>,

    /// Output format — see the wiki's Event stream and storage page,
    /// "Export formats" section, for what each one guarantees.
    #[arg(long, value_enum)]
    pub format: ExportFormatArg,

    /// Range start: an exact sequence number, or an absolute RFC 3339
    /// timestamp (e.g. `2026-07-27T10:00:00+08:00`). Mutually exclusive
    /// with `--last`/`--boot`.
    #[arg(long)]
    pub from: Option<String>,

    /// Range end: an exact sequence number, or an absolute RFC 3339
    /// timestamp. Omitted means "up to whatever is recorded right now".
    #[arg(long)]
    pub to: Option<String>,

    /// Export the last DURATION of history (e.g. `10m`, `2h`, `30s`, `1d`)
    /// up to now. Mutually exclusive with `--from`/`--boot`/`--to`.
    #[arg(long)]
    pub last: Option<String>,

    /// Export from this device's most recent boot marker (its latest
    /// `connect` event) up to now. Mutually exclusive with `--from`/`--last`.
    #[arg(long)]
    pub boot: bool,

    /// Only include `rx` records whose content matches this regex.
    /// `jsonl`/`txt` only — combined with `--format bin` this is a
    /// rejected, explicit error (never silently ignored): a filtered byte
    /// stream is not the byte-exact artifact `bin` promises.
    #[arg(long)]
    pub filter: Option<String>,

    /// Write to this file instead of stdout.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,
}

pub async fn run(args: ExportArgs) -> io::Result<()> {
    let format = WireExportFormat::from(args.format);
    validate_range_flags(&args)?;

    let filter = build_filter(&args, format)?;

    // Refuse *before* ever contacting the daemon: dumping arbitrary device
    // bytes into a terminal can reprogram it (control sequences) or just
    // look like garbage — the same stance `cli::render` already takes for
    // `tail`. A file output (`-o`) never touches stdout at all, so this
    // check only applies to the no-`-o` (stdout) case.
    if format == WireExportFormat::Bin && args.output.is_none() && io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to write a `bin` export to a terminal (it would corrupt your terminal \
             session with raw device bytes) — redirect it (e.g. `> out.bin`) or pass `-o file` \
             instead",
        ));
    }

    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-export", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let device = resolve_device(&mut client, args.device.as_deref()).await?;

    let from = resolve_from(&mut client, &device, &args).await?;
    let to = match &args.to {
        Some(raw) => Some(parse_bound(raw)?),
        None => None,
    };

    let reply = client
        .call(Request::Export {
            device: device.clone(),
            format,
            from,
            to,
            filter,
        })
        .await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(io::Error::other(describe_wire_error(
            &reply["error"],
            Some(&device),
        )));
    }

    // Never silent: the wiki's own words for a range extending into
    // ring-evicted data are "產生警告與截斷結果，不靜默" — surfaced here as
    // a stderr warning, distinct from stdout/the output file (which only
    // ever carries the exported bytes themselves).
    if let Some(oldest) = reply["truncated_start"].as_u64() {
        eprintln!(
            "serialwrap: warning: the requested range reaches back further than what's still \
             retained; truncated to the oldest data still available (seq {oldest} onward)"
        );
    }

    let data_b64 = reply["data_b64"].as_str().unwrap_or("");
    let bytes = BASE64.decode(data_b64).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed data_b64 in export reply: {e}"),
        )
    })?;

    match &args.output {
        Some(path) => {
            let mut file = File::create(path)?;
            file.write_all(&bytes)?;
        }
        None => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            out.write_all(&bytes)?;
            out.flush()?;
        }
    }

    Ok(())
}

fn build_filter(args: &ExportArgs, format: WireExportFormat) -> io::Result<Option<Filter>> {
    let Some(pattern) = &args.filter else {
        return Ok(None);
    };
    if format == WireExportFormat::Bin {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--filter is not allowed with --format bin: it would silently break byte-exactness \
             — drop --filter, or export jsonl/txt instead",
        ));
    }
    Ok(Some(Filter {
        pattern: pattern.clone(),
        exclude: false,
    }))
}

/// Reject conflicting ways of specifying where the range starts/ends,
/// rather than silently letting one flag win over another.
fn validate_range_flags(args: &ExportArgs) -> io::Result<()> {
    let start_flags = [args.from.is_some(), args.last.is_some(), args.boot]
        .iter()
        .filter(|set| **set)
        .count();
    if start_flags > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--from, --last, and --boot are mutually exclusive — pick exactly one way to say \
             where the exported range starts",
        ));
    }
    if args.last.is_some() && args.to.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--last already bounds the range up to now; combining it with --to doesn't leave \
             anything for --to to mean — drop one",
        ));
    }
    Ok(())
}

async fn resolve_from(
    client: &mut DaemonClient,
    device: &str,
    args: &ExportArgs,
) -> io::Result<Option<ExportBound>> {
    if let Some(raw) = &args.from {
        return Ok(Some(parse_bound(raw)?));
    }
    if let Some(raw) = &args.last {
        let threshold = parse_since(raw)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, format!("--last: {msg}")))?;
        return Ok(Some(ExportBound::Wall(threshold.to_rfc3339())));
    }
    if args.boot {
        return Ok(Some(resolve_boot_marker(client, device).await?));
    }
    Ok(None)
}

/// Resolve `--boot` to this device's most recent boot marker: the highest
/// `seq` among its `connect` events.
///
/// # Why `connect`
///
/// The daemon's hotplug detector (`serialwrapd::port`) appends a `connect`
/// event every time a device is (re)enumerated — the one unambiguous
/// "this is a fresh session with the device" signal this project currently
/// records. A physical power cycle or USB replug always produces a fresh
/// `connect`; that's the case `--boot` most needs to serve ("what did the
/// device print since it last rebooted"). Reusing the existing
/// `query_events` endpoint for this (rather than adding daemon-side boot
/// logic) also means a future GUI's own `--boot`-equivalent option is
/// exactly this same two-call sequence — no separate API to keep in sync.
///
/// # Known limitation
///
/// Some boards (notably Arduino-style auto-reset conventions — see
/// `serialwrapd::port_io`'s own commentary) reboot via a DTR pulse
/// (`dtr_pulse` events) without any USB re-enumeration at all, so without a
/// physical replug `--boot` won't see that as a new boot. Folding
/// `dtr_pulse` in as an *additional* boot signal was considered and
/// deliberately left out: not every device treats a DTR pulse as a reset
/// (it's also used for plain flow-control-line toggling), so treating every
/// `dtr_pulse` as a boot marker would over-trigger for those. If this
/// becomes a real pain point, the fix is additive (broaden the `kinds`
/// filter below), not a rewrite.
async fn resolve_boot_marker(client: &mut DaemonClient, device: &str) -> io::Result<ExportBound> {
    let reply = client
        .call(Request::QueryEvents {
            device: device.to_string(),
            kinds: vec!["connect".to_string()],
            since_seq: None,
            until_seq: None,
        })
        .await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(io::Error::other(describe_wire_error(
            &reply["error"],
            Some(device),
        )));
    }
    let last_connect_seq = reply["events"]
        .as_array()
        .and_then(|events| events.iter().filter_map(|e| e["seq"].as_u64()).max());
    match last_connect_seq {
        Some(seq) => Ok(ExportBound::Seq(seq)),
        None => {
            eprintln!(
                "serialwrap: warning: no `connect` event recorded yet for {device:?} — --boot \
                 is exporting the full retained history instead"
            );
            Ok(ExportBound::Seq(0))
        }
    }
}

/// Parse a `--from`/`--to` value: a plain integer is a `seq`; anything else
/// must be a valid RFC 3339 timestamp.
fn parse_bound(raw: &str) -> io::Result<ExportBound> {
    let trimmed = raw.trim();
    if let Ok(seq) = trimmed.parse::<u64>() {
        return Ok(ExportBound::Seq(seq));
    }
    match chrono::DateTime::parse_from_rfc3339(trimmed) {
        Ok(dt) => Ok(ExportBound::Wall(dt.to_rfc3339())),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "not a recognized seq or RFC 3339 timestamp: {raw:?} (expected a plain integer \
                 seq, or e.g. 2026-07-27T10:00:00+08:00)"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_args() -> ExportArgs {
        ExportArgs {
            device: None,
            format: ExportFormatArg::Jsonl,
            from: None,
            to: None,
            last: None,
            boot: false,
            filter: None,
            output: None,
        }
    }

    #[test]
    fn parse_bound_accepts_a_plain_seq() {
        assert_eq!(parse_bound("42").unwrap(), ExportBound::Seq(42));
    }

    #[test]
    fn parse_bound_accepts_an_rfc3339_timestamp() {
        match parse_bound("2026-07-27T10:00:00+08:00").unwrap() {
            ExportBound::Wall(s) => assert!(s.contains("2026-07-27")),
            other => panic!("expected Wall, got {other:?}"),
        }
    }

    #[test]
    fn parse_bound_rejects_garbage() {
        assert!(parse_bound("not-a-thing").is_err());
    }

    #[test]
    fn validate_range_flags_rejects_from_and_boot_together() {
        let mut args = base_args();
        args.from = Some("10".to_string());
        args.boot = true;
        assert!(validate_range_flags(&args).is_err());
    }

    #[test]
    fn validate_range_flags_rejects_last_and_to_together() {
        let mut args = base_args();
        args.last = Some("10m".to_string());
        args.to = Some("20".to_string());
        assert!(validate_range_flags(&args).is_err());
    }

    #[test]
    fn validate_range_flags_allows_boot_with_to() {
        let mut args = base_args();
        args.boot = true;
        args.to = Some("20".to_string());
        assert!(validate_range_flags(&args).is_ok());
    }

    #[test]
    fn validate_range_flags_allows_nothing_given() {
        assert!(validate_range_flags(&base_args()).is_ok());
    }

    #[test]
    fn build_filter_rejects_filter_with_bin_format() {
        let mut args = base_args();
        args.filter = Some("x".to_string());
        let err = build_filter(&args, WireExportFormat::Bin).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn build_filter_allows_filter_with_jsonl_format() {
        let mut args = base_args();
        args.filter = Some("x".to_string());
        let filter = build_filter(&args, WireExportFormat::Jsonl).unwrap();
        assert_eq!(filter.unwrap().pattern, "x");
    }

    #[test]
    fn build_filter_is_none_when_not_given() {
        let args = base_args();
        assert!(build_filter(&args, WireExportFormat::Jsonl)
            .unwrap()
            .is_none());
    }
}
