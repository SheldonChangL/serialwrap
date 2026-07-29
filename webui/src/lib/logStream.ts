/**
 * Live log data source for one device (`TASKS.md` T5.2, issue #19):
 * fetches the initial `tail` page, then opens `WS /api/stream?device=...`
 * with `since_cursor` set to that page's cursor — closing the
 * tail-then-subscribe gap exactly the way the Client-protocol wiki
 * documents (`crates/serialwrapd/src/web/stream.rs`'s module doc comment
 * has the daemon side of this contract).
 *
 * Deliberately a separate socket from `connection.ts`'s app-level
 * `Connection` (which has no device concept and drives the top-level
 * connection-status pill): this task is scoped to "one device per browser
 * tab" (see the UX-design wiki's "deliberate omissions" section), so a
 * second, device-scoped socket is simpler than threading device selection
 * through the shared one.
 */
import type { PresentedPageJson } from "./liveLog";

export type LogStreamState = "connecting" | "open" | "closed" | "error";

export interface LogStreamCallbacks {
  onPage: (page: PresentedPageJson) => void;
  onState: (state: LogStreamState, detail?: string) => void;
}

async function fetchTail(deviceId: string, n?: number): Promise<PresentedPageJson> {
  const url = new URL(`/api/devices/${encodeURIComponent(deviceId)}/tail`, location.origin);
  if (n) url.searchParams.set("n", String(n));
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`GET ${url.pathname} failed: ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as PresentedPageJson;
}

/** `decode_health`'s wire shape (`TASKS.md` T5.3, issue #20 —
 * `crates/serialwrapd/src/web/api.rs`'s `DecodeHealth`): whether recent
 * output looks like it's arriving at the wrong baud, and what to try
 * instead. `suggested_baud` is only ever present once both a minimum sample
 * size and an undecodable-ratio threshold are met — see that struct's doc
 * comment for the exact numbers and the reasoning against them. */
export interface DecodeHealth {
  checked_bytes: number;
  undecodable_ratio: number;
  suggested_baud: number | null;
}

export interface DeviceConfig {
  config: Record<string, unknown>;
  error_counts?: { status: "available" | "unavailable"; framing?: number; overrun?: number; parity?: number };
  decode_health?: DecodeHealth;
}

export async function fetchDeviceConfig(deviceId: string): Promise<DeviceConfig> {
  const res = await fetch(`/api/devices/${encodeURIComponent(deviceId)}/config`);
  if (!res.ok) {
    throw new Error(`GET config failed: ${res.status} ${res.statusText}`);
  }
  return (await res.json()) as DeviceConfig;
}

/** `POST /api/devices/:id/config` (`TASKS.md` T5.3, issue #20): the port
 * settings popover's "Apply" action, and the `config_change` log row's
 * one-click "還原" (revert) button — both send a partial
 * `PortConfig` patch (only the fields that actually changed; revert sends
 * back the event's whole `old` value) and get the merged, full config back.
 * Ungated — see `crates/serialwrapd/src/web/api.rs`'s `GUI_CHANGED_BY` doc
 * comment for why the web GUI's own config/control-line writes never go
 * through the write gate. */
export async function setDeviceConfig(
  deviceId: string,
  patch: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const res = await fetch(`/api/devices/${encodeURIComponent(deviceId)}/config`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(patch),
  });
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    throw new Error(
      `POST config failed: ${res.status} ${(body as { error?: { message?: string } }).error?.message ?? res.statusText}`,
    );
  }
  const body = (await res.json()) as { config: Record<string, unknown> };
  return body.config;
}

/** `POST /api/devices/:id/control_lines` (`TASKS.md` T5.3, issue #20): the
 * port settings popover's DTR/RTS toggle switches — a live, immediate
 * assert, deliberately separate from `setDeviceConfig` (which only ever
 * touches the *open-time* `open_control_lines` policy) per the UX-design
 * wiki's "control lines are separated from data settings" principle. Either
 * field omitted (`undefined`) means "leave that line untouched". */
export async function setControlLines(
  deviceId: string,
  lines: { dtr?: boolean; rts?: boolean },
): Promise<void> {
  const res = await fetch(`/api/devices/${encodeURIComponent(deviceId)}/control_lines`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(lines),
  });
  if (!res.ok) {
    throw new Error(`POST control_lines failed: ${res.status} ${res.statusText}`);
  }
}

/** What to append after `text`. Mirrors `wrap_proto::LineEnding`'s wire
 * names — sending the wrong one to a firmware CLI is the classic reason a
 * board "ignores" a command, which is why this is a visible control in the
 * write bar rather than a hidden constant. */
export type LineEnding = "lf" | "crlf" | "cr" | "none";

export interface WritePayload {
  /** UTF-8 text; the daemon appends `line_ending`. */
  text?: string;
  /** Exact bytes, base64. Sent verbatim, no line ending — hex mode parses
   * the operator's digits in the browser and encodes them here, so the
   * daemon keeps exactly one byte-decoding path (see the `WriteBody` doc
   * comment in `crates/serialwrapd/src/web/api.rs`). */
  data_b64?: string;
  line_ending?: LineEnding;
}

/** `POST /api/devices/:id/write`: send bytes to the port as the operator.
 *
 * Goes out immediately rather than through the write gate, because the
 * person clicking Send is the human the gate exists to ask — the same
 * `human` bypass `serialwrap write` has always had over UDS. It is still
 * audited: the daemon appends the same `tx` record every other write path
 * produces, so this write appears in this very log view, in every other
 * client's `tail`, and in `serialwrap audit`. */
export async function writeToDevice(
  deviceId: string,
  payload: WritePayload,
): Promise<{ written: number }> {
  const res = await fetch(`/api/devices/${encodeURIComponent(deviceId)}/write`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error(
      (body as { error?: { message?: string } }).error?.message ??
        `${res.status} ${res.statusText}`,
    );
  }
  return body as { written: number };
}

const BACKOFF_SCHEDULE_MS = [500, 1_000, 2_000, 4_000, 5_000];

function backoffFor(attempt: number): number {
  return BACKOFF_SCHEDULE_MS[Math.min(attempt, BACKOFF_SCHEDULE_MS.length - 1)];
}

function wsUrl(deviceId: string, sinceCursor: number): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const params = new URLSearchParams({ device: deviceId, since_cursor: String(sinceCursor) });
  return `${proto}//${location.host}/api/stream?${params.toString()}`;
}

/** Owns the tail-fetch + WS-subscribe lifecycle for one device. `start()`
 * fetches the initial page (calling `onPage` once), then opens the
 * follow-on subscription; every subsequent push also calls `onPage`. */
export class LogStream {
  private socket: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private stopped = false;
  private cursor = 0;

  constructor(
    private readonly deviceId: string,
    private readonly callbacks: LogStreamCallbacks,
  ) {}

  async start(): Promise<void> {
    this.stopped = false;
    this.callbacks.onState("connecting");
    try {
      const page = await fetchTail(this.deviceId);
      this.cursor = page.cursor;
      this.callbacks.onPage(page);
    } catch (e) {
      this.callbacks.onState("error", e instanceof Error ? e.message : String(e));
      // Still try to subscribe from cursor 0 — a fresh device with no
      // history yet is a normal case, not an error worth giving up over.
      this.cursor = 0;
    }
    if (!this.stopped) this.connect();
  }

  stop(): void {
    this.stopped = true;
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.socket?.close();
    this.socket = null;
  }

  private connect(): void {
    if (this.stopped) return;
    const socket = new WebSocket(wsUrl(this.deviceId, this.cursor));
    this.socket = socket;
    socket.addEventListener("open", () => {
      // Mirrors `connection.ts`'s stance: an actual application message
      // (not the bare WS `open` event) is what "connected" means. `hello`
      // arrives immediately after open, so this is mostly a formality, but
      // no state flips to "open" here.
    });
    socket.addEventListener("message", (event) => this.handleMessage(event.data));
    socket.addEventListener("close", () => this.handleDisconnect());
    socket.addEventListener("error", () => {});
  }

  private handleMessage(raw: unknown): void {
    if (typeof raw !== "string") return;
    let parsed: { type?: string } & Record<string, unknown>;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return;
    }
    if (parsed.type === "hello" || parsed.type === "heartbeat") {
      this.attempt = 0;
      this.callbacks.onState("open");
      return;
    }
    if (parsed.type === "push") {
      const page = parsed as unknown as PresentedPageJson;
      this.cursor = page.cursor;
      this.callbacks.onPage(page);
      return;
    }
    if (parsed.type === "stream_error") {
      this.callbacks.onState("error", String(parsed.code ?? "unknown"));
    }
  }

  private handleDisconnect(): void {
    this.socket = null;
    if (this.stopped) return;
    this.callbacks.onState("closed");
    this.attempt += 1;
    const delay = backoffFor(this.attempt - 1);
    this.reconnectTimer = setTimeout(() => this.connect(), delay);
  }
}
