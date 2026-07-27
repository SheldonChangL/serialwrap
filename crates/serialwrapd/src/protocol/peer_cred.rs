//! Peer credentials: the kernel-reported pid of whoever connected to the
//! UDS socket (`TASKS.md` T1.4). This is the whole reason the daemon can
//! answer "which process just wrote that?" as a fact instead of trusting
//! the `name` a client volunteers in its `hello` message — see the wiki's
//! [Client protocol](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)
//! page: "The `pid` in the response is what the kernel reported, not what
//! the client claimed."
//!
//! Both platform implementations below go through `nix`'s `getsockopt`
//! rather than hand-rolled `libc::getsockopt` calls, because `nix` 0.31
//! already ships exactly the two sockopts this needs, verified directly
//! against its vendored source before writing this module:
//!
//! - Linux: `sockopt::PeerCredentials` (`SO_PEERCRED`), returning a
//!   `UnixCredentials` wrapping `libc::ucred { pid, uid, gid }`.
//! - macOS: `sockopt::LocalPeerPid` (`SOL_LOCAL`/`LOCAL_PEERPID`), returning
//!   the peer's `pid_t` directly. Deliberately *not*
//!   `sockopt::LocalPeerCred`/`LOCAL_PEERCRED` (also available on macOS,
//!   and what the wiki's prose names alongside `getpeereid`) — that sockopt
//!   returns a `struct xucred` (uid/gid only, no pid field at all on
//!   Darwin); `LOCAL_PEERPID` is the actual API that hands back the peer's
//!   pid, which is specifically what this project's acceptance criterion
//!   ("兩平台都能取得連線者真實 pid") needs.

use std::io;

use tokio::net::UnixStream;

#[cfg(target_os = "linux")]
pub fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let creds = getsockopt(stream, PeerCredentials)?;
    Ok(creds.pid() as u32)
}

#[cfg(target_os = "macos")]
pub fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};
    let pid = getsockopt(stream, LocalPeerPid)?;
    Ok(pid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("serialwrapd::protocol::peer_cred is only implemented for linux and macos");

#[cfg(test)]
mod tests {
    use super::*;

    /// The only thing testable without a second real process: connect to
    /// ourselves over a UDS pair and confirm the reported peer pid is
    /// *this* test process's own pid — exactly acceptance criterion 5
    /// ("測試斷言 pid 等於測試行程自己的 pid"), just against a loopback pair
    /// rather than the full daemon (the daemon-level version of this
    /// assertion lives in `tests/protocol.rs`).
    #[tokio::test]
    async fn peer_pid_of_a_self_connected_pair_is_this_process() {
        let (a, b) = UnixStream::pair().expect("create UDS pair");
        let my_pid = std::process::id();
        assert_eq!(peer_pid(&a).unwrap(), my_pid);
        assert_eq!(peer_pid(&b).unwrap(), my_pid);
    }
}
