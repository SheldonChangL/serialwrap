//! [`DeviceBackend`]: the seam between the protocol layer (this module's
//! siblings) and however devices are actually discovered/configured
//! (`TASKS.md` T1.4).
//!
//! [`LiveBackend`] wraps `port::PortConfigApi` + `port::SharedRecorders` —
//! the real `HotplugDetector`-backed production path. [`testing::TestBackend`]
//! is a plain in-memory double that lets protocol tests register a
//! `Recorder` directly (exactly the pattern `recorder.rs`'s and
//! `port_hotplug.rs`'s own tests already use for a "mock device") without
//! needing a running `HotplugDetector`/PTY/enumerator underneath — hotplug
//! detection itself is T1.1/T1.3's already-merged, already-tested territory
//! (see `port.rs`); this task only needs *some* device with a `Recorder`
//! behind it to prove the protocol layer's own behavior.
//!
//! Kept deliberately narrow: existence, config, and the `Recorder` handle.
//! Query state (line assembly, `wait_for`, `subscribe`) is a separate
//! concern layered on top by `protocol::registry::QueryRegistry` — see that
//! module — so this trait doesn't need to know anything about lines,
//! filters, or `wait_for` at all.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::error_counts::ErrorCounts;
use crate::port::{
    DeviceId, DeviceSummary, LeaseAcquired, LeaseError, LeaseReleased, PortConfigApi,
    SharedRecorders,
};
use crate::port_config::PortConfig;
use crate::recorder::Recorder;

/// What the protocol layer needs from "wherever devices come from" —
/// listing, config read/write, control lines, and a `Recorder` handle. See
/// the module docs for why this is intentionally not the same thing as
/// `HotplugDetector` itself.
pub trait DeviceBackend: Send + Sync {
    fn list_devices(&self) -> Vec<DeviceSummary>;
    fn recorder(&self, id: &DeviceId) -> Option<Arc<Recorder>>;
    fn get_config(&self, id: &DeviceId) -> io::Result<PortConfig>;
    /// Merge `patch` (a partial `PortConfig` field set — see
    /// `wrap_proto::Request::SetConfig`'s docs) onto the current
    /// configuration and apply it. Returns the resulting full config.
    fn set_config(
        &self,
        id: &DeviceId,
        patch: &serde_json::Map<String, serde_json::Value>,
        changed_by: &str,
    ) -> io::Result<PortConfig>;
    fn set_control_line(
        &self,
        id: &DeviceId,
        dtr: Option<bool>,
        rts: Option<bool>,
        changed_by: &str,
    ) -> io::Result<()>;
    fn dtr_pulse(&self, id: &DeviceId, duration: Duration, changed_by: &str) -> io::Result<()>;
    fn error_counts(&self, id: &DeviceId) -> io::Result<ErrorCounts>;
    /// Write `data` to the device's physical port (`TASKS.md` T2.1, issue
    /// #8). Callers (see `protocol::session`'s `Request::Write` handler)
    /// are responsible for the write-gate decision *before* ever calling
    /// this — by the time this runs, the request has already been
    /// determined allowed (today: human clients only, per the
    /// Security-model wiki's policy table; T4.1's rule engine is what
    /// decides for `agent`/`tool` later). This trait method only ever
    /// moves bytes; it never records a `tx` event itself (the caller does,
    /// since only the caller knows the requesting client's identity and
    /// the gate decision that authorized the write).
    fn write_bytes(&self, id: &DeviceId, data: &[u8]) -> io::Result<()>;

    /// Acquire a temporary, exclusive lease on `id`'s port for `command`
    /// (`TASKS.md` T2.2, issue #9): closes every fd this backend holds open
    /// for the device, appends a `lease_start` event, and returns the
    /// device's current path plus an opaque `token` for
    /// [`Self::release_lease`]. `pid` is the kernel-verified pid of the
    /// connection making the request (see `protocol::session`'s
    /// `Request::LeaseAcquire` handler) — not necessarily the pid of
    /// whatever the caller eventually execs, which the backend has no way
    /// to know at acquire time. `timeout_s`, if given, bounds how long the
    /// lease can stay open before the backend reclaims it on its own.
    fn acquire_lease(
        &self,
        id: &DeviceId,
        command: &str,
        pid: u32,
        timeout_s: Option<f64>,
    ) -> Result<LeaseAcquired, LeaseError>;

    /// End a lease previously granted by [`Self::acquire_lease`], identified
    /// by its opaque `token`. Reopens the device and resumes recording,
    /// appends a `lease_end` event (`reason: "released"`), and returns how
    /// long the lease was held.
    fn release_lease(&self, token: &str, exit_code: i32) -> Result<LeaseReleased, LeaseError>;
}

/// Merge a partial JSON config patch onto `current`, producing a new,
/// fully-specified [`PortConfig`]. Implemented via a JSON round trip
/// (serialize `current`, overwrite matching keys from `patch`, deserialize
/// back) rather than a field-by-field match, so this stays correct as
/// `PortConfig` grows fields — this crate's own type, not `wrap-proto`'s
/// (see `Request::SetConfig`'s docs on why the wire shape is a generic
/// map).
pub fn merge_config_patch(
    current: &PortConfig,
    patch: &serde_json::Map<String, serde_json::Value>,
) -> Result<PortConfig, String> {
    let mut value = serde_json::to_value(current).map_err(|e| e.to_string())?;
    if let serde_json::Value::Object(map) = &mut value {
        for (k, v) in patch {
            map.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Production backend: `HotplugDetector`'s `PortConfigApi` + `SharedRecorders`.
pub struct LiveBackend {
    config_api: PortConfigApi,
    recorders: SharedRecorders,
}

impl LiveBackend {
    pub fn new(config_api: PortConfigApi, recorders: SharedRecorders) -> Self {
        Self {
            config_api,
            recorders,
        }
    }
}

impl DeviceBackend for LiveBackend {
    fn list_devices(&self) -> Vec<DeviceSummary> {
        self.config_api.list_devices()
    }

    fn recorder(&self, id: &DeviceId) -> Option<Arc<Recorder>> {
        self.recorders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    fn get_config(&self, id: &DeviceId) -> io::Result<PortConfig> {
        self.config_api.get_config(id)
    }

    fn set_config(
        &self,
        id: &DeviceId,
        patch: &serde_json::Map<String, serde_json::Value>,
        changed_by: &str,
    ) -> io::Result<PortConfig> {
        let current = self.config_api.get_config(id)?;
        let merged = merge_config_patch(&current, patch)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.config_api
            .set_port_config(id, merged.clone(), changed_by)?;
        Ok(merged)
    }

    fn set_control_line(
        &self,
        id: &DeviceId,
        dtr: Option<bool>,
        rts: Option<bool>,
        changed_by: &str,
    ) -> io::Result<()> {
        if let Some(dtr) = dtr {
            self.config_api.set_dtr(id, dtr, changed_by)?;
        }
        if let Some(rts) = rts {
            self.config_api.set_rts(id, rts, changed_by)?;
        }
        Ok(())
    }

    fn dtr_pulse(&self, id: &DeviceId, duration: Duration, changed_by: &str) -> io::Result<()> {
        self.config_api.dtr_pulse(id, duration, changed_by)
    }

    fn error_counts(&self, id: &DeviceId) -> io::Result<ErrorCounts> {
        self.config_api.error_counts(id)
    }

    /// Writes through `PortConfigApi`'s already-open, shared fd — the same
    /// one `HotplugDetector`'s reader thread and every `set_dtr`/`dtr_pulse`
    /// ioctl already operate on (see `port::PortConfigApi::write_bytes`'s
    /// docs). Never a second, independently opened fd: T2.1's original
    /// implementation did open one (scope at the time excluded touching
    /// `port.rs`), but T2.2 (issue #9) needed that fixed first — lease
    /// mode's entire premise is that acquiring a lease closes *every* fd
    /// this daemon holds for the device, which a second write-path fd would
    /// silently defeat.
    fn write_bytes(&self, id: &DeviceId, data: &[u8]) -> io::Result<()> {
        self.config_api.write_bytes(id, data)
    }

    fn acquire_lease(
        &self,
        id: &DeviceId,
        command: &str,
        pid: u32,
        timeout_s: Option<f64>,
    ) -> Result<LeaseAcquired, LeaseError> {
        self.config_api.acquire_lease(id, command, pid, timeout_s)
    }

    fn release_lease(&self, token: &str, exit_code: i32) -> Result<LeaseReleased, LeaseError> {
        self.config_api.release_lease(token, exit_code)
    }
}

/// Test-only in-memory [`DeviceBackend`]. Not `#[cfg(test)]`-gated for the
/// same reason `port::testing` isn't: this crate's own `tests/*.rs`
/// integration tests need it too, and `cfg(test)` inside the library crate
/// is invisible there.
pub mod testing {
    use super::*;
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    /// [`TestBackend`]'s in-memory stand-in for `port::ActiveLease` (which
    /// is private to `port.rs`'s poll thread) — same fields, no channel
    /// round-trip needed since `TestBackend` has no poll thread at all.
    struct TestLease {
        token: String,
        command: String,
        pid: u32,
        started: Instant,
        deadline: Option<Instant>,
    }

    struct Entry {
        recorder: Arc<Recorder>,
        config: PortConfig,
        path: Option<std::path::PathBuf>,
        connected: bool,
        /// A writable fd a test registered via [`TestBackend::register_writer`]
        /// — e.g. the slave side of a raw PTY pair, so a write-path test can
        /// read back exactly what `write_bytes` sent from the pair's master
        /// side. `None` until a test opts in; `write_bytes` errors clearly
        /// rather than silently discarding bytes when it is.
        writer: Option<Mutex<File>>,
        /// This device's active lease, if any — see [`TestBackend::acquire_lease`].
        lease: Option<TestLease>,
    }

    /// Backs [`TestBackend`]'s lease tokens — only needs to be unique
    /// within one test process, same reasoning as `port::generate_lease_token`.
    static TEST_LEASE_TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A device registry with no real hardware underneath: tests
    /// `register()` a `Recorder` (typically backed by a `MockDevice` PTY —
    /// see `tests/protocol.rs`) directly under a chosen [`DeviceId`],
    /// instead of standing up a full `HotplugDetector`.
    #[derive(Default)]
    pub struct TestBackend {
        devices: Mutex<HashMap<DeviceId, Entry>>,
    }

    impl TestBackend {
        pub fn new() -> Self {
            Self::default()
        }

        /// Register `recorder` as device `id`, connected, with the default
        /// port configuration.
        pub fn register(&self, id: DeviceId, recorder: Arc<Recorder>) {
            self.devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id,
                    Entry {
                        recorder,
                        config: PortConfig::default(),
                        path: None,
                        connected: true,
                        writer: None,
                        lease: None,
                    },
                );
        }

        /// Set `id`'s reported device path — what [`TestBackend::acquire_lease`]
        /// hands back to a caller as the path to run an external tool
        /// against. A no-op if `id` was never `register`ed.
        pub fn set_path(&self, id: &DeviceId, path: PathBuf) {
            if let Some(entry) = self
                .devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(id)
            {
                entry.path = Some(path);
            }
        }

        /// Give `id` a real fd to write to, so `write_bytes` has somewhere
        /// to send bytes a test can read back independently (e.g. a raw
        /// PTY pair's slave side, with the test reading the master side) —
        /// `TASKS.md` T2.1's byte-exact acceptance criteria need this. A
        /// no-op if `id` was never `register`ed.
        pub fn register_writer(&self, id: &DeviceId, writer: File) {
            if let Some(entry) = self
                .devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(id)
            {
                entry.writer = Some(Mutex::new(writer));
            }
        }

        pub fn set_connected(&self, id: &DeviceId, connected: bool) {
            if let Some(entry) = self
                .devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(id)
            {
                entry.connected = connected;
            }
        }

        /// If `id` has an active lease whose deadline has passed, end it
        /// exactly the way [`Self::release_lease`] would (reopen — here,
        /// just flip `connected` back on — append `lease_end` with
        /// `reason: "timeout"`, drop the lease). `TestBackend` has no
        /// background poll thread to drive this on its own (unlike
        /// `port::HotplugDetector`, which checks every tick — see
        /// `reclaim_expired_leases`'s docs), so callers that need to
        /// observe a timeout must poll a method that calls this, which
        /// [`Self::list_devices`] does.
        fn maybe_expire_lease(&self, id: &DeviceId) {
            let (recorder, lease, path) = {
                let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
                let Some(entry) = devices.get_mut(id) else {
                    return;
                };
                let expired = entry
                    .lease
                    .as_ref()
                    .and_then(|l| l.deadline)
                    .is_some_and(|d| Instant::now() >= d);
                if !expired {
                    return;
                }
                let lease = entry.lease.take().expect("checked above");
                entry.connected = true;
                (Arc::clone(&entry.recorder), lease, entry.path.clone())
            };
            let duration_ms = lease.started.elapsed().as_millis() as u64;
            if let Err(e) = crate::port::append_lease_end_event(
                &recorder,
                id,
                path.as_deref(),
                &lease.command,
                lease.pid,
                &lease.token,
                None,
                duration_ms,
                "timeout",
            ) {
                eprintln!("TestBackend: failed to append lease_end (timeout) event: {e}");
            }
        }
    }

    impl DeviceBackend for TestBackend {
        fn list_devices(&self) -> Vec<DeviceSummary> {
            let ids: Vec<DeviceId> = self
                .devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .cloned()
                .collect();
            for id in &ids {
                self.maybe_expire_lease(id);
            }
            self.devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(id, e)| DeviceSummary {
                    id: id.clone(),
                    path: e.path.clone(),
                    connected: e.connected,
                    config: e.config.clone(),
                })
                .collect()
        }

        fn recorder(&self, id: &DeviceId) -> Option<Arc<Recorder>> {
            self.devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .map(|e| Arc::clone(&e.recorder))
        }

        fn get_config(&self, id: &DeviceId) -> io::Result<PortConfig> {
            self.devices
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .map(|e| e.config.clone())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
                })
        }

        fn set_config(
            &self,
            id: &DeviceId,
            patch: &serde_json::Map<String, serde_json::Value>,
            changed_by: &str,
        ) -> io::Result<PortConfig> {
            let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            let entry = devices.get_mut(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            let old = entry.config.clone();
            let merged = merge_config_patch(&entry.config, patch)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            entry.config = merged.clone();
            let recorder = Arc::clone(&entry.recorder);
            drop(devices);
            crate::device_profile::append_config_change_event(
                &recorder,
                Some(&old),
                &merged,
                changed_by,
            )?;
            Ok(merged)
        }

        fn set_control_line(
            &self,
            id: &DeviceId,
            dtr: Option<bool>,
            rts: Option<bool>,
            changed_by: &str,
        ) -> io::Result<()> {
            let devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            let entry = devices.get(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            if !entry.connected {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("device {} is not connected", id.0),
                ));
            }
            let recorder = Arc::clone(&entry.recorder);
            drop(devices);
            if let Some(dtr) = dtr {
                crate::device_profile::append_control_line_change_event(
                    &recorder, "dtr", dtr, changed_by,
                )?;
            }
            if let Some(rts) = rts {
                crate::device_profile::append_control_line_change_event(
                    &recorder, "rts", rts, changed_by,
                )?;
            }
            Ok(())
        }

        fn dtr_pulse(&self, id: &DeviceId, duration: Duration, changed_by: &str) -> io::Result<()> {
            let devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            let entry = devices.get(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            if !entry.connected {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("device {} is not connected", id.0),
                ));
            }
            let recorder = Arc::clone(&entry.recorder);
            drop(devices);
            crate::device_profile::append_dtr_pulse_event(
                &recorder,
                duration.as_millis() as u64,
                changed_by,
            )?;
            Ok(())
        }

        fn error_counts(&self, id: &DeviceId) -> io::Result<ErrorCounts> {
            let devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            let entry = devices.get(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            if !entry.connected {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("device {} is not connected", id.0),
                ));
            }
            Ok(ErrorCounts::Unavailable)
        }

        fn write_bytes(&self, id: &DeviceId, data: &[u8]) -> io::Result<()> {
            let devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            let entry = devices.get(id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("unknown device {}", id.0))
            })?;
            if !entry.connected {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!("device {} is not connected", id.0),
                ));
            }
            match &entry.writer {
                Some(w) => w.lock().unwrap_or_else(|e| e.into_inner()).write_all(data),
                None => Err(io::Error::other(format!(
                    "no writer registered for test device {} (see TestBackend::register_writer)",
                    id.0
                ))),
            }
        }

        fn acquire_lease(
            &self,
            id: &DeviceId,
            command: &str,
            pid: u32,
            timeout_s: Option<f64>,
        ) -> Result<LeaseAcquired, LeaseError> {
            let (recorder, path, token) = {
                let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
                let entry = devices.get_mut(id).ok_or(LeaseError::UnknownDevice)?;
                if let Some(existing) = &entry.lease {
                    return Err(LeaseError::AlreadyLeased {
                        holder: format!("pid {} running `{}`", existing.pid, existing.command),
                    });
                }
                if !entry.connected {
                    return Err(LeaseError::NotConnected);
                }
                let n = TEST_LEASE_TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
                let token = format!("test-lease-{n}");
                let path = entry
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(format!("/test/{}", id.0)));
                let started = Instant::now();
                let deadline = timeout_s
                    .filter(|s| s.is_finite() && *s > 0.0)
                    .map(|s| started + std::time::Duration::from_secs_f64(s));
                entry.connected = false;
                entry.lease = Some(TestLease {
                    token: token.clone(),
                    command: command.to_string(),
                    pid,
                    started,
                    deadline,
                });
                (Arc::clone(&entry.recorder), path, token)
            };
            if let Err(e) = crate::port::append_lease_start_event(
                &recorder, id, &path, command, pid, &token, timeout_s,
            ) {
                eprintln!("TestBackend: failed to append lease_start event: {e}");
            }
            Ok(LeaseAcquired { token, path })
        }

        fn release_lease(&self, token: &str, exit_code: i32) -> Result<LeaseReleased, LeaseError> {
            let (id, recorder, lease, path) = {
                let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
                let id = devices
                    .iter()
                    .find(|(_, e)| e.lease.as_ref().is_some_and(|l| l.token == token))
                    .map(|(id, _)| id.clone())
                    .ok_or(LeaseError::UnknownToken)?;
                let entry = devices.get_mut(&id).expect("found by the search above");
                let lease = entry.lease.take().expect("found by the search above");
                entry.connected = true;
                (id, Arc::clone(&entry.recorder), lease, entry.path.clone())
            };
            let duration_ms = lease.started.elapsed().as_millis() as u64;
            if let Err(e) = crate::port::append_lease_end_event(
                &recorder,
                &id,
                path.as_deref(),
                &lease.command,
                lease.pid,
                &lease.token,
                Some(exit_code),
                duration_ms,
                "released",
            ) {
                eprintln!("TestBackend: failed to append lease_end event: {e}");
            }
            Ok(LeaseReleased { duration_ms })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_config::{DataBits, PortConfig};

    #[test]
    fn merge_config_patch_overwrites_only_named_fields() {
        let current = PortConfig::default();
        let mut patch = serde_json::Map::new();
        patch.insert("baud".to_string(), 74880u32.into());
        let merged = merge_config_patch(&current, &patch).unwrap();
        assert_eq!(merged.baud, 74880);
        assert_eq!(
            merged.data_bits,
            DataBits::Eight,
            "untouched fields keep their current value"
        );
    }

    #[test]
    fn merge_config_patch_rejects_an_invalid_field_value() {
        let current = PortConfig::default();
        let mut patch = serde_json::Map::new();
        patch.insert("data_bits".to_string(), "not_a_real_variant".into());
        assert!(merge_config_patch(&current, &patch).is_err());
    }
}
