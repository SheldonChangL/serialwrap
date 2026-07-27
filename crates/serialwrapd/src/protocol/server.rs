//! UDS listener: socket path resolution, `0600` permissions, and the
//! accept loop (`TASKS.md` T1.4).

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::UnixListener;

use super::backend::DeviceBackend;
use super::registry::{ClientRegistry, QueryRegistry};
use super::session;

/// State shared by every connection: the device backend, the per-device
/// query-state registry, and the client registry. One instance per running
/// daemon, held behind an `Arc` and cloned (cheaply — it's just the `Arc`)
/// into every spawned connection task.
pub struct Shared {
    pub backend: Arc<dyn DeviceBackend>,
    pub queries: QueryRegistry,
    pub clients: ClientRegistry,
    pub server_version: String,
}

impl Shared {
    pub fn new(backend: Arc<dyn DeviceBackend>, server_version: impl Into<String>) -> Self {
        Self {
            backend,
            queries: QueryRegistry::default(),
            clients: ClientRegistry::new(),
            server_version: server_version.into(),
        }
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
