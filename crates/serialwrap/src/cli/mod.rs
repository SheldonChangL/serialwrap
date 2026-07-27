//! Implementation of the `devices`/`tail` subcommands (issue #7 /
//! `TASKS.md` T1.5).
//!
//! Deliberately kept out of `main.rs` — see that file's module docs for
//! why: T3.1's MCP bridge (also `TASKS.md`) lands its own dispatch arms in
//! `main.rs` right after this task, and every line of subcommand logic
//! that lives here instead of there is one less line the two changes can
//! collide on.
//!
//! This crate is the top of the dependency graph (`serialwrap` ->
//! `serialwrapd` -> `wrap-proto`, per the workspace docs) and is the
//! *first* real client of the wire protocol `serialwrapd::protocol`/
//! `wrap-proto` define — everything here talks to the daemon only over the
//! UDS socket, the same way any future MCP bridge or GUI client will.

pub mod client;
pub mod clients;
pub mod config;
pub mod devices;
pub mod error;
pub mod export;
pub mod render;
pub mod run;
pub mod tail;
pub mod time;
pub mod write;

use std::io;

/// Turn a subcommand's `Result` into the process's actual exit behavior.
///
/// Success is silent. Failure prints exactly one actionable line to
/// stderr — see `error::describe_connect_error`/`error::describe_wire_error`
/// for what "actionable" means here, the same standard
/// `serialwrapd::port`'s `describe_open_error` sets for open failures —
/// and exits non-zero. Never a bare `Debug`-formatted `io::Error` dump,
/// which is what `#[tokio::main]` would print by default if a subcommand's
/// `Result` were simply returned from `main` unhandled.
pub fn dispatch(result: io::Result<()>) -> io::Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("serialwrap: {e}");
            std::process::exit(1);
        }
    }
}
