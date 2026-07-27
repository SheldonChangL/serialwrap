//! `serialwrap run [device] [--lease-timeout SECONDS] -- COMMAND...` (issue
//! #9 / `TASKS.md` T2.2): acquire a temporary, exclusive lease on a
//! device's port, run `COMMAND` against it with inherited stdio (so its own
//! progress output — e.g. `esptool`'s flashing percentage — is visible
//! directly), and release the lease once it exits, however that happens
//! (normal exit, `--lease-timeout`, or the process being killed out from
//! under this one).
//!
//! # Why lease, not a PTY
//!
//! Existing tools like `esptool`/`openocd`/`stm32flash` want a real device
//! node, not this project's protocol — and `esptool`'s bootloader entry
//! depends on precise DTR/RTS timing that does not reliably survive a PTY
//! (see the [Architecture wiki's "Lease versus PTY
//! passthrough"](https://github.com/SheldonChangL/serialwrap/wiki/Architecture)).
//! A lease is the honest alternative: the daemon really does close every fd
//! it holds for the device before this ever spawns the child (see
//! `serialwrapd::port::PortConfigApi::acquire_lease` — this is exactly why
//! T2.2 needed T2.1's write path fixed first to go through that same shared
//! fd, rather than a second one that would have left the daemon still
//! holding the port open), and the gap is marked explicitly in the event
//! stream (`lease_start`/`lease_end`) rather than silently passed through.
//!
//! # Concurrent clients are never disconnected
//!
//! Another `tail -f`/`subscribe` client stays connected for the whole lease
//! window — it just receives `lease_start`/`lease_end` as ordinary pushed
//! events instead of new data (see `serialwrapd::protocol::session`'s
//! `Request::LeaseAcquire`/`Request::LeaseRelease` handlers). Losing the
//! connection and "a lease is in progress" are different facts, and
//! conflating them is exactly the kind of silent misdiagnosis this
//! project's design tries to avoid — an agent reading a disconnect would
//! have no way to tell "the daemon crashed" apart from "someone is
//! flashing right now".
//!
//! # `--lease-timeout`
//!
//! Enforced from two independent places, on purpose: this process races the
//! child's own exit against a local timer and kills it if the timer wins,
//! *and* the daemon itself tracks the same deadline (see
//! `serialwrapd::port::HotplugDetector::reclaim_expired_leases`) and
//! reclaims the port on its own even if this process is killed before it
//! can act. Either one alone would leave a gap: this process's own timer
//! is useless if `serialwrap run` itself is killed; the daemon's alone
//! would still leave the actual child process running against a port the
//! daemon has since reopened underneath it.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use clap::Args;
use tokio::process::Command;

use wrap_proto::Request;

use super::client::{resolve_device, resolve_socket_path, DaemonClient};
use super::error::{describe_connect_error, describe_wire_error};

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Device id — optional only when exactly one device is known to the
    /// daemon (same resolution `tail`/`write`/`config` use).
    pub device: Option<String>,

    /// Kill the command and reclaim the port if it hasn't exited within
    /// this many seconds. The daemon enforces the same deadline on its own
    /// (see the module docs), so the port is reclaimed even if this
    /// process is killed before it can act on its own timer.
    #[arg(long = "lease-timeout", value_name = "SECONDS")]
    pub lease_timeout: Option<f64>,

    /// The command (and its arguments) to run against the leased device,
    /// given after `--`.
    #[arg(required = true, num_args = 1.., last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

pub async fn run(args: RunArgs) -> io::Result<()> {
    let socket_path = resolve_socket_path()?;
    let (mut client, _ack) = DaemonClient::connect(&socket_path, "serialwrap-run", "human")
        .await
        .map_err(|e| io::Error::new(e.kind(), describe_connect_error(&e, &socket_path)))?;

    let device = resolve_device(&mut client, args.device.as_deref()).await?;
    let command_line = args.command.join(" ");

    let reply = client
        .call(Request::LeaseAcquire {
            device: device.clone(),
            command: command_line,
            timeout_s: args.lease_timeout,
        })
        .await?;
    if reply["ok"].as_bool() != Some(true) {
        return Err(io::Error::other(describe_wire_error(
            &reply["error"],
            Some(&device),
        )));
    }
    let token = reply["token"]
        .as_str()
        .ok_or_else(|| io::Error::other("daemon did not return a lease token"))?
        .to_string();
    let path = reply["path"].as_str().unwrap_or_default().to_string();

    eprintln!(
        "serialwrap: leased {device} ({path}); running: {}",
        args.command.join(" ")
    );

    let mut cmd = Command::new(&args.command[0]);
    cmd.args(&args.command[1..]);
    // Convenience for whatever the command wants to do with the resolved
    // path — nothing here rewrites the caller's own argv, since a lease
    // isn't specific to any one tool's flag conventions.
    cmd.env("SERIALWRAP_LEASE_PATH", &path);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // The lease was already acquired (the port fd is already
            // closed) — release it before reporting the spawn failure so a
            // typo'd command name doesn't leave the device stuck leased.
            let _ = client
                .call(Request::LeaseRelease {
                    token,
                    exit_code: -1,
                })
                .await;
            return Err(e);
        }
    };

    let exit_status = match args.lease_timeout {
        Some(timeout) => {
            match tokio::time::timeout(Duration::from_secs_f64(timeout), child.wait()).await {
                Ok(status) => status?,
                Err(_elapsed) => {
                    eprintln!(
                    "serialwrap: --lease-timeout ({timeout}s) elapsed; killing the command and \
                     reclaiming {device}"
                );
                    let _ = child.kill().await;
                    child.wait().await?
                }
            }
        }
        None => child.wait().await?,
    };

    let wire_exit_code = wire_exit_code(exit_status);
    let release_reply = client
        .call(Request::LeaseRelease {
            token,
            exit_code: wire_exit_code,
        })
        .await;
    match release_reply {
        Ok(reply) if reply["ok"].as_bool() != Some(true) => {
            eprintln!(
                "serialwrap: warning: failed to report lease release to the daemon: {}",
                describe_wire_error(&reply["error"], Some(&device))
            );
        }
        Err(e) => {
            eprintln!("serialwrap: warning: failed to reach the daemon to release the lease: {e}");
        }
        Ok(_) => {}
    }

    std::process::exit(os_exit_code(exit_status));
}

/// The exit code recorded in the `lease_end` event's `exit_code` field:
/// the process's real exit code when it has one, or the negated signal
/// number when it was terminated by a signal (SIGKILL -> `-9`) — an
/// unambiguous, commonly used convention for "died to a signal" that a
/// human or agent reading the event stream can tell apart from a real
/// program-chosen exit code (which is always >= 0).
#[cfg(unix)]
fn wire_exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| -status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn wire_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// This process's own exit code: the child's real code when it has one, or
/// the conventional `128 + signal` shells use for a signal-terminated
/// child — distinct from [`wire_exit_code`] because the OS truncates
/// `std::process::exit`'s argument to a single byte, so a negative value
/// would not survive the way it does in the (signed, wire-level)
/// `lease_end` event.
#[cfg(unix)]
fn os_exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    }
}

#[cfg(not(unix))]
fn os_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}
