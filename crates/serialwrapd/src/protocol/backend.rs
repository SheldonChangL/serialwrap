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
use crate::port::{DeviceId, DeviceSummary, PortConfigApi, SharedRecorders};
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

    /// # Known limitation: a second, independent fd
    ///
    /// This task's scope deliberately excludes touching `port.rs` (see
    /// `TASKS.md` T2.1's boundaries), so this cannot reach into
    /// `PortConfigApi`'s already-open, shared fd (the one
    /// `HotplugDetector`'s reader thread and every `set_dtr`/`dtr_pulse`
    /// ioctl already share). Instead this opens a *fresh*, independent,
    /// write-only fd at the device's current path — the same thing
    /// `mock_device::MockDevice::open_slave`'s docs describe as "as the
    /// daemon would when it opens a device node" — writes `data`, and lets
    /// it close again. This is safe (a tty's termios/baud/parity is a
    /// property of the line discipline itself, not of any one fd, so a
    /// freshly opened fd inherits whatever the shared fd already
    /// configured) but not ideal: a follow-up should instead expose the
    /// already-open fd through `PortConfigApi`, the same way
    /// `set_dtr`/`dtr_pulse` already do, and drop this second-fd approach.
    fn write_bytes(&self, id: &DeviceId, data: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        let summary = self
            .config_api
            .list_devices()
            .into_iter()
            .find(|d| &d.id == id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no such device: {}", id.0))
            })?;
        if !summary.connected {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} is not currently attached", id.0),
            ));
        }
        let path = summary.path.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("{} has no known device path", id.0),
            )
        })?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)?;
        file.write_all(data)
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
    use std::sync::Mutex;

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
    }

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
                    },
                );
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
    }

    impl DeviceBackend for TestBackend {
        fn list_devices(&self) -> Vec<DeviceSummary> {
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
