//! Actionable error messages for this CLI's own connect/request failures.
//!
//! Mirrors the style `serialwrapd::port`'s `describe_open_error` already
//! set for T1.3 (named explicitly in issue #7 as the reference): every
//! message says what concretely to do next, never just what went wrong —
//! "daemon 沒跑、socket 不存在、裝置不存在——訊息要指出具體處置，不是 raw
//! error."

use std::io;
use std::path::Path;

use serde_json::Value;

/// Describe a failure to connect to the daemon's UDS socket at `path`.
/// `UnixStream::connect`'s `io::Error` already carries the right
/// [`io::ErrorKind`] for the common cases (mapped from `errno` by the
/// standard library) — this only adds the concrete next step for each one,
/// the same `ErrorKind`/`raw_os_error`-matching shape `describe_open_error`
/// uses.
pub fn describe_connect_error(err: &io::Error, path: &Path) -> String {
    let path = path.display();
    match err.kind() {
        io::ErrorKind::NotFound => format!(
            "cannot reach the serialwrap daemon: no socket at {path} — it isn't running yet; \
             start it with `serialwrap daemon` (in another terminal or as a background service), \
             then retry"
        ),
        io::ErrorKind::ConnectionRefused => format!(
            "cannot reach the serialwrap daemon: nothing is listening on {path} — this is \
             usually a stale socket file left by a daemon that didn't shut down cleanly; run \
             `serialwrap daemon` again (it removes and rebinds a stale socket itself)"
        ),
        io::ErrorKind::PermissionDenied => format!(
            "cannot reach the serialwrap daemon: permission denied connecting to {path} — the \
             socket is user-owned (mode 0600); make sure you're running as the same user that \
             started `serialwrap daemon`"
        ),
        _ => format!("cannot reach the serialwrap daemon at {path}: {err}"),
    }
}

/// Describe a `{"ok": false, "error": {...}}` wire reply — the JSON shape
/// of `wrap_proto::WireError` — as an actionable message. Read back as
/// plain JSON (via `serde_json::Value`) rather than deserializing into
/// `WireError` itself: this crate talks to the daemon only over the wire,
/// never by depending on `serialwrapd`'s request-handling internals, and
/// matching on `code` as a string keeps this forward-compatible with a
/// future error code this build doesn't recognize yet (falls through to
/// the generic arm instead of failing to deserialize).
pub fn describe_wire_error(error: &Value, device: Option<&str>) -> String {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let device = device.unwrap_or("<unspecified>");
    match code {
        "device_not_found" => format!(
            "device {device:?} not found — run `serialwrap devices` to see what the daemon \
             currently knows about; a device must be plugged in (or have been seen before) for a \
             record stream to exist"
        ),
        "device_disconnected" => format!(
            "device {device:?} is known but not currently connected — its recorded history is \
             still readable, but no new data will arrive until it's replugged"
        ),
        "data_aged_out" => match error.get("oldest_available_seq").and_then(Value::as_u64) {
            Some(seq) => format!(
                "the requested position has aged out of the recording's ring buffer — the \
                 oldest data still available starts at seq {seq}; drop `--since` (or use a more \
                 recent one) to see what's still there"
            ),
            None => {
                "the requested position has aged out of the recording's ring buffer".to_string()
            }
        },
        _ if message.is_empty() => format!("daemon rejected the request ({code})"),
        _ => format!("daemon rejected the request ({code}): {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path() -> PathBuf {
        PathBuf::from("/tmp/does-not-matter.sock")
    }

    #[test]
    fn not_found_names_starting_the_daemon() {
        let err = io::Error::from(io::ErrorKind::NotFound);
        let msg = describe_connect_error(&err, &path());
        assert!(msg.contains("serialwrap daemon"), "message was: {msg}");
        assert!(msg.contains("isn't running"), "message was: {msg}");
    }

    #[test]
    fn connection_refused_names_a_stale_socket() {
        let err = io::Error::from(io::ErrorKind::ConnectionRefused);
        let msg = describe_connect_error(&err, &path());
        assert!(msg.contains("stale socket"), "message was: {msg}");
    }

    #[test]
    fn permission_denied_names_the_owning_user() {
        let err = io::Error::from(io::ErrorKind::PermissionDenied);
        let msg = describe_connect_error(&err, &path());
        assert!(msg.contains("same user"), "message was: {msg}");
    }

    #[test]
    fn device_not_found_points_at_the_devices_subcommand() {
        let error =
            serde_json::json!({"code": "device_not_found", "message": "no such device: dev"});
        let msg = describe_wire_error(&error, Some("dev"));
        assert!(msg.contains("serialwrap devices"), "message was: {msg}");
        assert!(msg.contains("dev"), "message was: {msg}");
    }

    #[test]
    fn data_aged_out_includes_the_oldest_available_seq() {
        let error = serde_json::json!({"code": "data_aged_out", "message": "x", "oldest_available_seq": 4096});
        let msg = describe_wire_error(&error, Some("dev"));
        assert!(msg.contains("4096"), "message was: {msg}");
    }

    #[test]
    fn unrecognized_code_still_surfaces_the_message_not_a_panic() {
        let error = serde_json::json!({"code": "brand_new_future_code", "message": "details here"});
        let msg = describe_wire_error(&error, None);
        assert!(msg.contains("brand_new_future_code"), "message was: {msg}");
        assert!(msg.contains("details here"), "message was: {msg}");
    }
}
