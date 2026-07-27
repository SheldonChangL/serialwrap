/**
 * Clients panel data model and API client (`TASKS.md` T5.5, issue #22).
 *
 * `GET /api/clients` merges two sources — see
 * `crates/serialwrapd/src/web/api.rs`'s `list_clients` doc comment:
 *
 * - every *live* client, straight from
 *   `crate::protocol::registry::ClientRegistry::list` (the same table
 *   `Request::ListClients` reads over the UDS wire);
 * - every *finished lease* still visible in the event stream (a `lease_end`
 *   record), so "who touched the board just now" stays answerable even
 *   after the client itself disconnects (UX-design wiki: "已結束的 lease 保
 *   留在列表").
 *
 * These are deliberately two separate arrays in the wire shape (`clients`/
 * `finished_leases`), not pre-merged server-side — this module merges them
 * into one display-ordered list for the panel.
 */

export type ClientKind = "human" | "agent" | "tool";

/** Wire spelling of `wrap_proto::Permission` — matches its own
 * `#[serde(rename = ...)]` strings exactly (not `rename_all`, since `+`
 * isn't a valid case-transform target). */
export type Permission = "read+write" | "read+gated_write" | "lease_only";

export interface ActivityIdle {
  state: "idle";
}

export interface ActivityWaitingFor {
  state: "waiting_for";
  device: string;
  pattern: string;
  /** Seconds left on this `wait_for` call's own timeout, computed
   * server-side at request time (`deadline.saturating_duration_since(now)`)
   * — never negative, and never a value this client needs to keep ticking
   * down itself between polls (see `ClientsPanel.svelte`'s poll interval). */
  remaining_s: number;
}

export type Activity = ActivityIdle | ActivityWaitingFor;

export interface ActiveClient {
  status: "active";
  client_id: number;
  name: string;
  pid: number;
  type: ClientKind;
  permission: Permission;
  bytes_in: number;
  bytes_out: number;
  activity: Activity;
}

export interface FinishedLease {
  status: "offline";
  device: string;
  name: string;
  pid: number | null;
  type: "tool";
  command: string;
  exit_code: number | null;
  duration_ms: number | null;
  reason: string | null;
  ended_at: string;
  ended_seq: number;
}

export type ClientRow = ActiveClient | FinishedLease;

interface ClientsWire {
  clients: ActiveClient[];
  finished_leases: FinishedLease[];
}

export async function fetchClients(): Promise<ClientRow[]> {
  const res = await fetch("/api/clients");
  if (!res.ok) {
    throw new Error(`GET /api/clients failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as ClientsWire;
  // Active clients first (what an operator most wants to see at a glance),
  // finished leases after — mirrors the UX-design wiki mockup's own
  // top-to-bottom ordering (the two active rows, then the ended lease).
  return [...body.clients, ...body.finished_leases];
}

export type ClientActionOutcome = "ok" | "not_found";

export async function kickClient(clientId: number): Promise<ClientActionOutcome> {
  const res = await fetch(`/api/clients/${clientId}/kick`, { method: "POST" });
  if (res.status === 404) return "not_found";
  if (!res.ok) {
    throw new Error(`POST /api/clients/${clientId}/kick failed: ${res.status} ${res.statusText}`);
  }
  return "ok";
}

export async function demoteClient(clientId: number, permission: Permission): Promise<ClientActionOutcome> {
  const res = await fetch(`/api/clients/${clientId}/demote`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ permission }),
  });
  if (res.status === 404) return "not_found";
  if (!res.ok) {
    throw new Error(`POST /api/clients/${clientId}/demote failed: ${res.status} ${res.statusText}`);
  }
  return "ok";
}

/** Most- to least-privileged, per the Security-model wiki's policy table
 * (`human` > `agent` > `tool`) — [`nextDemotion`] walks this. */
const PERMISSION_ORDER: Permission[] = ["read+write", "read+gated_write", "lease_only"];

/** The next-lower permission than `current`, or `null` when already at the
 * lowest (`lease_only`) — the panel's single "Demote" button always steps
 * one level down rather than offering a full picker, matching the
 * UX-design wiki mockup (`[Demote]`, no dropdown). */
export function nextDemotion(current: Permission): Permission | null {
  const idx = PERMISSION_ORDER.indexOf(current);
  if (idx === -1 || idx === PERMISSION_ORDER.length - 1) return null;
  return PERMISSION_ORDER[idx + 1];
}

export function permissionBadge(permission: Permission): string {
  switch (permission) {
    case "read+write":
      return "RW";
    case "read+gated_write":
      return "R+GW";
    case "lease_only":
      return "lease";
  }
}

export function clientTypeIcon(type: ClientKind): string {
  switch (type) {
    case "human":
      return "\u{1F464}"; // 👤
    case "agent":
      return "\u{1F916}"; // 🤖
    case "tool":
      return "\u{1F527}"; // 🔧
  }
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}
