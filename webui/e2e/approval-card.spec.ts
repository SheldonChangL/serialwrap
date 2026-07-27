// E2E for the approval card (`TASKS.md` T5.4, issue #21). Drives the real
// compiled `serialwrap daemon` binary plus, for the "GUI and CLI don't
// double-decide" test, the real `serialwrap` CLI as a second client of the
// same daemon (`runCli`) — and the real built frontend throughout.
//
// A real MCP/UDS "agent" client is impractical to open from a browser test,
// so "an agent triggers a pending write" is simulated via
// `POST /api/devices/:id/test/submit_write` (`crates/serialwrapd/src/web/api.rs`'s
// `test_submit_write`, gated the same way `test/inject` is) — this still
// drives the real `Gate::submit_write` → `PendingQueue` → WS-push →
// `GET /api/approvals` → decide pipeline end to end; only the "an agent
// asked" step itself is synthesized rather than opened over a real MCP
// connection.
//
// Every wait is for an actual observable condition — never a fixed
// `waitForTimeout` — per the timing-stability lesson from issue #39.
import { expect, test, type Page } from "@playwright/test";
import { startDaemon, injectLog, runCli, type DaemonHandle } from "./daemon.js";

let daemon: DaemonHandle | undefined;

const DEVICE_ID = "demo";

test.afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

async function gotoConnectedLiveLog(page: Page, rulesToml?: string): Promise<void> {
  daemon = await startDaemon({ testDeviceId: DEVICE_ID, rulesToml });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });
}

interface SubmitWriteResult {
  decision: string;
  id?: number;
  matched_rule?: string;
}

async function submitPendingWrite(
  page: Page,
  body: {
    text: string;
    requester_name?: string;
    requester_pid?: number;
  },
): Promise<SubmitWriteResult> {
  const res = await page.request.post(`${daemon!.url}/api/devices/${DEVICE_ID}/test/submit_write`, {
    data: {
      requester_name: "claude-code",
      requester_pid: 4242,
      ...body,
    },
  });
  expect(res.ok()).toBe(true);
  return (await res.json()) as SubmitWriteResult;
}

// ---- Acceptance criterion 7: end-to-end approval ----

test("agent triggers a pending write, the card appears, approving it executes the write and audits it", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);

  const before = Date.now();
  const submitted = await submitPendingWrite(page, { text: "custom_cmd" });
  expect(submitted.decision).toBe("pending");

  const card = page.getByTestId("approval-card");
  await expect(card).toBeVisible({ timeout: 3_000 });
  expect(Date.now() - before).toBeLessThan(3_000);
  await expect(card).toContainText("claude-code");

  await card.getByTestId("approval-allow-once").click();
  await expect(card).toHaveCount(0, { timeout: 5_000 });

  // "指令執行" + "稽核有紀錄": a `tx` record tagged `approved_by:` lands in
  // the same device's stream and is visible in the live log.
  const txRow = page.locator('[data-row-kind="tx"]').filter({ hasText: "claude-code" });
  await expect(txRow).toBeVisible({ timeout: 5_000 });
  await expect(txRow.locator(".gate-badge")).toContainText("approved_by:", { timeout: 5_000 });
});

// ---- Acceptance criterion 8: countdown timeout ----

test("countdown reaching zero flips the card to timed-out with no clickable buttons left", async ({ page }) => {
  // A 2s timeout keeps this test fast without racing the assertion against
  // the real countdown — `rulesToml` writes this into the daemon's own
  // throwaway config dir before it starts (see `daemon.ts`'s
  // `rulesTomlPath`), so the *actual* server-side fail-safe timeout (not
  // just a client-side visual) is what resolves this request.
  await gotoConnectedLiveLog(page, "[approval]\ntimeout_s = 2\n");
  const submitted = await submitPendingWrite(page, { text: "custom_cmd" });
  const card = page.getByTestId("approval-card");
  await expect(card).toBeVisible({ timeout: 3_000 });

  await expect(card.getByTestId("approval-timed-out-banner")).toBeVisible({ timeout: 6_000 });
  await expect(card.getByTestId("approval-deny")).toHaveCount(0);
  await expect(card.getByTestId("approval-allow-once")).toHaveCount(0);

  // The daemon's own fail-safe timeout must actually have denied it —
  // confirmed independent of the card's own local countdown display.
  await expect
    .poll(
      async () => {
        const res = await page.request.get(`${daemon!.url}/api/approvals`);
        const { approvals } = (await res.json()) as { approvals: Array<{ id: number }> };
        return approvals.some((a) => a.id === submitted.id);
      },
      { timeout: 5_000 },
    )
    .toBe(false);
});

// ---- Acceptance criterion 9: GUI and CLI must not both decide ----

test("the GUI and a concurrent CLI decision on the same request never both succeed", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const submitted = await submitPendingWrite(page, { text: "custom_cmd" });
  const id = submitted.id!;

  const card = page.getByTestId("approval-card");
  await expect(card).toBeVisible({ timeout: 3_000 });

  // The CLI decides first (deterministically, rather than literally racing
  // two async browser/process actions against each other, which is racy by
  // construction and would make this test flaky without proving anything
  // more about the underlying atomicity — that atomicity is already
  // covered directly, at both the `PendingQueue::decide` and HTTP-handler
  // levels, by `crates/serialwrapd/src/gate/approval.rs`'s and
  // `crates/serialwrapd/src/web/api.rs`'s own Rust test suites). What this
  // E2E test proves is what those can't: that a GUI decision attempt on a
  // request the CLI *just* resolved gets a clean, structured rejection —
  // never a silent second success — and that the GUI eventually reflects
  // the resolved state.
  const cliResult = await runCli(daemon!, ["approvals", "deny", String(id)]);
  expect(cliResult.code).toBe(0);

  // A GUI decision attempt on the same, now-already-decided id — via the
  // exact endpoint "Allow once" itself calls — must not also succeed.
  const approveRes = await page.request.post(`${daemon!.url}/api/approvals/${id}/approve`);
  expect(approveRes.status()).toBe(409);

  // `GET /tail` reads through `DeviceQueryState`'s own cached, background-
  // polled view of the recorder (`crates/serialwrapd/src/query.rs`'s
  // `spawn_poller`, a 5ms tick this task's scope doesn't touch) rather than
  // the recorder directly — a CLI decision made over a wholly separate UDS
  // connection has no reason to be visible there *synchronously*, only
  // within that poll's own small latency budget. Polling here (never a
  // fixed sleep) is exactly this project's own test-discipline stance
  // (`TASKS.md`'s "測試紀律" section, issue #39) applied to that budget.
  let decisions: Array<Record<string, unknown>> = [];
  await expect
    .poll(
      async () => {
        const tailRes = await page.request.get(`${daemon!.url}/api/devices/${DEVICE_ID}/tail?n=1000`);
        const tail = (await tailRes.json()) as { events: Array<Record<string, unknown>> };
        decisions = tail.events.filter(
          (e) => e.kind === "gate" && e.request_seq === id && (e.action === "approve" || e.action === "deny"),
        );
        return decisions.length;
      },
      { timeout: 2_000 },
    )
    .toBe(1);
  expect(decisions[0].action).toBe("deny");

  // "後到者要看到已決狀態" (Security-model wiki's approval-flow section)
  // applied to the GUI as the later party here: the card must converge to
  // a settled, no-longer-actionable state rather than staying stuck
  // showing live countdown/buttons for a request that's actually already
  // resolved.
  await expect(card.getByTestId("approval-timed-out-banner")).toBeVisible({ timeout: 5_000 });
  await expect(card.getByTestId("approval-allow-once")).toHaveCount(0);
  await expect(card.getByTestId("approval-deny")).toHaveCount(0);
});

// ---- Acceptance criterion 10: focus safety ----

test("Allow once is never the default focus", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await submitPendingWrite(page, { text: "custom_cmd" });
  await expect(page.getByTestId("approval-card")).toBeVisible({ timeout: 3_000 });

  const allowIsFocused = await page.evaluate(
    () => document.activeElement?.getAttribute("data-testid") === "approval-allow-once",
  );
  expect(allowIsFocused).toBe(false);
});

// ---- Acceptance criterion 11: danger pattern disables the whitelist checkbox ----

test("a danger-pattern request disables the whitelist checkbox", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const submitted = await submitPendingWrite(page, { text: "flash_erase 0x0 0x100000" });
  expect(submitted.decision).toBe("force_pending");
  expect(submitted.matched_rule).toBe("danger:erase");

  const card = page.getByTestId("approval-card");
  await expect(card).toBeVisible({ timeout: 3_000 });
  await expect(card.getByTestId("approval-whitelist-checkbox")).toBeDisabled();
  await expect(card.getByTestId("approval-whitelist-checkbox")).not.toBeChecked();
});

// ---- Acceptance criterion 12: log context before the request ----

test("the approval card shows the log lines immediately before the request", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "ota: partition check failed\n" },
    { kind: "rx", text: "ota: image invalid, rollback armed\n" },
  ]);
  await submitPendingWrite(page, { text: "flash_erase 0x0 0x100000" });

  const card = page.getByTestId("approval-card");
  await expect(card).toBeVisible({ timeout: 3_000 });
  const context = card.getByTestId("approval-log-context");
  await expect(context).toContainText("ota: partition check failed");
  await expect(context).toContainText("ota: image invalid, rollback armed");
});
