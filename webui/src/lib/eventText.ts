/**
 * Turns an out-of-band record into the sentence a person reads.
 *
 * The live log used to render every non-data row as `JSON.stringify(extra)`,
 * which put things like
 * `{"changed_by":"system:connect","new":{"baud":9600,"data_bits":"eight",…}}`
 * inline between log lines — technically complete and practically unreadable,
 * and the single biggest reason the GUI didn't answer "what is happening on
 * this port right now". Every event kind the daemon appends is spelled out
 * here instead, in the vocabulary an operator already has ("port opened at
 * 9600 8N1", not "config_change with old: null").
 *
 * Three rules this module follows:
 *
 * 1. **Nothing is hidden, only demoted.** Every row keeps its exact original
 *    payload one click away (`LogRow` renders `raw` on expand), because a
 *    summary that can't be checked against the record is worse than no
 *    summary in a tool whose whole value proposition is a faithful log.
 * 2. **Say only what changed.** A `config_change` that moved the baud rate
 *    reports the baud rate. Re-printing seven unchanged fields is how the
 *    original JSON dump hid the one field that mattered.
 * 3. **An unknown event still reads as an event.** New event names get the
 *    name plus their payload rather than an empty row — the daemon's event
 *    schema is explicitly forward-growing (see `device_profile.rs`'s event
 *    naming docs), so this must never need updating in lockstep to stay
 *    correct, only to stay eloquent.
 */

/** Which of the palette's semantic channels a row belongs to — drives the
 * left gutter color. `rx` is anything the device/daemon originated, `tx`
 * anything a client sent, `gate` a security decision, `warn` a degraded but
 * non-blocking condition. See `app.css`'s module comment for why direction
 * of data flow is the thing that gets colored. */
export type EventTone = "rx" | "tx" | "gate" | "warn";

export interface EventDescription {
  /** The sentence. Sentence case, no trailing period — it sits in a table. */
  summary: string;
  /** Secondary text, dimmed: the specifics (a path, an exit code, who did
   * it). `null` when the summary already says everything. */
  detail: string | null;
  tone: EventTone;
}

type Extra = Record<string, unknown>;

function str(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}

function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

const DATA_BITS: Record<string, string> = { five: "5", six: "6", seven: "7", eight: "8" };
const PARITY: Record<string, string> = { none: "N", even: "E", odd: "O" };
const STOP_BITS: Record<string, string> = { one: "1", two: "2" };

export interface PortConfigLike {
  baud?: number;
  data_bits?: string;
  parity?: string;
  stop_bits?: string;
  flow_control?: string;
}

/** `115200 8N1` — the framing notation every serial tool in this space uses,
 * and the one an operator is already reading off a datasheet. Shared with
 * the top bar's config chip so the two can never drift apart. */
export function formatConfig(cfg: PortConfigLike | null | undefined): string {
  if (!cfg) return "…";
  const bits = DATA_BITS[cfg.data_bits ?? ""] ?? "?";
  const parity = PARITY[cfg.parity ?? ""] ?? "?";
  const stop = STOP_BITS[cfg.stop_bits ?? ""] ?? "?";
  return `${cfg.baud ?? "?"} ${bits}${parity}${stop}`;
}

/** Human names for the config fields a diff can mention, in the order a
 * reader scans them (the one they changed on purpose first). */
const CONFIG_FIELDS: Array<[keyof PortConfigLike, string]> = [
  ["baud", "baud"],
  ["data_bits", "data bits"],
  ["parity", "parity"],
  ["stop_bits", "stop bits"],
  ["flow_control", "flow control"],
];

function configDiff(old: PortConfigLike, next: PortConfigLike): string[] {
  const out: string[] = [];
  for (const [key, label] of CONFIG_FIELDS) {
    const a = old[key];
    const b = next[key];
    if (a === b || b === undefined) continue;
    out.push(`${label} ${a ?? "?"} → ${b}`);
  }
  return out;
}

/** `changed_by`/`kicked_by` are `"<name>:<pid>"` for a UDS peer and the bare
 * string `"gui"` for the web layer (`web::api`'s `GUI_CHANGED_BY`), plus
 * `"system:connect"` for the daemon applying a saved profile on open. Read
 * back as the actor, not the wire format. */
function actor(raw: unknown): string | null {
  const s = str(raw);
  if (s === null) return null;
  if (s === "gui") return "this browser";
  if (s === "system:connect") return "the daemon, restoring this port's saved profile";
  return s;
}

function describeConnect(extra: Extra): EventDescription {
  const path = str(extra.path);
  const serial = str(extra.serial_number);
  const pathBased = str(extra.id_kind) === "path";
  // Worth saying out loud, once, on the row where it's decided: a device
  // with no USB serial number can only be identified by where it's plugged
  // in, so its saved port settings do not survive a replug that renumbers
  // the tty. That is a real, surprising limitation of the hardware (CH340s
  // ship without one), and the connect row is where an operator can still
  // act on it.
  const detail = pathBased
    ? `${path ?? "unknown port"} · no USB serial number, so this id follows the port path and changes on replug`
    : (path ?? (serial ? `serial ${serial}` : null));
  return { summary: "Port opened", detail, tone: "rx" };
}

function describeConfigChange(extra: Extra): EventDescription {
  const next = (extra.new ?? null) as PortConfigLike | null;
  const old = (extra.old ?? null) as PortConfigLike | null;
  const by = actor(extra.changed_by);
  if (!next) return { summary: "Port settings changed", detail: by, tone: "warn" };
  if (!old) {
    // First config ever applied to this device — a statement of the
    // starting state, not a change. Saying "9600 → 9600" here would be a
    // lie, and saying "changed" would send someone hunting for who did it.
    return {
      summary: `Port set to ${formatConfig(next)}`,
      detail: by ? `by ${by}` : null,
      tone: "rx",
    };
  }
  const diff = configDiff(old, next);
  if (diff.length === 0) {
    return { summary: "Port settings reapplied, nothing changed", detail: by, tone: "warn" };
  }
  return {
    summary: diff.join(", "),
    detail: by ? `by ${by}` : null,
    tone: "warn",
  };
}

function describeLeaseEnd(extra: Extra): EventDescription {
  const command = str(extra.command);
  const exit = num(extra.exit_code);
  const ms = num(extra.duration_ms);
  const reason = str(extra.reason);
  const held = ms === null ? null : `held ${(ms / 1000).toFixed(1)}s`;
  // `exit_code: null` is not "exited 0" — it's a lease the daemon reclaimed
  // without ever learning the child's fate (a timeout, or a residual lease
  // found at startup). Those two must not read the same.
  const outcome =
    exit === null
      ? reason === "released"
        ? "released"
        : `reclaimed by the daemon (${reason ?? "unknown reason"})`
      : exit === 0
        ? "finished cleanly"
        : `exited ${exit}`;
  return {
    summary: `Port handed back — ${outcome}`,
    detail: [command, held].filter(Boolean).join(" · ") || null,
    tone: exit !== null && exit !== 0 ? "gate" : "rx",
  };
}

/** Decode the base64 payload of a pending write into something printable, so
 * the "waiting for approval" row says *what* is waiting rather than just
 * that something is. Control bytes render as their Unicode control pictures,
 * the same convention `liveLog.ts`'s TX rows use. */
function decodePreview(b64: unknown): string | null {
  const s = str(b64);
  if (s === null) return null;
  try {
    const bin = atob(s);
    let out = "";
    for (let i = 0; i < bin.length && i < 80; i++) {
      const c = bin.charCodeAt(i);
      out += c < 0x20 ? String.fromCodePoint(0x2400 + c) : c === 0x7f ? "␡" : bin[i];
    }
    return bin.length > 80 ? `${out}…` : out;
  } catch {
    return null;
  }
}

export function describeEvent(name: string, extra: Extra): EventDescription {
  switch (name) {
    case "connect":
      return describeConnect(extra);

    case "disconnect":
      return {
        summary: "Port closed",
        detail: str(extra.reason) ?? str(extra.path),
        tone: "gate",
      };

    case "open_failed":
      // `message` is already written to be human-actionable at the point it
      // was produced (`port.rs`'s `describe_open_error` — permission denied,
      // device busy). Passing it through beats paraphrasing it here.
      return {
        summary: str(extra.message) ?? "Could not open the port",
        detail: str(extra.path),
        tone: "gate",
      };

    case "config_change":
      return describeConfigChange(extra);

    case "control_line_change": {
      const line = (str(extra.line) ?? "line").toUpperCase();
      const high = extra.level === true;
      const by = actor(extra.changed_by);
      return {
        summary: `${line} ${high ? "asserted" : "released"}`,
        detail: by ? `by ${by}` : null,
        tone: "tx",
      };
    }

    case "dtr_pulse": {
      const ms = num(extra.duration_ms);
      const by = actor(extra.changed_by);
      return {
        summary: `Board reset — DTR pulsed${ms === null ? "" : ` ${ms} ms`}`,
        detail: by ? `by ${by}` : null,
        tone: "tx",
      };
    }

    case "lease_start": {
      const command = str(extra.command);
      const pid = num(extra.pid);
      return {
        summary: `Port handed to ${command ?? "another tool"}`,
        detail: [pid === null ? null : `pid ${pid}`, "recording pauses until it exits"]
          .filter(Boolean)
          .join(" · "),
        tone: "warn",
      };
    }

    case "lease_end":
      return describeLeaseEnd(extra);

    case "write_request": {
      const who = str(extra.requester_name);
      const kind = str(extra.requester_type);
      const preview = decodePreview(extra.bytes_b64);
      const danger = str(extra.danger_reason);
      const rule = str(extra.matched_rule);
      return {
        summary: `${who ?? "A client"} wants to send${preview === null ? "" : `: ${preview}`}`,
        detail:
          danger ?? (rule ? `matched rule ${rule}` : kind ? `${kind} · waiting for you` : null),
        tone: "gate",
      };
    }

    case "client_kicked": {
      const who = str(extra.name);
      const pid = num(extra.pid);
      const by = actor(extra.kicked_by);
      return {
        summary: `Disconnected ${who ?? "a client"}`,
        detail: [pid === null ? null : `pid ${pid}`, by ? `by ${by}` : null]
          .filter(Boolean)
          .join(" · "),
        tone: "gate",
      };
    }

    case "recovery": {
      const bytes = num(extra.discarded_bytes);
      return {
        summary: "Recovered from an unclean shutdown",
        detail:
          bytes === null
            ? null
            : `${bytes.toLocaleString()} unreadable bytes at the end of the log were discarded`,
        tone: "warn",
      };
    }

    default: {
      // Forward compatibility, per this module's rule 3: a name this build
      // has never heard of still gets a readable row.
      const keys = Object.keys(extra);
      return {
        summary: name.replace(/_/g, " "),
        detail: keys.length === 0 ? null : keys.map((k) => `${k}=${fmtValue(extra[k])}`).join(" "),
        tone: "warn",
      };
    }
  }
}

function fmtValue(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}
