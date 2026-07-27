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
pub mod export;
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
    let shared = Arc::new(
        protocol::Shared::new(backend, env!("CARGO_PKG_VERSION")).with_gate(production_gate()),
    );

    protocol::server::serve(listener, shared).await;

    // Unreachable in practice (`serve` loops forever absent a fatal accept
    // error, which it logs and continues past) — kept so `handle` has a
    // clear owner and an explicit, orderly shutdown path exists if `serve`
    // is ever changed to return.
    handle.stop();
    Ok(())
}

/// Build the write gate's production [`gate::Gate`]: `rules.toml` at
/// [`gate::rules::default_rules_path`] if one exists and parses, falling
/// back to [`gate::rules::RuleSet::builtin`] otherwise — including on a
/// malformed file, deliberately fail-safe rather than refusing to start the
/// whole daemon over a typo in an operator's hand-edited danger list (see
/// `RuleSet::load`'s doc comment). Either way the built-in danger patterns
/// are never less protected than "falls back to them", only ever extended
/// by whatever `rules.toml` adds.
fn production_gate() -> gate::Gate {
    let rules = gate::rules::default_rules_path()
        .and_then(|path| gate::rules::RuleSet::load(&path))
        .unwrap_or_else(|e| {
            eprintln!(
                "serialwrapd: gate: could not load rules.toml ({e}); falling back to built-in \
                 danger patterns with no whitelist"
            );
            gate::rules::RuleSet::builtin()
        });
    gate::Gate::new(rules, std::sync::Arc::new(gate::notify::DesktopNotifier))
}
