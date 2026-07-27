/**
 * Approval card data model and API client (`TASKS.md` T5.4, issue #21).
 *
 * Reuses the exact same daemon-side API `serialwrap approvals` already
 * decides through: `GET /api/approvals` / `POST /api/approvals/:id/approve`
 * / `POST /api/approvals/:id/deny` call straight into
 * `crates/serialwrapd/src/protocol/Shared::gate` (see that module's own doc
 * comment), the same single `Gate`/`PendingQueue` instance a concurrent
 * `serialwrap approvals approve/deny` decides through over UDS — there is
 * no second write-gate implementation for the GUI.
 */

/** Wire shape of `crate::gate::approval::ApprovalSnapshot`, serialized
 * as-is by `GET /api/approvals`. */
export interface ApprovalSnapshot {
  id: number;
  device: string;
  requester_name: string;
  requester_pid: number;
  requester_type: "human" | "agent" | "tool";
  session_request_no: number;
  bytes_b64: string;
  bytes_text: string;
  bytes_hex: string;
  matched_rule: string | null;
  danger_reason: string | null;
  log_context: string[];
  age_s: number;
  /** Total configured approval timeout, in seconds — see
   * `ApprovalSnapshot::timeout_s`'s doc comment. Paired with `age_s` above,
   * this is what lets the card compute "how much time is left" without
   * assuming a fixed duration. */
  timeout_s: number;
}

export async function fetchApprovals(): Promise<ApprovalSnapshot[]> {
  const res = await fetch("/api/approvals");
  if (!res.ok) {
    throw new Error(`GET /api/approvals failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as { approvals: ApprovalSnapshot[] };
  return body.approvals;
}

/** Outcome of an approve/deny call. `"conflict"` means someone else (the
 * CLI, or another open GUI tab) already decided this same id first — the
 * caller should treat the card as resolved either way, never retry as a
 * "double decision" (T5.4 acceptance criterion 3). */
export type DecideOutcome = "ok" | "conflict";

async function decide(path: string, body: Record<string, unknown>): Promise<DecideOutcome> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  if (res.status === 409) return "conflict";
  if (!res.ok) {
    throw new Error(`POST ${path} failed: ${res.status} ${res.statusText}`);
  }
  return "ok";
}

export function approveApproval(id: number): Promise<DecideOutcome> {
  return decide(`/api/approvals/${id}/approve`, {});
}

export function denyApproval(id: number, reason?: string): Promise<DecideOutcome> {
  return decide(`/api/approvals/${id}/deny`, reason ? { reason } : {});
}

/**
 * Best-effort "something happened on this device" signal, used purely to
 * refresh the approvals list sooner than the host component's own coarse
 * poll interval — a new pending write (or someone else's decision) already
 * flows through the device's ordinary `event`/`gate` record stream (see
 * `crates/serialwrapd/src/gate.rs`'s `submit_write`: it appends a `gate`
 * `request` record and a `write_request` event on the *same* per-device
 * stream `WS /api/stream?device=...` already pushes), so this doesn't need
 * to parse or care which message arrived — any `push` is worth a refetch.
 * A tiny, content-agnostic subscription like this is deliberately simpler
 * than threading a callback through `LiveLog.svelte`'s own stream: the
 * approval card host has no other reason to depend on that component at
 * all, and duplicating one lightweight WS connection per tab is cheap.
 *
 * Returns a stop function.
 */
export function watchDeviceActivity(deviceId: string, onActivity: () => void): () => void {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/api/stream?${new URLSearchParams({ device: deviceId }).toString()}`;
  let stopped = false;
  let socket: WebSocket | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  function connect(): void {
    if (stopped) return;
    socket = new WebSocket(url);
    socket.addEventListener("message", (event) => {
      if (typeof event.data !== "string") return;
      try {
        const parsed = JSON.parse(event.data) as { type?: string };
        if (parsed.type === "push") onActivity();
      } catch {
        // Not JSON, or not a shape we care about — ignore.
      }
    });
    socket.addEventListener("close", () => {
      socket = null;
      if (stopped) return;
      reconnectTimer = setTimeout(connect, 1_000);
    });
    socket.addEventListener("error", () => {});
  }

  connect();
  return () => {
    stopped = true;
    if (reconnectTimer !== null) clearTimeout(reconnectTimer);
    socket?.close();
    socket = null;
  };
}
