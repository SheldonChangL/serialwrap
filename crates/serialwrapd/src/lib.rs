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

/// Test-only seam (`TASKS.md` T5.2, issue #19): the name of the env var
/// that switches [`run`] from the real `HotplugDetector`/`SystemEnumerator`
/// path to an in-memory [`protocol::backend::testing::TestBackend`] with
/// one device registered under this var's *value* as the [`port::DeviceId`].
///
/// # Why this exists, and why it's safe in a production binary
///
/// The Playwright E2E suite (`webui/e2e/`) needs *some* device with real
/// flowing data to drive T5.2's acceptance criteria (5,000 lines/sec
/// throughput, virtual-scroll/follow-pause, dedup/binary folding, TX/event
/// rendering) against the actual compiled `serialwrap daemon` binary — but
/// `HotplugDetector` only ever discovers real hardware
/// (`port::SystemEnumerator`), and there is no CLI flag or PTY plumbing
/// wired into the production binary to fake that (see `web::api`'s
/// `test_inject` handler, gated behind this exact same env var, for how the
/// E2E harness actually pushes records once this backend exists). Rather
/// than stand up a second, parallel "daemon-with-a-fake-port" code path,
/// [`run`] reuses `protocol::backend::testing::TestBackend` — already
/// public, already the exact seam this crate's own `tests/*.rs` integration
/// tests use for "a device with no real hardware underneath" — and gates it
/// on an env var an operator has no reason to ever set. A real deployment
/// never sets this, so `run()`'s behavior for every real user is completely
/// unchanged; this is the same "test-only knob, always compiled in, opt-in
/// via env var" shape `web::web_addr`'s `SERIALWRAP_WEB_PORT` already
/// established in this same file's neighborhood.
///
/// Deliberately *not* documented in `webui/README.md` or any user-facing
/// doc — this is CI/E2E-internal plumbing, not a supported feature.
pub const TEST_BACKEND_DEVICE_ENV: &str = "SERIALWRAP_TEST_BACKEND_DEVICE";

/// Entry point the `serialwrap daemon` subcommand calls: brings up hotplug
/// detection (T1.1) against the real system enumerator, the UDS protocol
/// server (T1.4) on the production socket path, and the embedded web GUI
/// (T5.1, issue #18) on `127.0.0.1` — and serves forever.
///
/// If [`TEST_BACKEND_DEVICE_ENV`] is set, hotplug detection and real device
/// I/O are skipped entirely in favor of an in-memory test backend — see
/// that constant's docs. Every other real-user code path is byte-for-byte
/// what this function did before T5.2.
pub async fn run() -> std::io::Result<()> {
    match std::env::var(TEST_BACKEND_DEVICE_ENV) {
        Ok(device_id) if !device_id.is_empty() => run_with_test_backend(device_id).await,
        _ => run_with_hotplug().await,
    }
}

/// The real production path: [`port::SystemEnumerator`]-backed hotplug
/// detection feeding a [`protocol::backend::LiveBackend`]. Extracted out of
/// [`run`] so [`run_with_test_backend`] can share [`serve_forever`] instead
/// of duplicating the listener/`Shared`/`select!` wiring.
async fn run_with_hotplug() -> std::io::Result<()> {
    let data_dir = recorder::default_data_dir()?;
    let detector = port::HotplugDetector::new(
        Box::new(port::SystemEnumerator::new()),
        data_dir,
        port::HotplugConfig::default(),
    );
    let backend: Arc<dyn protocol::backend::DeviceBackend> = Arc::new(
        protocol::backend::LiveBackend::new(detector.port_config_api(), detector.recorders()),
    );
    let handle = detector.spawn();
    serve_forever(backend).await?;
    // Unreachable in practice — see `serve_forever`'s doc comment for why
    // this line exists at all.
    handle.stop();
    Ok(())
}

/// The [`TEST_BACKEND_DEVICE_ENV`] path: one device, named `device_id`,
/// registered against a real [`recorder::Recorder`] (so `append_rx`/
/// `append_tx`/`append_event`/`append_gate` — and therefore the *real*
/// query/presentation/WS-push pipeline — all behave exactly as they would
/// against a real device) but with no hotplug detection, no PTY, and no
/// real port I/O anywhere underneath. See [`TEST_BACKEND_DEVICE_ENV`]'s
/// docs for why this exists and why it's a safe thing to compile into the
/// production binary.
async fn run_with_test_backend(device_id: String) -> std::io::Result<()> {
    let data_dir = recorder::default_data_dir()?;
    let recorder = Arc::new(recorder::Recorder::open(
        &data_dir,
        &device_id,
        recorder::RecorderConfig::default(),
    )?);
    let test_backend = protocol::backend::testing::TestBackend::new();
    test_backend.register(port::DeviceId(device_id), recorder);
    let backend: Arc<dyn protocol::backend::DeviceBackend> = Arc::new(test_backend);
    serve_forever(backend).await
}

/// Shared tail of [`run_with_hotplug`]/[`run_with_test_backend`]: bind the
/// web listener and the UDS socket, build [`protocol::Shared`], and serve
/// both forever. Identical to the body `run` had before T5.2 split it in
/// two, modulo `backend` now being a parameter instead of always a
/// freshly-built [`protocol::backend::LiveBackend`].
async fn serve_forever(backend: Arc<dyn protocol::backend::DeviceBackend>) -> std::io::Result<()> {
    // Bind the web listener *before* `protocol::server::bind`: the UDS
    // bind is destructive — it unconditionally unlinks whatever socket
    // file is already at that path, live daemon or not (see that
    // function's own doc comment) — while a TCP bind failure just fails.
    // Binding the safe one first means an accidental second `serialwrap
    // daemon` (its web port already taken by the first instance) exits
    // here, before ever touching the first instance's socket, instead of
    // unlinking a socket a perfectly healthy daemon is still listening on.
    let web_listener = tokio::net::TcpListener::bind(web::web_addr()).await?;

    let socket_path = protocol::default_socket_path()?;
    let listener = protocol::server::bind(&socket_path)?;
    let shared = Arc::new(
        protocol::Shared::new(backend, env!("CARGO_PKG_VERSION")).with_gate(production_gate()),
    );
    let web_shared = Arc::clone(&shared);

    // Both futures loop forever absent a fatal error of their own kind
    // (an accept-loop failure for the UDS side, a bind/serve failure for
    // the web side) — `select!` means either one returning at all ends
    // this function, which is intentional: neither half of "daemon" is
    // optional.
    tokio::select! {
        () = protocol::server::serve(listener, shared) => {}
        result = web::serve_on(web_listener, web_shared) => {
            result?;
        }
    }
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
