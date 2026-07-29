//! UDS listener: socket path resolution, `0600` permissions, and the
//! accept loop (`TASKS.md` T1.4).

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::UnixListener;

use crate::device_profile::ProfileStore;
use crate::gate::Gate;
use crate::query::DEFAULT_POLL_INTERVAL;

use super::backend::DeviceBackend;
use super::registry::{ClientRegistry, QueryRegistry};
use super::session;

/// State shared by every connection: the device backend, the per-device
/// query-state registry, the client registry, and the write gate. One
/// instance per running daemon, held behind an `Arc` and cloned (cheaply —
/// it's just the `Arc`) into every spawned connection task.
pub struct Shared {
    pub backend: Arc<dyn DeviceBackend>,
    pub queries: QueryRegistry,
    pub clients: ClientRegistry,
    pub server_version: String,
    /// The write gate (`TASKS.md` T4.1/T4.2, issues #14/#15). Defaults to
    /// [`Gate::builtin`] (built-in danger patterns, no whitelist, 60s
    /// timeout, real desktop notifications) — call [`Self::with_gate`] to
    /// override, e.g. with a loaded `rules.toml` (production, see
    /// `serialwrapd::run`) or a short-timeout/custom-notifier `Gate` for
    /// tests. Kept a plain field (not `pub` behind a getter) matching this
    /// struct's existing convention for `backend`/`clients`/`queries`.
    pub gate: Gate,
}

impl Shared {
    /// `data_dir` is where per-device state lives (`devices/<id>/...` —
    /// see `recorder.rs`'s and `device_profile.rs`'s "Storage layout"
    /// docs); it's needed here only so [`QueryRegistry`] can read a
    /// device's persisted line-terminator override (issue #52) the moment
    /// it first creates that device's query state — see
    /// [`QueryRegistry`]'s own docs for why this is a second,
    /// independent [`ProfileStore`] handle rather than one threaded
    /// through [`DeviceBackend`].
    pub fn new(
        backend: Arc<dyn DeviceBackend>,
        server_version: impl Into<String>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            backend,
            queries: QueryRegistry::new(
                DEFAULT_POLL_INTERVAL,
                Arc::new(ProfileStore::new(data_dir.into())),
            ),
            clients: ClientRegistry::new(),
            server_version: server_version.into(),
            gate: Gate::default(),
        }
    }

    /// Replace the default [`Gate`] — see [`Self::gate`]'s doc comment.
    /// Consuming/builder-style so production startup (`serialwrapd::run`)
    /// and tests can both write `Shared::new(...).with_gate(...)` inline
    /// rather than needing a separate constructor overload.
    pub fn with_gate(mut self, gate: Gate) -> Self {
        self.gate = gate;
        self
    }
}

/// Resolve the production socket path per the wiki: Linux
/// `$XDG_RUNTIME_DIR/serialwrap.sock`, macOS `~/.serialwrap/serialwrap.sock`.
pub fn default_socket_path() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "XDG_RUNTIME_DIR is not set"))?;
        Ok(PathBuf::from(dir).join("serialwrap.sock"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
        Ok(PathBuf::from(home)
            .join(".serialwrap")
            .join("serialwrap.sock"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        compile_error!("serialwrapd::protocol::server is only implemented for linux and macos");
    }
}

/// Bind a UDS listener at `path` with `0600` permissions (see the wiki's
/// Security model: "The socket is user-owned with `0600` permissions").
/// Removes any stale socket file left behind by a daemon that didn't shut
/// down cleanly first — otherwise `bind` would fail with `AddrInUse` even
/// though nothing is actually listening on it anymore.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Accept connections forever, handling each on its own spawned task. Only
/// returns (with the causing error logged) if `accept` itself fails
/// unrecoverably; a per-connection error never brings this loop down.
pub async fn serve(listener: UnixListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    session::handle_connection(stream, shared).await;
                });
            }
            Err(e) => {
                eprintln!("serialwrapd: protocol: accept failed: {e}");
            }
        }
    }
}
