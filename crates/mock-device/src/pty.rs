//! Low-level PTY plumbing.
//!
//! Opens a fresh master/slave pair via `openpty()` and puts the shared line
//! discipline into raw mode: no echo, no canonical line buffering, no
//! signal-generating control characters, and no output post-processing
//! (which would otherwise rewrite `\n` to `\r\n` and corrupt byte-exact
//! assertions on scripted binary payloads).

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use nix::pty::openpty;
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};

/// A freshly opened PTY pair: the master fd, the slave's filesystem path,
/// and a spare slave fd kept only to hold the "connection" open.
///
/// A pty's master sees a hangup condition (`POLLHUP`, and a `read()` of 0)
/// whenever the slave side has *zero* open file descriptors — that's normal
/// POSIX tty behavior, not specific to this fixture. Between opening this
/// pair and the caller's first [`crate::MockDevice::open_slave`] call there
/// would otherwise be a window with no slave fd open at all, which the
/// responder thread's very first poll can observe as a spurious "device
/// gone" before anyone ever tried to talk to it. `spare_slave` exists
/// purely to keep the slave-open-count above zero for as long as the pair
/// is "connected"; nothing reads or writes through it directly, and it
/// doesn't stop the caller from opening additional, independent fds on the
/// same path.
pub struct PtyPair {
    pub master: OwnedFd,
    pub spare_slave: OwnedFd,
    pub slave_path: PathBuf,
}

/// Open a new PTY pair and configure it for raw, byte-exact I/O.
pub fn open_raw_pty() -> io::Result<PtyPair> {
    let result = openpty(None, None).map_err(io::Error::from)?;

    let mut attrs = tcgetattr(&result.slave).map_err(io::Error::from)?;
    cfmakeraw(&mut attrs);
    tcsetattr(&result.slave, SetArg::TCSANOW, &attrs).map_err(io::Error::from)?;

    let slave_path = ttyname(result.slave.as_raw_fd())?;

    Ok(PtyPair {
        master: result.master,
        spare_slave: result.slave,
        slave_path,
    })
}

/// Resolve the filesystem path of an open tty fd via POSIX `ttyname_r`.
///
/// This works uniformly on macOS and Linux, unlike a `/proc/self/fd`
/// readlink (Linux-only) or `nix::pty::ptsname` (tied to the
/// `posix_openpt`/`ptsname` flow rather than the BSD-style `openpty()` this
/// module uses) — `ttyname_r` is POSIX and only cares that the fd refers to
/// a tty.
fn ttyname(fd: RawFd) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `fd` is a valid, currently-open fd (the slave side of the PTY
    // pair we just created above), and `buf` is a correctly sized buffer
    // for `ttyname_r` to write a NUL-terminated path into per POSIX.
    let rc = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(nul);
    Ok(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
}
