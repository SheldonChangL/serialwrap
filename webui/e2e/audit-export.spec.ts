// E2E for the audit panel and export dialog (`TASKS.md` T5.5, issue #22).
//
// Every wait is for an actual observable condition — never a fixed
// `waitForTimeout` — per the timing-stability lesson from issue #39.
import { expect, test, type Page } from "@playwright/test";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { injectLog, runCli, startDaemon, type DaemonHandle } from "./daemon.js";

let daemon: DaemonHandle | undefined;

const DEVICE_ID = "demo";

test.afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

async function gotoConnectedLiveLog(page: Page): Promise<void> {
  daemon = await startDaemon({ testDeviceId: DEVICE_ID });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });
}

/** The audit trail is a status-bar drawer now rather than a card below the
 * log (see `App.svelte`'s layout doc comment). Export stayed a dialog and is
 * still opened straight from the status bar, so only the audit tests need
 * this. */
async function openAuditDrawer(page: Page): Promise<void> {
  await page.getByTestId("open-audit").click();
  await expect(page.getByTestId("audit-drawer")).toBeVisible({ timeout: 5_000 });
}

// ---- T5.5 acceptance criterion 1: audit "jump to log" lands on the exact seq ----

test("audit panel's jump-to-log lands on the exact record it names", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "boot ok\n" },
    { kind: "tx", text: "status\n", client: "claude-code", client_type: "agent", gate: "whitelist" },
  ]);
  await openAuditDrawer(page);

  const auditRow = page.getByTestId("audit-row").filter({ hasText: "claude-code" }).first();
  await expect(auditRow).toBeVisible({ timeout: 5_000 });
  const seq = await auditRow.getAttribute("data-seq");
  expect(seq).toBeTruthy();

  await auditRow.getByTestId("audit-row-toggle").click();
  await auditRow.getByTestId("audit-jump-to-log").click();

  // The row this lands on is identified purely by `seq` — the same
  // sequence number both the audit panel and the main log view already
  // render for the identical record (no correlation lookup involved).
  const logRow = page.locator(`[data-testid="log-row"][data-seq="${seq}"]`);
  await expect(logRow).toBeVisible({ timeout: 5_000 });
  await expect(logRow).toHaveAttribute("data-highlighted", "true");
});

// ---- T5.5 acceptance criterion 2: GUI export byte-identical to CLI export ----

async function cliExportToFile(daemon: DaemonHandle, args: string[]): Promise<Buffer> {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "serialwrap-export-e2e-"));
  const outFile = path.join(tmp, "out.bin");
  try {
    const result = await runCli(daemon, [...args, "-o", outFile]);
    expect(result.code, `CLI export failed (code ${result.code}): ${result.stderr}`).toBe(0);
    return fs.readFileSync(outFile);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

test("GUI export is byte-identical to the CLI's for the same parameters, in all three formats", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "boot ok\n" },
    { kind: "tx", text: "status\n", client: "claude-code", client_type: "agent", gate: "whitelist" },
    { kind: "rx", data_b64: Buffer.from([0x00, 0x01, 0xff, 0xfe]).toString("base64") },
    { kind: "event", name: "config_change", extra: { field: "baud", old: 9600, new: 115200 } },
    { kind: "gate", action: "deny", reason: "timeout_60s", request_seq: 0 },
  ]);

  for (const format of ["jsonl", "txt", "bin"] as const) {
    const guiRes = await page.request.get(`${daemon!.url}/api/devices/${DEVICE_ID}/export?format=${format}`);
    expect(guiRes.ok(), `GUI export (${format}) failed: ${guiRes.status()}`).toBe(true);
    const guiBytes = await guiRes.body();

    const cliBytes = await cliExportToFile(daemon!, ["export", DEVICE_ID, "--format", format]);

    expect(Buffer.compare(guiBytes, cliBytes), `GUI vs CLI byte mismatch for format=${format}`).toBe(0);
    expect(guiBytes.length).toBeGreaterThan(0);
  }
});

test("GUI export with boot=true matches the CLI's --boot for the same device", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "before boot\n" },
    { kind: "event", name: "connect", extra: {} },
    { kind: "rx", text: "after boot\n" },
  ]);

  const guiRes = await page.request.get(`${daemon!.url}/api/devices/${DEVICE_ID}/export?format=txt&boot=true`);
  expect(guiRes.ok()).toBe(true);
  const guiBytes = await guiRes.body();

  const cliBytes = await cliExportToFile(daemon!, ["export", DEVICE_ID, "--format", "txt", "--boot"]);
  expect(Buffer.compare(guiBytes, cliBytes)).toBe(0);

  const text = guiBytes.toString("utf8");
  expect(text).toContain("after boot");
  expect(text).not.toContain("before boot");
});

// ---- T5.5 acceptance criterion 6: bin + filter must be explicitly blocked ----

test("export dialog blocks bin format combined with a filter, rather than silently dropping it", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);

  await page.getByTestId("export-dialog-open").click();
  const dialog = page.getByTestId("export-dialog");
  await expect(dialog).toBeVisible({ timeout: 5_000 });

  await page.getByTestId("export-format-bin").check();
  await page.getByTestId("export-filter").fill("ERROR");

  await expect(page.getByTestId("export-bin-filter-error")).toBeVisible();
  const exportControl = page.getByTestId("export-download");
  await expect(exportControl).toBeDisabled();
  // A blocked export renders as a disabled `<button>`, never an `<a
  // href download>` a user could still trigger — the control's own tag
  // proves there is no way to navigate/download while blocked, not just
  // that it looks greyed out.
  const tagName = await exportControl.evaluate((el) => el.tagName.toLowerCase());
  expect(tagName).toBe("button");

  // Clearing the filter (still `bin`) un-blocks it.
  await page.getByTestId("export-filter").fill("");
  await expect(page.getByTestId("export-bin-filter-error")).toHaveCount(0);
  await expect(page.getByTestId("export-download")).toBeEnabled();
});
