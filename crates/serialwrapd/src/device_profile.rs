//! Per-device configuration profile: persistence keyed by `DeviceId`, plus
//! the event-append helpers that make every change to this shared,
//! per-device state auditable (`TASKS.md` T1.3, issue #5).
//!
//! # Config is shared, per-device state
//!
//! A `PortConfig` belongs to a device, not to whichever client last
//! touched it — there are no clients yet (T1.4 owns the UDS protocol),
//! but the storage/event design here is already shaped for that: one
//! profile per [`crate::port::DeviceId`], every change recorded with an
//! explicit `changed_by` string a future client layer supplies, and a
//! live re-application path (`port.rs`'s `PortConfigApi`) so a change
//! while connected affects the one shared fd everyone reads from — not a
//! per-connection copy.
//!
//! # Storage location
//!
//! `<data_dir>/devices/<device_id>/profile.json` — right alongside
//! `Recorder`'s own `segments/`, `index.jsonl`, and `.lock` for the same
//! device (see `recorder.rs`'s "Storage layout" docs). Chosen over a
//! separate top-level `config/` directory because it reuses the exact
//! same sanitized-device-id-as-directory-name convention `Recorder::open`
//! already established, so a device's recording and its profile live,
//! back up, and get deleted together under one path, without introducing
//! a second key scheme.
//!
//! # Event naming
//!
//! Three distinct event kinds, on purpose, so a future rule engine or
//! human audit trail (T4.1) can tell these apart without inspecting
//! payload contents:
//!
//! - `config_change` — a [`crate::port_config::PortConfig`] change
//!   (baud/data bits/parity/stop bits/flow control), with full old/new
//!   values and `changed_by`.
//! - `control_line_change` — a manual, single-line DTR or RTS
//!   assert/deassert.
//! - `dtr_pulse` — the independently-named reset-shaped operation the
//!   issue specifically calls out as *not* a `set_config` parameter, so it
//!   reads as "reset the board" rather than "changed a control line".
//!
//! # Old data is never reinterpreted
//!
//! None of the functions here ever read, rewrite, or otherwise touch
//! previously-recorded `rx`/`tx` records — `Recorder` only ever gets
//! `append_event` calls from this module. Changing baud does not, and
//! structurally cannot, alter how previously-stored bytes are interpreted
//! (see `recorder.rs`'s "Write semantics": it records bytes, not
//! characters).

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::port_config::PortConfig;
use crate::recorder::Recorder;

/// One device's persisted configuration. Currently just [`PortConfig`];
/// a `struct` (rather than a bare type alias) so future per-device
/// settings (e.g. a friendly name) have somewhere to go without changing
/// every call site.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeviceProfile {
    pub config: PortConfig,
}

/// Persists [`DeviceProfile`]s under `<data_dir>/devices/<device_id>/profile.json`.
/// See the module docs for why this location.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    data_dir: PathBuf,
}

impl ProfileStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    fn profile_path(&self, device_id: &str) -> PathBuf {
        self.data_dir
            .join("devices")
            .join(device_id)
            .join("profile.json")
    }

    /// `Ok(None)` if no profile has ever been saved for this device — the
    /// caller should fall back to [`PortConfig::default`].
    pub fn load(&self, device_id: &str) -> io::Result<Option<DeviceProfile>> {
        let path = self.profile_path(device_id);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Persist `profile` for `device_id`, creating parent directories as
    /// needed. Writes to a temp file and renames over the real path — an
    /// atomic replace on the same filesystem — so a crash mid-write can
    /// never leave a torn `profile.json` as the only copy for the next
    /// `load` to choke on (same "never leave a half-written file behind"
    /// principle `recorder.rs` applies to its own segments).
    pub fn save(&self, device_id: &str, profile: &DeviceProfile) -> io::Result<()> {
        let path = self.profile_path(device_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(profile)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp_path = path.with_extension("json.tmp");
        fs::write(&tmp_path, &bytes)?;
        fs::rename(&tmp_path, &path)?;
        Ok(())
    }
}

/// Append a `config_change` event: full old/new [`PortConfig`] values and
/// who changed it. `old: None` means "no config has ever been applied to
/// this device before" (its very first connect, with no saved profile).
pub fn append_config_change_event(
    recorder: &Recorder,
    old: Option<&PortConfig>,
    new: &PortConfig,
    changed_by: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert(
        "old".to_string(),
        old.map(|c| serde_json::to_value(c).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null),
    );
    extra.insert(
        "new".to_string(),
        serde_json::to_value(new).unwrap_or(serde_json::Value::Null),
    );
    extra.insert("changed_by".to_string(), changed_by.into());
    recorder.append_event("config_change", extra)?;
    Ok(())
}

/// Append a `control_line_change` event: manual DTR/RTS assert/deassert —
/// distinct from `dtr_pulse` (see module docs).
pub fn append_control_line_change_event(
    recorder: &Recorder,
    line: &str,
    level: bool,
    changed_by: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("line".to_string(), line.into());
    extra.insert("level".to_string(), level.into());
    extra.insert("changed_by".to_string(), changed_by.into());
    recorder.append_event("control_line_change", extra)?;
    Ok(())
}

/// Append a `dtr_pulse` event — independently named per this task's spec
/// (see module docs).
pub fn append_dtr_pulse_event(
    recorder: &Recorder,
    duration_ms: u64,
    changed_by: &str,
) -> io::Result<()> {
    let mut extra = Map::new();
    extra.insert("duration_ms".to_string(), duration_ms.into());
    extra.insert("changed_by".to_string(), changed_by.into());
    recorder.append_event("dtr_pulse", extra)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port_config::{DataBits, FlowControl, OpenControlLines, Parity, StopBits};
    use crate::recorder::RecorderConfig;
    use wrap_proto::Record;

    fn custom_config() -> PortConfig {
        PortConfig {
            baud: 74_880,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::Hardware,
            open_control_lines: OpenControlLines::Assert {
                dtr: true,
                rts: false,
            },
        }
    }

    // ---- Acceptance criterion 4: persistence + reconnect application ----

    #[test]
    fn profile_saved_then_reloaded_matches_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        assert!(store.load("dev-1").unwrap().is_none(), "nothing saved yet");

        let profile = DeviceProfile {
            config: custom_config(),
        };
        store.save("dev-1", &profile).unwrap();

        let loaded = store
            .load("dev-1")
            .unwrap()
            .expect("profile must load back");
        assert_eq!(loaded, profile);
    }

    #[test]
    fn profile_is_stored_alongside_the_recorder_directory_for_the_same_device() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        store
            .save(
                "dev-1",
                &DeviceProfile {
                    config: custom_config(),
                },
            )
            .unwrap();

        let expected = tmp
            .path()
            .join("devices")
            .join("dev-1")
            .join("profile.json");
        assert!(expected.is_file(), "expected profile.json at {expected:?}");
    }

    #[test]
    fn saving_a_second_time_overwrites_the_first_no_stale_leftover() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ProfileStore::new(tmp.path());
        store
            .save(
                "dev-1",
                &DeviceProfile {
                    config: PortConfig::default(),
                },
            )
            .unwrap();
        store
            .save(
                "dev-1",
                &DeviceProfile {
                    config: custom_config(),
                },
            )
            .unwrap();

        let loaded = store.load("dev-1").unwrap().unwrap();
        assert_eq!(loaded.config.baud, 74_880);
        // No leftover temp file from the atomic rename.
        let dir = tmp.path().join("devices").join("dev-1");
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["profile.json".to_string()]);
    }

    // ---- Acceptance criterion 3: config_change carries old/new + changed_by ----

    #[test]
    fn config_change_event_carries_full_old_and_new_values_and_changed_by() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        let old = PortConfig::default();
        let new = custom_config();
        append_config_change_event(&recorder, Some(&old), &new, "cli:sheldon").unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let (extra_old, extra_new, changed_by) = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "config_change" => Some((
                    extra.get("old").cloned().unwrap(),
                    extra.get("new").cloned().unwrap(),
                    extra
                        .get("changed_by")
                        .and_then(|v| v.as_str())
                        .unwrap()
                        .to_string(),
                )),
                _ => None,
            })
            .expect("expected a config_change event");

        assert_eq!(extra_old, serde_json::to_value(&old).unwrap());
        assert_eq!(extra_new, serde_json::to_value(&new).unwrap());
        assert_eq!(extra_new.get("baud").and_then(|v| v.as_u64()), Some(74_880));
        assert_eq!(changed_by, "cli:sheldon");
    }

    #[test]
    fn config_change_event_with_no_prior_config_records_old_as_null() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        append_config_change_event(&recorder, None, &PortConfig::default(), "system:connect")
            .unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let extra_old = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "config_change" => {
                    Some(extra.get("old").cloned().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert!(extra_old.is_null());
    }

    #[test]
    fn control_line_change_event_carries_line_level_and_changed_by() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        append_control_line_change_event(&recorder, "dtr", true, "agent:claude").unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let extra = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "control_line_change" => {
                    Some(extra.clone())
                }
                _ => None,
            })
            .expect("expected a control_line_change event");
        assert_eq!(extra.get("line").and_then(|v| v.as_str()), Some("dtr"));
        assert_eq!(extra.get("level").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            extra.get("changed_by").and_then(|v| v.as_str()),
            Some("agent:claude")
        );
    }

    #[test]
    fn dtr_pulse_event_is_distinct_from_control_line_change_and_config_change() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        append_dtr_pulse_event(&recorder, 50, "cli:sheldon").unwrap();

        let records = recorder.read_since(0, usize::MAX).unwrap().records;
        let kinds: Vec<&str> = records
            .iter()
            .filter_map(|r| match r {
                Record::Event { event, .. } => Some(event.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["dtr_pulse"]);

        let extra = records
            .iter()
            .find_map(|r| match r {
                Record::Event { event, extra, .. } if event == "dtr_pulse" => Some(extra.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(extra.get("duration_ms").and_then(|v| v.as_u64()), Some(50));
        assert_eq!(
            extra.get("changed_by").and_then(|v| v.as_str()),
            Some("cli:sheldon")
        );
    }

    // ---- Acceptance criterion 5: old data is never reinterpreted ----

    #[test]
    fn appending_config_change_events_never_alters_previously_recorded_rx_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let recorder = Recorder::open(tmp.path(), "dev", RecorderConfig::default()).unwrap();

        let before: Vec<Vec<u8>> = (0..10)
            .map(|i| format!("boot log line {i}\n").into_bytes())
            .collect();
        for line in &before {
            recorder.append_rx(line).unwrap();
        }
        let before_records = recorder.read_since(0, usize::MAX).unwrap().records;

        // Change baud (and every other setting) several times.
        append_config_change_event(&recorder, None, &PortConfig::default(), "system:connect")
            .unwrap();
        append_config_change_event(
            &recorder,
            Some(&PortConfig::default()),
            &custom_config(),
            "cli:sheldon",
        )
        .unwrap();
        append_config_change_event(
            &recorder,
            Some(&custom_config()),
            &PortConfig {
                baud: 115_200,
                ..PortConfig::default()
            },
            "cli:sheldon",
        )
        .unwrap();

        // Every previously-recorded rx record must be byte-for-byte
        // unchanged — same seq, same t_mono/t_wall, same data_b64.
        let after_records = recorder.read_since(0, usize::MAX).unwrap().records;
        for (idx, original) in before_records.iter().enumerate() {
            assert_eq!(
                &after_records[idx], original,
                "record at index {idx} must be untouched by config changes"
            );
        }

        // And the decoded bytes themselves still match what was written,
        // proving the *content*, not just the record wrapper, survives.
        for (i, expected) in before.iter().enumerate() {
            match &after_records[i] {
                Record::Rx { data_b64, .. } => {
                    use base64::engine::general_purpose::STANDARD as BASE64;
                    use base64::Engine as _;
                    let decoded = BASE64.decode(data_b64).unwrap();
                    assert_eq!(&decoded, expected);
                }
                other => panic!("expected an Rx record, got {other:?}"),
            }
        }
    }
}
