// E2E for the clients panel (`TASKS.md` T5.5, issue #22) and the M5 exit
// scenario S2 ("人機共視" — a human at the GUI and an agent over the raw
// UDS/MCP protocol observing the same device simultaneously).
//
// A real MCP bridge is impractical to open from a browser test (see
// `approval-card.spec.ts`'s module doc comment for the same stance on a
// different criterion) — `connectRawClient` (`daemon.ts`) is the escape
// hatch: it speaks the same newline-delimited-JSON wire protocol a real
// MCP/CLI client does, over a real UDS connection to the same daemon this
// test's GUI tab is looking at. This is the only place in the suite that
// needs it — the kick/wait_for criteria below are specifically about a real
// connection's own socket state, which a synthesized HTTP call
// (`test_inject`/`test_submit_write`) cannot observe.
//
// Every wait is for an actual observable condition — never a fixed
// `waitForTimeout` — per the timing-stability lesson from issue #39.
import { expect, test, type Page } from "@playwright/test";
import { connectRawClient, injectLog, startDaemon, type DaemonHandle, type RawClient } from "./daemon.js";

let daemon: DaemonHandle | undefined;
let rawClients: RawClient[] = [];

const DEVICE_ID = "demo";

test.afterEach(async () => {
  for (const client of rawClients) client.close();
  rawClients = [];
  await daemon?.stop();
  daemon = undefined;
});

async function gotoConnectedLiveLog(page: Page): Promise<void> {
  daemon = await startDaemon({ testDeviceId: DEVICE_ID });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });
  // The clients list is a status-bar drawer now, not a permanently-mounted
  // card below the log (see `App.svelte`'s layout doc comment) — every
  // assertion below is about the panel's contents, so open it once here.
  await page.getByTestId("open-clients").click();
  await expect(page.getByTestId("clients-drawer")).toBeVisible({ timeout: 5_000 });
}

// ---- T5.5 acceptance criterion 4: agent's wait_for pattern + remaining time ----

test("clients panel shows an agent's wait_for pattern and remaining time", async ({ page }) => {
  await gotoConnectedLiveLog(page);

  const agent = await connectRawClient(daemon!, { name: "claude-code", type: "agent" });
  rawClients.push(agent);
  // A pattern that will never match this device's (empty) stream, with a
  // generous timeout — this is a real, in-flight `wait_for` call on the
  // real `DeviceQueryState`, the same op an MCP `wait_for` tool sends.
  agent.send({ id: 1, op: "wait_for", device: DEVICE_ID, pattern: "OTA done", timeout_s: 20 });

  const row = page.getByTestId("client-row").filter({ hasText: "claude-code" });
  await expect(row).toBeVisible({ timeout: 5_000 });

  const waiting = row.getByTestId("client-waiting");
  await expect(waiting).toBeVisible({ timeout: 5_000 });
  await expect(waiting).toContainText("OTA done");
  await expect(waiting).toHaveAttribute("data-pattern", "OTA done");

  const remaining = Number(await waiting.getAttribute("data-remaining-s"));
  expect(remaining).toBeGreaterThan(0);
  expect(remaining).toBeLessThanOrEqual(20);
});

// ---- T5.5 acceptance criterion 5: a finished lease stays listed ----

test("clients panel keeps a finished lease listed after it ends", async ({ page }) => {
  await gotoConnectedLiveLog(page);

  // A `lease_end` event carries everything `port::append_lease_end_event`
  // records for a real lease (`command`/`pid`/`duration_ms`/`exit_code`/
  // `reason`) — injected here exactly the way a real `esptool` lease
  // ending would append it, via the real recorder (`test/inject`).
  await injectLog(daemon!, DEVICE_ID, [
    {
      kind: "event",
      name: "lease_end",
      extra: {
        device_id: DEVICE_ID,
        command: "esptool.py write_flash 0x0 firmware.bin",
        pid: 5311,
        token: "tok-1",
        exit_code: 0,
        duration_ms: 46_000,
        reason: "released",
      },
    },
  ]);

  const row = page.locator('[data-testid="client-row"][data-status="offline"]');
  await expect(row).toBeVisible({ timeout: 5_000 });
  await expect(row).toContainText("esptool");
  await expect(row.getByTestId("finished-lease-detail")).toContainText("46s");
  await expect(row.getByTestId("finished-lease-detail")).toContainText("esptool.py write_flash");
});

// ---- T5.5 acceptance criterion 3: kicking a client closes its connection ----

test("kicking an agent closes its connection with an observable error, not a silent hang", async ({ page }) => {
  await gotoConnectedLiveLog(page);

  const agent = await connectRawClient(daemon!, { name: "claude-code", type: "agent" });
  rawClients.push(agent);
  // A long-running in-flight request, not just an idle connection — a kick
  // must terminate the socket out from under a *pending* call, which is
  // what proves an MCP tool mid-call sees a definite connection error
  // rather than hanging until its own (here, 60s) timeout.
  agent.send({ id: 1, op: "wait_for", device: DEVICE_ID, pattern: "never-matches-anything", timeout_s: 60 });

  const row = page.getByTestId("client-row").filter({ hasText: "claude-code" });
  await expect(row).toBeVisible({ timeout: 5_000 });
  await expect(row.getByTestId("client-waiting")).toBeVisible({ timeout: 5_000 });

  await row.getByTestId("kick-button").click();

  // The kicked connection's own socket must close observably — Playwright's
  // own test timeout (30s, `playwright.config.ts`) bounds this wait, so a
  // regression that leaves the connection silently hanging fails loudly
  // rather than never resolving unnoticed.
  await agent.waitForClose();
  expect(agent.isClosed()).toBe(true);

  // The clients panel must reflect the kick too (the row disappears once
  // the registry actually unregisters the torn-down connection).
  await expect(row).toHaveCount(0, { timeout: 5_000 });
});

// ---- M5 exit scenario S2: human (GUI) and agent (raw UDS) share one seq ----

/**
 * Poll a raw client's `tail` op until a line with exact `text` shows up, or
 * `deadlineMs` elapses. Needed because `test/inject`'s `POST` writes
 * straight to the recorder, but a UDS `tail` reply is served from a
 * `DeviceQueryState` that's only refreshed by its own background poller
 * (`crate::query::spawn_poller`) on an already-open connection's cached
 * state — a real, small, race-prone latency window, not something a single
 * fixed-timing `tail` call after the injection can assume has already
 * closed. Bounded polling (not a fixed sleep) for the real event, same
 * discipline `crates/serialwrapd/src/web/api.rs`'s own `wait_until` test
 * helper and this suite's `expect.poll` calls already follow.
 */
async function pollAgentTailForLine(
  agent: RawClient,
  device: string,
  text: string,
  deadlineMs = 2_000,
): Promise<{ text: string; seq: number }> {
  const deadline = Date.now() + deadlineMs;
  let id = 100;
  for (;;) {
    agent.send({ id: id++, op: "tail", device, n: 50 });
    const reply = await agent.nextMessage();
    const lines = (reply.lines ?? []) as Array<{ text: string; seq: number }>;
    const found = lines.find((l) => l.text === text);
    if (found) return found;
    if (Date.now() > deadline) {
      throw new Error(`line ${JSON.stringify(text)} not seen via tail within ${deadlineMs}ms`);
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

test("S2: a human at the GUI and an agent over raw UDS reference the same seq for the same line", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [{ kind: "rx", text: "boot ok, all systems nominal\n" }]);

  // Agent side: the same `tail` op an MCP bridge's `tail` tool sends, over
  // a real UDS connection — not a browser call.
  const agent = await connectRawClient(daemon!, { name: "claude-code", type: "agent" });
  rawClients.push(agent);
  const agentLine = await pollAgentTailForLine(agent, DEVICE_ID, "boot ok, all systems nominal");
  const agentSeq = agentLine.seq;

  // Human side: the same content's row in the rendered live log.
  const row = page.locator('[data-testid="log-row"][data-row-kind="line"]', {
    hasText: "boot ok, all systems nominal",
  });
  await expect(row).toBeVisible({ timeout: 5_000 });
  await expect(row).toHaveAttribute("data-seq", String(agentSeq));

  // The agent's own read is unaffected by anything the human did in the
  // GUI (no scrolling/filter state leaks across connections) — a second
  // `read_since` from the same cursor sees no unexpected gap.
  agent.send({ id: 2, op: "read_since", device: DEVICE_ID, cursor: agentSeq });
  const followUp = await agent.nextMessage();
  expect(followUp.ok).toBe(true);
});
