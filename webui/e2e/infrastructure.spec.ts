// E2E for the web infrastructure foundation (`TASKS.md` T5.1, issue #18).
// Drives the real compiled `serialwrap daemon` binary + the real built
// frontend — no mocking of either. Scope is deliberately narrow: this task
// proves the foundation (WS connects, connection status is honest, one API
// call renders), not the log view/timeline/approvals/exports that land in
// T5.2-T5.5.
//
// Every wait below is for an actual observable condition (a DOM attribute
// Playwright polls, or a real HTTP response) — never a fixed
// `waitForTimeout` — per the timing-stability lesson from issue #39.
import { expect, test } from "@playwright/test";
import { startDaemon, type DaemonHandle } from "./daemon.js";

let daemon: DaemonHandle | undefined;

test.afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

test("serves the embedded frontend with no separate frontend service, connects over WS, and renders a real API result", async ({
  page,
}) => {
  daemon = await startDaemon();
  await page.goto(daemon.url);

  const status = page.getByTestId("connection-status");
  await expect(status).toHaveAttribute("data-state", "open", { timeout: 10_000 });
  await expect(status).toContainText("Connected");

  // The full port list moved into a status-bar drawer (see `App.svelte`'s
  // layout doc comment); this assertion is about the HTTP round-trip
  // rendering real API data, so open the drawer that holds it.
  await page.getByTestId("open-devices").click();
  const devices = page.getByTestId("device-list");
  await expect(devices).toHaveAttribute("data-state", "loaded", { timeout: 10_000 });
});

test("shows an honest disconnected state and auto-reconnects after the daemon restarts", async ({
  page,
}) => {
  daemon = await startDaemon();
  await page.goto(daemon.url);

  const status = page.getByTestId("connection-status");
  await expect(status).toHaveAttribute("data-state", "open", { timeout: 10_000 });

  const port = daemon.port;
  await daemon.stop();
  daemon = undefined;

  // The acceptance criterion this exists for: the UI must say it's
  // disconnected, not silently keep showing "Connected" with stale data.
  await expect(status).toHaveAttribute("data-state", "closed", { timeout: 10_000 });
  await expect(status).toContainText("Disconnected");

  daemon = await startDaemon(port);

  await expect(status).toHaveAttribute("data-state", "open", { timeout: 15_000 });
  await expect(status).toContainText("Connected");
});
