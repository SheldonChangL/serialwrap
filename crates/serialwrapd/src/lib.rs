//! Daemon core library for serialwrap.
//!
//! `serialwrapd` owns the serial device exclusively and is the only thing
//! that ever calls into termios/ioctl directly. Everything else (the CLI,
//! the MCP bridge, the web GUI) is a client that connects over the UDS
//! protocol defined in `wrap-proto`.
//!
//! This crate depends on `wrap-proto` and must never depend on the
//! `serialwrap` binary crate (dependency direction:
//! `serialwrap` -> `serialwrapd` -> `wrap-proto`).
//!
//! At this stage (`TASKS.md` T0.1) these are module skeletons only — no
//! device I/O, no recording, no protocol handling. See the module docs for
//! which later task fills each one in.

pub mod device_profile;
pub mod error_counts;
pub mod gate;
pub mod port;
pub mod port_config;
pub mod port_io;
pub mod presentation;
pub mod protocol;
pub mod query;
pub mod recorder;

use std::sync::Arc;

/// Entry point the `serialwrap daemon` subcommand calls: brings up hotplug
/// detection (T1.1) against the real system enumerator and the UDS
/// protocol server (T1.4) on the production socket path, and serves
/// forever.
///
/// CLI-level concerns (daemonizing, PID files, log destinations) are
/// T1.5's territory — this is the in-process daemon core only.
pub async fn run() -> std::io::Result<()> {
    let data_dir = recorder::default_data_dir()?;
    let detector = port::HotplugDetector::new(
        Box::new(port::SystemEnumerator::new()),
        data_dir,
        port::HotplugConfig::default(),
    );
    let backend = Arc::new(protocol::backend::LiveBackend::new(
        detector.port_config_api(),
        detector.recorders(),
    ));
    let handle = detector.spawn();

    let socket_path = protocol::default_socket_path()?;
    let listener = protocol::server::bind(&socket_path)?;
    let shared = Arc::new(protocol::Shared::new(backend, env!("CARGO_PKG_VERSION")));

    protocol::server::serve(listener, shared).await;

    // Unreachable in practice (`serve` loops forever absent a fatal accept
    // error, which it logs and continues past) — kept so `handle` has a
    // clear owner and an explicit, orderly shutdown path exists if `serve`
    // is ever changed to return.
    handle.stop();
    Ok(())
}
