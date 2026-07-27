//! Desktop notification for a newly-pending write (`TASKS.md` T4.2, issue
//! #15). macOS gets `osascript`; Linux gets `notify-send`.
//!
//! # Why a failure here can never affect the approval flow
//!
//! [`Notifier::notify`] returns `()`, not `Result` — there is no error for
//! a caller to even consider propagating, by construction. [`Gate::submit_write`](super::Gate::submit_write)
//! additionally fires it via `tokio::task::spawn_blocking` and never awaits
//! (or otherwise observes) the spawned task's outcome: the pending entry is
//! already in the queue and its timeout task already scheduled before the
//! notification is even kicked off, so a `notify-send` binary that's
//! missing, a headless session with no notification daemon running, or a
//! notifier implementation that panics outright, can *at absolute worst*
//! mean the human never sees a popup — the request still sits in
//! `serialwrap approvals list` and still times out on schedule either way.
//! This is exactly the acceptance criterion ("通知失效不影響流程") and is
//! covered by an integration test using [`FailingNotifier`] below rather
//! than by convention alone.

use std::process::Command;

/// Something that can tell a human "a write needs your approval". A trait
/// (rather than calling `osascript`/`notify-send` directly from `Gate`) so
/// tests can inject a double that deliberately fails, without needing an
/// actual desktop session — this crate has no way to verify a real popup
/// appeared (see `docs/manual-checklist.md` section 4 for the one that
/// does, which needs a human at a real desktop).
pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

/// Real desktop notifications: `osascript -e 'display notification'` on
/// macOS, `notify-send` on Linux. Any failure (binary missing, no display
/// server, no notification daemon registered on the session bus, etc.) is
/// swallowed here — see the module docs for why that's safe rather than
/// lossy in a way that matters.
pub struct DesktopNotifier;

impl Notifier for DesktopNotifier {
    #[cfg(target_os = "macos")]
    fn notify(&self, title: &str, body: &str) {
        // AppleScript string literals: escape `"` and `\` so a requester
        // name or command text containing either can't break out of the
        // quoted literal (this is display text only, never executed as a
        // command by anything downstream — but a malformed script would
        // still make `osascript` fail, which is the one failure mode this
        // function bothers to avoid causing unnecessarily).
        fn escape(s: &str) -> String {
            s.replace('\\', "\\\\").replace('"', "\\\"")
        }
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape(body),
            escape(title)
        );
        let _ = Command::new("osascript").arg("-e").arg(script).status();
    }

    #[cfg(target_os = "linux")]
    fn notify(&self, title: &str, body: &str) {
        let _ = Command::new("notify-send").arg(title).arg(body).status();
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn notify(&self, _title: &str, _body: &str) {
        // No supported desktop notification backend on this platform —
        // same "never break the approval flow" contract applies: silently
        // do nothing rather than fail to compile or panic.
    }
}
