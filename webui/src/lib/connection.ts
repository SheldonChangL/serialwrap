import { writable, type Writable } from "svelte/store";

/**
 * Connection lifecycle for the `/api/stream` WebSocket (`TASKS.md` T5.1,
 * issue #18).
 *
 * `open` is only ever reached from an actual application-level message
 * (the server's `hello`/`heartbeat` JSON), never from the browser's `open`
 * event alone — a completed WS handshake proves a TCP+HTTP upgrade
 * succeeded, not that the daemon on the other end is alive and making
 * sense. `stale` exists for the same reason in the other direction: if the
 * socket is still technically open but no heartbeat has arrived recently,
 * we stop claiming "connected" rather than silently trusting a
 * possibly-wedged connection. This is the project's "never silently
 * pretend the stream is fine" principle (see the Client-protocol wiki's
 * disconnect-event guarantee) applied to the GUI's own transport.
 */
export type ConnectionState = "connecting" | "open" | "stale" | "closed";

export interface ConnectionInfo {
  state: ConnectionState;
  serverVersion: string | null;
  deviceCount: number | null;
  lastMessageAt: number | null;
  attempt: number;
}

const INITIAL: ConnectionInfo = {
  state: "connecting",
  serverVersion: null,
  deviceCount: null,
  lastMessageAt: null,
  attempt: 0,
};

/**
 * The daemon heartbeats every 2s (`serialwrapd::web::stream::HEARTBEAT_INTERVAL`).
 * 7s (3.5 beats) gives slack for one slow tick without flapping the UI, but
 * still catches a wedged connection well before a human would notice on
 * their own.
 */
const STALE_AFTER_MS = 7_000;
const WATCHDOG_INTERVAL_MS = 1_000;

/** Reconnect backoff schedule, in ms — capped rather than unbounded so a
 * long-dead daemon still gets retried at a human-reasonable cadence. */
const BACKOFF_SCHEDULE_MS = [500, 1_000, 2_000, 4_000, 5_000];

function backoffFor(attempt: number): number {
  const idx = Math.min(attempt, BACKOFF_SCHEDULE_MS.length - 1);
  return BACKOFF_SCHEDULE_MS[idx];
}

function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}/api/stream`;
}

interface ServerMessage {
  type?: string;
  server_version?: string;
  device_count?: number;
}

/**
 * Owns the WebSocket for the life of the page: connects, reconnects with
 * backoff on close/error, and runs a watchdog that demotes `open` to
 * `stale` if heartbeats stop arriving. `info` is the one thing components
 * need to subscribe to.
 */
export class Connection {
  readonly info: Writable<ConnectionInfo> = writable({ ...INITIAL });

  private socket: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private watchdogTimer: ReturnType<typeof setInterval> | null = null;
  private attempt = 0;
  private stopped = true;

  start(): void {
    this.stopped = false;
    this.watchdogTimer = setInterval(() => this.checkStale(), WATCHDOG_INTERVAL_MS);
    this.connect();
  }

  stop(): void {
    this.stopped = true;
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    if (this.watchdogTimer !== null) clearInterval(this.watchdogTimer);
    this.reconnectTimer = null;
    this.watchdogTimer = null;
    this.socket?.close();
    this.socket = null;
  }

  private connect(): void {
    if (this.stopped) return;
    const socket = new WebSocket(wsUrl());
    this.socket = socket;

    socket.addEventListener("message", (event) => this.handleMessage(event.data));
    socket.addEventListener("close", () => this.handleDisconnect());
    // A browser WebSocket always follows an `error` event with a `close`
    // event — reconnect scheduling lives solely in `handleDisconnect` so it
    // runs exactly once per drop, not twice.
    socket.addEventListener("error", () => {});
  }

  private handleMessage(raw: unknown): void {
    if (typeof raw !== "string") return;
    let parsed: ServerMessage;
    try {
      parsed = JSON.parse(raw) as ServerMessage;
    } catch {
      return;
    }
    if (parsed.type !== "hello" && parsed.type !== "heartbeat") return;

    this.attempt = 0;
    const now = Date.now();
    this.info.update((i) => ({
      ...i,
      state: "open",
      serverVersion: parsed.server_version ?? i.serverVersion,
      deviceCount: parsed.device_count ?? i.deviceCount,
      lastMessageAt: now,
      attempt: 0,
    }));
  }

  private handleDisconnect(): void {
    this.socket = null;
    if (this.stopped) return;
    this.info.update((i) => ({ ...i, state: "closed" }));
    this.attempt += 1;
    const delay = backoffFor(this.attempt - 1);
    this.reconnectTimer = setTimeout(() => this.connect(), delay);
  }

  private checkStale(): void {
    this.info.update((i) => {
      if (i.state !== "open" || i.lastMessageAt === null) return i;
      if (Date.now() - i.lastMessageAt > STALE_AFTER_MS) {
        return { ...i, state: "stale" };
      }
      return i;
    });
  }
}
