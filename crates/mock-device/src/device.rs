//! `MockDevice`: the test-facing handle to a PTY-backed fake serial device.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nix::unistd::write;

use crate::pty::open_raw_pty;
use crate::responder::{Pattern, PatternTable, Responder};

/// A PTY-backed stand-in for a real serial device.
///
/// The "device" side (this struct) writes to the PTY master; test code
/// reads from [`Self::open_slave`] the same way the real daemon will
/// eventually `open()` a device node. A background thread ([`Responder`])
/// watches for commands written to the slave and answers registered
/// patterns, independent of whatever scripted output the test is also
/// driving.
pub struct MockDevice {
    master: Option<Arc<OwnedFd>>,
    // Kept only to hold the slave-open-count above zero while "connected" —
    // see `pty::PtyPair::spare_slave`. Never read or written directly.
    spare_slave: Option<OwnedFd>,
    slave_path: PathBuf,
    patterns: PatternTable,
    responder: Option<Responder>,
}

impl MockDevice {
    /// Open a fresh PTY pair and start servicing commands.
    pub fn new() -> io::Result<Self> {
        let pair = open_raw_pty()?;
        let master = Arc::new(pair.master);
        let patterns: PatternTable = Arc::new(Mutex::new(Vec::new()));
        let responder = Responder::spawn(Arc::clone(&master), Arc::clone(&patterns));

        Ok(Self {
            master: Some(master),
            spare_slave: Some(pair.spare_slave),
            slave_path: pair.slave_path,
            patterns,
            responder: Some(responder),
        })
    }

    /// The slave's filesystem path — pass this to daemon test-mode code the
    /// same way a real device path would be handed to it.
    pub fn slave_path(&self) -> &Path {
        &self.slave_path
    }

    /// `true` unless [`Self::disconnect`] has been called without a
    /// following [`Self::reconnect`].
    pub fn is_connected(&self) -> bool {
        self.master.is_some()
    }

    /// Open the slave path read/write, as the daemon would when it opens a
    /// device node. `O_NOCTTY` avoids accidentally making the test process
    /// adopt the slave as its controlling terminal.
    pub fn open_slave(&self) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&self.slave_path)
    }

    /// Write bytes as if the device produced them — i.e. what a reader on
    /// the slave side (`open_slave`) sees as RX.
    pub fn write_device_output(&self, bytes: &[u8]) -> io::Result<()> {
        let master = self.master()?;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let n = write(master.as_fd(), remaining).map_err(io::Error::from)?;
            remaining = &remaining[n..];
        }
        Ok(())
    }

    /// Register a command pattern -> response. When the responder thread
    /// sees a complete line (whatever a "daemon" writes to the slave side)
    /// matching `pattern`, it writes `response` back on the same wire.
    pub fn on_command(&self, pattern: Pattern, response: impl Into<Vec<u8>>) {
        self.patterns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((pattern, response.into()));
    }

    /// Simulate a USB unplug: stop the responder thread and close the
    /// master side entirely. Any reader already holding the slave open (via
    /// `open_slave`) will see EOF or an I/O error on its next read —  which
    /// one is platform-dependent (see crate docs) — and the slave path
    /// itself stops accepting new opens, mirroring a real device node
    /// disappearing.
    pub fn disconnect(&mut self) -> io::Result<()> {
        if let Some(mut responder) = self.responder.take() {
            responder.stop();
        }
        // Dropping our own (now-last, since the responder thread already
        // dropped its clone in `stop()`) `Arc<OwnedFd>` closes the master
        // fd. Drop the spare slave fd too so the pair is fully torn down.
        self.master = None;
        self.spare_slave = None;
        Ok(())
    }

    /// Simulate USB replug: open a brand new PTY pair (a real replug can
    /// just as well surface under a different device path, e.g.
    /// `ttyUSB0 -> ttyUSB1`) and resume servicing the same registered
    /// command patterns.
    pub fn reconnect(&mut self) -> io::Result<()> {
        let pair = open_raw_pty()?;
        let master = Arc::new(pair.master);
        let responder = Responder::spawn(Arc::clone(&master), Arc::clone(&self.patterns));

        self.slave_path = pair.slave_path;
        self.master = Some(master);
        self.spare_slave = Some(pair.spare_slave);
        self.responder = Some(responder);
        Ok(())
    }

    fn master(&self) -> io::Result<&Arc<OwnedFd>> {
        self.master
            .as_ref()
            .ok_or_else(|| io::Error::new(ErrorKind::NotConnected, "mock device is disconnected"))
    }
}
