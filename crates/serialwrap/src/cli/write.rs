//! `serialwrap write [device] "text"` (issue #8 / `TASKS.md` T2.1): send
//! bytes to a device, as a `human` client — always audited, never gated
//! (see the [Security-model
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Security-model)'s
//! policy table; the daemon-side gate check itself lives in
//! `serialwrapd::protocol::session`'s `Request::Write` handler).
//!
//! Payload sources, in priority order: `--hex`, the positional text
//! argument, then stdin (read to EOF). Line endings (`-e`) apply only to
//! the positional/stdin *text* path — `--hex` and non-UTF-8 stdin bytes are
//! sent exactly as given, on the theory that a caller who spelled out exact
//! bytes wants exactly those bytes on the wire, nothing appended.
//!
//! # Disambiguating one positional token
//!
//! `[device] "text"` are both optional, which makes a *single* positional
//! token genuinely ambiguous — is `serialwrap write foo` device `foo` with
//! the payload on stdin, or text `foo` with the device auto-resolved? This
//! resolves it the same way common Unix tools (`cat`, `grep`) effectively
//! do: check whether stdin is actually piped/redirected
//! ([`std::io::IsTerminal`]). If it is, a lone token is the *device* and
//! the payload comes from stdin (`serialwrap write dev < commands.txt`,
//! `echo status | serialwrap write dev`); if stdin is an interactive
//! terminal (nothing sensible to read), a lone token is the *text* instead
//! (`serialwrap write "status"`), with the device auto-resolved.

use std::io::{self, IsTerminal as _, Read as _, Write as _};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use clap::Args;

use wrap_proto::{LineEnding, Request};

use super::client::{resolve_device, resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
#[value(rename_all = "lower")]
pub enum LineEndingArg {
    Lf,
    Crlf,
    Cr,
    None,
}

impl From<LineEndingArg> for LineEnding {
    fn from(v: LineEndingArg) -> Self {
        match v {
            LineEndingArg::Lf => LineEnding::Lf,
            LineEndingArg::Crlf => LineEnding::Crlf,
            LineEndingArg::Cr => LineEnding::Cr,
            LineEndingArg::None => LineEnding::None,
        }
    }
}

#[derive(Args, Debug)]
pub struct WriteArgs {
    /// `[device] "text"` — device id is optional only when exactly one
    /// device is known to the daemon (same resolution `serialwrap tail`
    /// uses); the text itself is optional too when `--hex` is given or the
    /// payload is piped in on stdin. See the module docs for exactly how a
    /// single positional token is disambiguated between the two.
    #[arg(value_name = "DEVICE_OR_TEXT", num_args = 0..=2)]
    pub positional: Vec<String>,

    /// How to terminate the text payload on the wire (never applied to
    /// `--hex` or to non-UTF-8 stdin input — see the module docs).
    #[arg(short = 'e', long = "line-ending", value_enum, default_value = "lf")]
    pub line_ending: LineEndingArg,

    /// Send exactly these bytes instead of text, e.g. `--hex "DE AD BE EF"`
    /// (whitespace between byte pairs is optional).
    #[arg(long)]
    pub hex: Option<String>,
}

enum Payload {
    Text(String),
    Bytes(Vec<u8>),
}

pub async fn run(args: WriteArgs) -> io::Result<()> {
    let stdin_is_piped = !io::stdin().is_terminal();
    let (device_arg, payload) =
        resolve_device_and_payload(args.hex.as_deref(), args.positional, stdin_is_piped)?;

    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-write", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let device = resolve_device(&mut client, device_arg.as_deref()).await?;

    let request = match payload {
        Payload::Text(text) => Request::Write {
            device: device.clone(),
            data_b64: None,
            text: Some(text),
            line_ending: args.line_ending.into(),
        },
        Payload::Bytes(bytes) => Request::Write {
            device: device.clone(),
            data_b64: Some(BASE64.encode(&bytes)),
            text: None,
            line_ending: LineEnding::None,
        },
    };

    let reply = client.call(request).await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(io::Error::other(describe_wire_error(
            &reply["error"],
            Some(&device),
        )));
    }
    let written = reply["written"].as_u64().unwrap_or(0);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "wrote {written} bytes to {device}")?;
    out.flush()
}

fn too_many_args_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "too many arguments — quote your text: `serialwrap write [device] \"text with spaces\"`",
    )
}

/// Resolve clap's 0/1/2 collected positional tokens (plus `--hex` and
/// whether stdin is actually piped) into `(device, payload)` — see the
/// module docs for the disambiguation rule a single token needs.
fn resolve_device_and_payload(
    hex: Option<&str>,
    mut positional: Vec<String>,
    stdin_is_piped: bool,
) -> io::Result<(Option<String>, Payload)> {
    if let Some(hex) = hex {
        let bytes = parse_hex(hex)?;
        return match positional.len() {
            0 => Ok((None, Payload::Bytes(bytes))),
            1 => Ok((
                Some(positional.pop().expect("len checked")),
                Payload::Bytes(bytes),
            )),
            _ => Err(too_many_args_error()),
        };
    }
    match positional.len() {
        0 => Ok((None, read_stdin_payload()?)),
        1 if stdin_is_piped => {
            // A real payload is waiting on stdin: the one token given must
            // be the device (there's nowhere else for it to go).
            Ok((
                Some(positional.pop().expect("len checked")),
                read_stdin_payload()?,
            ))
        }
        1 => {
            // No piped stdin to fall back to: the one token is the text,
            // device auto-resolved.
            Ok((None, Payload::Text(positional.pop().expect("len checked"))))
        }
        2 => {
            let text = positional.pop().expect("len checked");
            let device = positional.pop().expect("len checked");
            Ok((Some(device), Payload::Text(text)))
        }
        _ => Err(too_many_args_error()),
    }
}

fn read_stdin_payload() -> io::Result<Payload> {
    let mut buf = Vec::new();
    io::stdin().lock().read_to_end(&mut buf)?;
    // Strip exactly one trailing line terminator (LF or CRLF) — the same
    // "don't double up the ending" convention as `$(...)` command
    // substitution: the caller chose `-e` for what should terminate the
    // wire payload, not for whatever their input stream happened to end
    // with.
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    match String::from_utf8(buf) {
        Ok(text) => Ok(Payload::Text(text)),
        Err(e) => Ok(Payload::Bytes(e.into_bytes())),
    }
}

/// Parse a hex string like `"DE AD BE EF"` or `"deadbeef"` into raw bytes.
/// Whitespace between byte pairs is ignored; anything else invalid is an
/// actionable `InvalidInput` error, never a panic.
fn parse_hex(s: &str) -> io::Result<Vec<u8>> {
    let cleaned: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("--hex: odd number of hex digits in {s:?}"),
        ));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.chunks(2) {
        let hi = pair[0].to_digit(16).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--hex: invalid hex digit {:?} in {s:?}", pair[0]),
            )
        })?;
        let lo = pair[1].to_digit(16).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--hex: invalid hex digit {:?} in {s:?}", pair[1]),
            )
        })?;
        out.push(((hi << 4) | lo) as u8);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_kind(p: &Payload) -> &'static str {
        match p {
            Payload::Text(_) => "text",
            Payload::Bytes(_) => "bytes",
        }
    }

    #[test]
    fn parse_hex_ignores_whitespace_between_pairs() {
        assert_eq!(
            parse_hex("DE AD BE EF").unwrap(),
            vec![0xDE, 0xAD, 0xBE, 0xEF]
        );
        assert_eq!(parse_hex("deadbeef").unwrap(), vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }

    #[test]
    fn parse_hex_rejects_odd_length() {
        assert!(parse_hex("ABC").is_err());
    }

    #[test]
    fn parse_hex_rejects_invalid_digit() {
        assert!(parse_hex("ZZ").is_err());
    }

    #[test]
    fn one_token_is_text_when_stdin_is_a_terminal() {
        let (device, payload) =
            resolve_device_and_payload(None, vec!["hello".to_string()], false).unwrap();
        assert_eq!(device, None);
        assert_eq!(payload_kind(&payload), "text");
        match payload {
            Payload::Text(t) => assert_eq!(t, "hello"),
            _ => unreachable!(),
        }
    }

    #[test]
    fn two_tokens_are_always_device_then_text_regardless_of_stdin() {
        for stdin_is_piped in [false, true] {
            let (device, payload) = resolve_device_and_payload(
                None,
                vec!["dev1".to_string(), "hello".to_string()],
                stdin_is_piped,
            )
            .unwrap();
            assert_eq!(device, Some("dev1".to_string()));
            match payload {
                Payload::Text(t) => assert_eq!(t, "hello"),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn rejects_more_than_two_tokens() {
        assert!(
            resolve_device_and_payload(None, vec!["a".into(), "b".into(), "c".into()], false)
                .is_err()
        );
    }

    #[test]
    fn hex_with_one_token_treats_it_as_the_device() {
        let (device, payload) =
            resolve_device_and_payload(Some("DEAD"), vec!["dev1".to_string()], false).unwrap();
        assert_eq!(device, Some("dev1".to_string()));
        match payload {
            Payload::Bytes(b) => assert_eq!(b, vec![0xDE, 0xAD]),
            _ => unreachable!(),
        }
    }
}
