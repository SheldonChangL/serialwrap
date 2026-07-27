//! Two bits of shared state every connection needs beyond [`super::backend::DeviceBackend`]
//! itself (`TASKS.md` T1.4):
//!
//! - [`QueryRegistry`]: lazily creates and caches one
//!   [`crate::query::DeviceQueryState`] (+ background poller) per device,
//!   shared by every connection that ever asks about that device — see
//!   `query.rs`'s module docs for why sharing (not one per connection) is
//!   what makes concurrent subscribers consistent.
//! - [`ClientRegistry`]: the `list_clients`/`kick`/`demote` session
//!   management table, keyed by a daemon-assigned `client_id` (distinct
//!   from the kernel pid — a client's pid is a fact about *who* connected;
//!   `client_id` is just this registry's own row key).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wrap_proto::{ClientType, Permission};

use crate::port::DeviceId;
use crate::query::{spawn_poller, DeviceQueryState, DEFAULT_POLL_INTERVAL};
use crate::recorder::Recorder;

/// One cached device's query state plus the background poller task
/// feeding it — see [`QueryRegistry`].
type CachedQueryState = (Arc<DeviceQueryState>, tokio::task::JoinHandle<()>);

/// Lazily creates and caches one [`DeviceQueryState`] per device. See the
/// module docs.
pub struct QueryRegistry {
    poll_interval: Duration,
    states: Mutex<HashMap<DeviceId, CachedQueryState>>,
}

impl QueryRegistry {
    pub fn new(poll_interval: Duration) -> Self {
        Self {
            poll_interval,
            states: Mutex::new(HashMap::new()),
        }
    }

    /// The shared [`DeviceQueryState`] for `id`, creating it (and spawning
    /// its background poller against `recorder`) on first reference.
    ///
    /// Performs one synchronous [`DeviceQueryState::ingest`] before
    /// returning a freshly created state, rather than relying solely on
    /// the newly spawned poller task's first tick: that task is merely
    /// *scheduled* by `tokio::spawn`, not guaranteed to have actually run
    /// yet by the time this call returns, so a `read_since`/`tail` request
    /// that's the very first thing to ever reference a device could
    /// otherwise observe an empty result even though the data is already
    /// durably on disk. `subscribe`'s own loop tolerates that race (it
    /// always yields at least once before its first check), but a
    /// one-shot query must not have to.
    pub fn get_or_spawn(&self, id: &DeviceId, recorder: Arc<Recorder>) -> Arc<DeviceQueryState> {
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((state, _)) = states.get(id) {
            return Arc::clone(state);
        }
        let state = Arc::new(DeviceQueryState::new());
        state.ingest(&recorder);
        let handle = spawn_poller(recorder, Arc::clone(&state), self.poll_interval);
        states.insert(id.clone(), (Arc::clone(&state), handle));
        state
    }
}

impl Default for QueryRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_POLL_INTERVAL)
    }
}

impl Drop for QueryRegistry {
    fn drop(&mut self) {
        for (_, (_, handle)) in self
            .states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            handle.abort();
        }
    }
}

/// What a client is doing right now — surfaced by `list_clients` so, per
/// the wiki, "the operator can see what the agent is waiting for without
/// asking it."
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Activity {
    #[default]
    Idle,
    WaitingFor {
        device: String,
        pattern: String,
        deadline: Instant,
    },
}

struct ClientEntry {
    name: String,
    pid: u32,
    client_type: ClientType,
    permission: Mutex<Permission>,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    /// How many `Request::Write`s this client has sent to the gate this
    /// session, whitelisted-and-immediately-allowed writes included
    /// (`TASKS.md` T4.2, issue #15: an approval payload's "本 session 第幾
    /// 次請求" field) — see [`ClientRegistry::next_write_attempt`].
    write_attempts: AtomicU64,
    activity: Mutex<Activity>,
    /// Notified (once, via `notify_waiters`) when this client is kicked or
    /// disconnects on its own — both the reader and writer loops for this
    /// connection race against it so a kick takes effect immediately
    /// regardless of any request (e.g. a long `wait_for`) in flight on the
    /// same connection.
    kill: Arc<tokio::sync::Notify>,
}

/// A `list_clients` row — a plain snapshot, decoupled from [`ClientEntry`]
/// so the protocol/session layer doesn't need to reach into this module's
/// locking internals to build a reply.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientSnapshot {
    pub client_id: u64,
    pub name: String,
    pub pid: u32,
    pub client_type: ClientType,
    pub permission: Permission,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub activity: Activity,
}

/// Session management table: every currently-connected client, keyed by a
/// daemon-assigned `client_id`.
#[derive(Default)]
pub struct ClientRegistry {
    next_id: AtomicU64,
    clients: Mutex<HashMap<u64, ClientEntry>>,
}

impl ClientRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly handshaked client, returning its `client_id`.
    pub fn register(
        &self,
        name: String,
        pid: u32,
        client_type: ClientType,
        permission: Permission,
        kill: Arc<tokio::sync::Notify>,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                ClientEntry {
                    name,
                    pid,
                    client_type,
                    permission: Mutex::new(permission),
                    bytes_in: AtomicU64::new(0),
                    bytes_out: AtomicU64::new(0),
                    write_attempts: AtomicU64::new(0),
                    activity: Mutex::new(Activity::Idle),
                    kill,
                },
            );
        id
    }

    pub fn unregister(&self, client_id: u64) {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&client_id);
    }

    pub fn add_bytes_in(&self, client_id: u64, n: u64) {
        if let Some(entry) = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client_id)
        {
            entry.bytes_in.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn add_bytes_out(&self, client_id: u64, n: u64) {
        if let Some(entry) = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client_id)
        {
            entry.bytes_out.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn set_activity(&self, client_id: u64, activity: Activity) {
        if let Some(entry) = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client_id)
        {
            *entry.activity.lock().unwrap_or_else(|e| e.into_inner()) = activity;
        }
    }

    /// This connection's [`ClientType`] and *current* [`Permission`] —
    /// looked up fresh on every call, never cached from the handshake, so a
    /// `demote` mid-connection is visible to the very next `write` request
    /// on that same connection. See `protocol::session`'s `Request::Write`
    /// handler, the only caller: it's the one place in this task's scope
    /// that decides "is this write allowed *right now*" (human's
    /// `ReadWrite` passes; everything else is still `permission_denied`
    /// pending T4.1's rule engine).
    pub fn type_and_permission(&self, client_id: u64) -> Option<(ClientType, Permission)> {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client_id)
            .map(|e| {
                (
                    e.client_type,
                    *e.permission.lock().unwrap_or_else(|e| e.into_inner()),
                )
            })
    }

    /// This connection's self-reported `name`, kernel-verified `pid`, and
    /// [`ClientType`] — looked up fresh, same "never cached from the
    /// handshake" convention [`Self::type_and_permission`] documents.
    /// [`crate::gate::RequesterCtx`] is built from this (plus
    /// [`Self::next_write_attempt`]) by `protocol::session`'s
    /// `Request::Write` handler for a gated (`agent`) write (`TASKS.md`
    /// T4.2, issue #15's approval payload: "requester 身分（name + verified
    /// pid + type）").
    pub fn identity(&self, client_id: u64) -> Option<(String, u32, ClientType)> {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client_id)
            .map(|e| (e.name.clone(), e.pid, e.client_type))
    }

    /// Increment and return this client's write-attempt counter (1-based —
    /// the first call returns `1`), for the approval payload's "本 session
    /// 第幾次請求" field (`TASKS.md` T4.2, issue #15). Counts every write
    /// that reaches the gate, allowed-by-whitelist ones included: an
    /// operator deciding whether to approve a pending write benefits from
    /// knowing "this is this agent's 13th write this session, the first 12
    /// went fine" just as much for a whitelisted history as a gated one.
    /// Returns `1` for an unknown `client_id` (same defensive fallback
    /// `type_and_permission`'s unreachable-in-practice branch documents)
    /// rather than panicking.
    pub fn next_write_attempt(&self, client_id: u64) -> u64 {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&client_id)
            .map(|e| e.write_attempts.fetch_add(1, Ordering::Relaxed) + 1)
            .unwrap_or(1)
    }

    pub fn list(&self) -> Vec<ClientSnapshot> {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, e)| ClientSnapshot {
                client_id: *id,
                name: e.name.clone(),
                pid: e.pid,
                client_type: e.client_type,
                permission: *e.permission.lock().unwrap_or_else(|e| e.into_inner()),
                bytes_in: e.bytes_in.load(Ordering::Relaxed),
                bytes_out: e.bytes_out.load(Ordering::Relaxed),
                activity: e.activity.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            })
            .collect()
    }

    /// Close `client_id`'s connection. Returns `false` if no such client is
    /// currently registered.
    pub fn kick(&self, client_id: u64) -> bool {
        let clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        match clients.get(&client_id) {
            Some(entry) => {
                entry.kill.notify_waiters();
                true
            }
            None => false,
        }
    }

    /// Reduce (or otherwise change) `client_id`'s permission in place.
    /// Returns `false` if no such client is currently registered. Actual
    /// enforcement of the new level is T4.1's rule engine; this registry
    /// only tracks and reports it — see `wrap_proto::Permission`'s docs.
    pub fn demote(&self, client_id: u64, permission: Permission) -> bool {
        let clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        match clients.get(&client_id) {
            Some(entry) => {
                *entry.permission.lock().unwrap_or_else(|e| e.into_inner()) = permission;
                true
            }
            None => false,
        }
    }
}
