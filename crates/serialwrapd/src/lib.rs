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
pub mod web;

use std::sync::Arc;

/// Entry point the `serialwrap daemon` subcommand calls: brings up hotplug
/// detection (T1.1) against the real system enumerator, the UDS protocol
/// server (T1.4) on the production socket path, and the embedded web GUI
/// (T5.1, issue #18) on `127.0.0.1` — and serves forever.
///
/// The web listener's bind failure is propagated (`?`), not
/// logged-and-skipped: per this project's stance against silently
/// half-working state (see `web::serve_on`'s doc comment), a daemon that
/// claims to have started but has no working browser endpoint is worse
/// than one that fails loudly at startup.
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

    let web_listener = tokio::net::TcpListener::bind(web::web_addr()).await?;
    let web_shared = Arc::clone(&shared);

    // Both futures loop forever absent a fatal error of their own kind
    // (an accept-loop failure for the UDS side, a bind/serve failure for
    // the web side) — `select!` means either one returning at all ends
    // `run`, which is intentional: neither half of "daemon" is optional.
    tokio::select! {
        () = protocol::server::serve(listener, shared) => {}
        result = web::serve_on(web_listener, web_shared) => {
            result?;
        }
    }

    // Reached only if one of the two servers above returns — kept so
    // `handle` has a clear owner and an explicit, orderly shutdown path
    // exists rather than an implicit process-exit teardown.
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
