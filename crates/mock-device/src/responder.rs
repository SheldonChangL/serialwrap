//! Command responder: a background thread that reads whatever is written
//! to the slave side (i.e. what a "daemon" sends the device), matches it
//! line-by-line against registered patterns, and writes the matching
//! response back — the same way a real device replies to a command over
//! the same wire it received it on.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{read, write};

/// How to match an incoming command line (trailing `\r`/`\n` already
/// stripped) against a registered pattern.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// Line must equal this exactly.
    Exact(Vec<u8>),
    /// Line must start with this.
    Prefix(Vec<u8>),
}

impl Pattern {
    pub fn exact(s: impl AsRef<[u8]>) -> Self {
        Pattern::Exact(s.as_ref().to_vec())
    }

    pub fn prefix(s: impl AsRef<[u8]>) -> Self {
        Pattern::Prefix(s.as_ref().to_vec())
    }

    fn matches(&self, line: &[u8]) -> bool {
        match self {
            Pattern::Exact(p) => line == p.as_slice(),
            Pattern::Prefix(p) => line.starts_with(p.as_slice()),
        }
    }
}

pub type PatternTable = Arc<Mutex<Vec<(Pattern, Vec<u8>)>>>;

/// Background thread servicing registered command patterns for one PTY
/// pair's master fd. Polls with a short timeout so `stop()` can join it
/// promptly instead of blocking forever on a `read()` that may never see
/// more data.
pub struct Responder {
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Responder {
    pub fn spawn(master: Arc<OwnedFd>, patterns: PatternTable) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_for_thread = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            // `master` is moved in; this is the thread's own Arc clone. It
            // is dropped when this closure returns, which is what lets
            // `MockDevice::disconnect` actually close the underlying fd
            // once this thread has also let go of it.
            run_responder_loop(&master, &patterns, &shutdown_for_thread);
        });

        Responder {
            shutdown,
            handle: Some(handle),
        }
    }

    /// Signal the loop to stop and wait for it to actually exit (and drop
    /// its `Arc<OwnedFd>` clone), so the caller can safely drop its own
    /// clone afterwards and know the fd will really close.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_responder_loop(master: &Arc<OwnedFd>, patterns: &PatternTable, shutdown: &AtomicBool) {
    let mut line_buf: Vec<u8> = Vec::new();
    let mut read_buf = [0u8; 4096];

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }

        let mut fds = [PollFd::new(master.as_fd(), PollFlags::POLLIN)];
        // Short timeout so we re-check `shutdown` promptly rather than
        // blocking indefinitely on a master fd that may never see input.
        let poll_result = poll(&mut fds, PollTimeout::from(50u16));
        let ready = match poll_result {
            Ok(n) => n,
            Err(_) => return,
        };
        if ready == 0 {
            continue; // timed out, loop back to re-check shutdown
        }

        let revents = fds[0].revents().unwrap_or_else(PollFlags::empty);
        if revents.contains(PollFlags::POLLIN) {
            match read(master.as_fd(), &mut read_buf) {
                Ok(0) => return, // master side gone
                Ok(n) => {
                    line_buf.extend_from_slice(&read_buf[..n]);
                    process_complete_lines(&mut line_buf, master, patterns);
                }
                Err(_) => return,
            }
        } else if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
            return;
        }
    }
}

fn process_complete_lines(line_buf: &mut Vec<u8>, master: &Arc<OwnedFd>, patterns: &PatternTable) {
    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = line_buf.drain(..=pos).collect();
        line.pop(); // drop '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }

        let response = {
            let table = patterns.lock().unwrap_or_else(|e| e.into_inner());
            table
                .iter()
                .find(|(pattern, _)| pattern.matches(&line))
                .map(|(_, response)| response.clone())
        };
        if let Some(response) = response {
            let _: io::Result<usize> = write(master.as_fd(), &response).map_err(io::Error::from);
        }
    }
}
