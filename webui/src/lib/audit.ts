/**
 * Audit panel data model and API client (`TASKS.md` T5.5, issue #22).
 *
 * `GET /api/devices/:id/audit` is a pure filtered read over the same event
 * stream `tail`/`export` read from (see
 * `crates/serialwrapd/src/web/api.rs`'s `audit` doc comment) — every row
 * here is one real record, shaped by the same `presentation::event_to_json`
 * function `tail`'s own `events` field already uses. Nothing is joined or
 * correlated across rows: a denied write's bytes live on their own
 * `write_request` row (its own `seq`); the eventual `gate` deny/approve
 * decision is a separate row at its own later `seq`. "Jump to the log" for
 * any row is therefore just `row.seq` — no lookup needed.
 */

export type AuditKind = "tx" | "gate" | "event";

export interface AuditRow {
  seq: number;
  t_mono: number;
  t_wall: string;
  kind: AuditKind;
  event?: string;
  [key: string]: unknown;
}

export async function fetchAudit(deviceId: string, sinceSeq?: number, untilSeq?: number): Promise<AuditRow[]> {
  const params = new URLSearchParams();
  if (sinceSeq !== undefined) params.set("since_seq", String(sinceSeq));
  if (untilSeq !== undefined) params.set("until_seq", String(untilSeq));
  const qs = params.toString();
  const res = await fetch(`/api/devices/${encodeURIComponent(deviceId)}/audit${qs ? `?${qs}` : ""}`);
  if (!res.ok) {
    throw new Error(`GET /api/devices/${deviceId}/audit failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as { audit: AuditRow[] };
  return body.audit;
}

function decodeB64Length(value: unknown): number | null {
  if (typeof value !== "string" || value.length === 0) return null;
  try {
    return atob(value).length;
  } catch {
    return null;
  }
}

/** One-line summary for a row's collapsed state — loosely mirrors the
 * UX-design wiki's audit mockup phrasing (`"claude-code → TX status (8
 * B)"`, `"gate: deny"`) without trying to reproduce it verbatim for every
 * possible event name; the expanded view (`AuditPanel.svelte`) always shows
 * every field regardless of what this summary omits. */
export function summarizeRow(row: AuditRow): string {
  if (row.kind === "tx") {
    const client = typeof row["client"] === "string" ? (row["client"] as string) : "?";
    const len = decodeB64Length(row["data_b64"]);
    return `${client} → TX${len !== null ? ` (${len} B)` : ""}`;
  }
  if (row.kind === "gate") {
    const action = typeof row["action"] === "string" ? (row["action"] as string) : "?";
    return `gate: ${action}`;
  }
  return row.event ?? "event";
}

/** The collapsed-row status tag (`"whitelisted"`/`"approved"`/`"DENIED"`-
 * shaped text in the UX-design wiki mockup) — `tx`'s own `gate` field, or
 * `gate`'s own `action`; empty for a plain named `event` row (those have no
 * single pass/fail outcome to tag). */
export function statusTag(row: AuditRow): string {
  if (row.kind === "tx") return typeof row["gate"] === "string" ? (row["gate"] as string) : "";
  if (row.kind === "gate") return typeof row["action"] === "string" ? (row["action"] as string) : "";
  return "";
}

/** Decode a base64 byte field (`data_b64`/`bytes_b64`) to lossy UTF-8 text
 * and a hex preview, for the expanded row's "bytes" view — the same
 * "readable text for judgement, hex for precision" principle the approval
 * card mockup already establishes (a trailing `\r` is invisible in the
 * first form and obvious in the second). */
export function decodeBytesField(value: unknown): { text: string; hex: string } | null {
  if (typeof value !== "string" || value.length === 0) return null;
  try {
    const binary = atob(value);
    const bytes = Uint8Array.from(binary, (c) => c.charCodeAt(0));
    const text = new TextDecoder("utf-8", { fatal: false }).decode(bytes);
    const hex = Array.from(bytes)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join(" ");
    return { text, hex };
  } catch {
    return null;
  }
}

const ENVELOPE_FIELDS = new Set(["seq", "t_mono", "t_wall", "kind", "event"]);
const BYTE_FIELDS = new Set(["data_b64", "bytes_b64"]);

/** `row`'s own byte field value (`data_b64` for `tx`, `bytes_b64` for a
 * `write_request` event), or `null` if this row carries none — a `gate`
 * decision and most named events have no bytes of their own; the payload
 * lives on the `write_request`/`tx` row instead (see this module's doc
 * comment on why that's not joined in here). */
export function byteFieldValue(row: AuditRow): unknown {
  return row["data_b64"] ?? row["bytes_b64"] ?? null;
}

/** Every field on `row` beyond the envelope (`seq`/`t_mono`/`t_wall`/
 * `kind`/`event`) and the byte fields (shown separately via
 * [`decodeBytesField`]) — the expanded "reason" view's full-payload
 * fallback: `reason`, `matched_rule`, `danger_reason`, `request_seq`,
 * `requester_name`, `command`, `duration_ms`, whatever this particular
 * row's kind/event happens to carry. Nothing is hidden — "被拒絕的請求要保
 * 留完整 payload" (UX-design wiki) means an operator can always see
 * everything a record carries, not a curated subset. */
export function extraFields(row: AuditRow): [string, unknown][] {
  return Object.entries(row).filter(([k]) => !ENVELOPE_FIELDS.has(k) && !BYTE_FIELDS.has(k));
}

/** Client-side text filter over a row — a plain case-insensitive substring
 * match against every field's stringified value, mirroring
 * `crates/serialwrap/src/cli/audit.rs`'s own `--actor` convention (a
 * substring match against the record's raw JSON) rather than a structured
 * per-field lookup: the same identity can appear as `client`,
 * `requester_name`, or inside a `reason` string. Applied client-side, same
 * "display concern" stance T5.2's live-log regex filter already takes. */
export function rowMatchesFilter(row: AuditRow, filter: string): boolean {
  if (filter.trim().length === 0) return true;
  const needle = filter.toLowerCase();
  return JSON.stringify(row).toLowerCase().includes(needle);
}
