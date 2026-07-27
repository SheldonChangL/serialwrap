/**
 * Export dialog data model (`TASKS.md` T5.5, issue #22). Pure logic only —
 * no `fetch` here, since an export download is triggered by navigating an
 * `<a download>` element to `GET /api/devices/:id/export` (see
 * `ExportDialog.svelte`), not by an XHR this module would need to wrap.
 *
 * `GET /api/devices/:id/export` walks the exact same
 * `crate::export::export_range` renderer `serialwrap export` calls
 * in-process (`crates/serialwrapd/src/web/api.rs`'s `export_device` doc
 * comment) — building the right query string here is the *entire*
 * contribution this module makes to the "byte-identical to the CLI"
 * guarantee; the byte-identity itself comes from the daemon side.
 */

export type ExportFormat = "jsonl" | "txt" | "bin";

/** Where the exported range comes from — the three sources T5.5's spec
 * calls out: a timeline drag-selection, a manually typed time range, or
 * `--boot` (most recent boot marker to now). */
export type ExportSource = "selection" | "range" | "boot";

export interface ExportRequest {
  format: ExportFormat;
  source: ExportSource;
  /** Only meaningful for `source: "range"` — a plain seq or an RFC 3339
   * wall-clock string, same as `cli::export`'s own `--from`/`--to`. Empty
   * means "open on that end". */
  from: string;
  to: string;
  /** Only meaningful for `jsonl`/`txt` — `bin` rejects any filter (see
   * [`binFilterConflict`]). */
  filter: string;
}

/** `true` when `format`/`filter` conflict — `bin` cannot be combined with a
 * filter (UX-design/T2.4: "bin 不允許過濾，保證完整性"). The GUI must block
 * this outright rather than silently dropping the filter (T5.5 acceptance
 * criterion 6) — this predicate is what [`ExportDialog.svelte`] uses to
 * disable the Export action and show a blocking message, and it's checked
 * independently of the daemon's own identical rejection inside
 * `export_range` (defense in depth, not a substitute for it). */
export function binFilterConflict(format: ExportFormat, filter: string): boolean {
  return format === "bin" && filter.trim().length > 0;
}

/** `true` when `req.source` has everything it needs to build a valid
 * request — `"selection"` needs an actual timeline selection to exist
 * (`hasSelection`), the other two sources always have enough (an empty
 * `range` just means "the full retained history", and `"boot"` needs
 * nothing extra). */
export function sourceIsReady(req: Pick<ExportRequest, "source">, hasSelection: boolean): boolean {
  if (req.source === "selection") return hasSelection;
  return true;
}

/** Build the export URL for `req` against `deviceId` — the one place a
 * source/format/filter combination turns into the actual query string
 * `GET /api/devices/:id/export` understands. `selection`'s bounds are
 * passed as plain seq numbers (not wall-clock strings) since the timeline
 * selection already resolved both forms and seq is the unambiguous,
 * boundary-exact one. */
export function buildExportUrl(
  deviceId: string,
  req: ExportRequest,
  selection: { fromSeq: number; toSeq: number } | null,
): string {
  const params = new URLSearchParams();
  params.set("format", req.format);

  if (req.source === "boot") {
    params.set("boot", "true");
  } else if (req.source === "selection" && selection) {
    params.set("from", String(selection.fromSeq));
    params.set("to", String(selection.toSeq));
  } else if (req.source === "range") {
    if (req.from.trim().length > 0) params.set("from", req.from.trim());
    if (req.to.trim().length > 0) params.set("to", req.to.trim());
  }

  if (!binFilterConflict(req.format, req.filter) && req.filter.trim().length > 0) {
    params.set("filter", req.filter.trim());
  }

  return `/api/devices/${encodeURIComponent(deviceId)}/export?${params.toString()}`;
}
