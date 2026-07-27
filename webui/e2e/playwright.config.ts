import { defineConfig } from "@playwright/test";

// This suite spawns real `serialwrap daemon` subprocesses bound to real
// TCP ports (see `daemon.ts`) rather than using Playwright's `webServer`
// option, because the WS-reconnect scenario needs to kill and restart the
// server mid-test. `workers: 1` keeps every test's port allocation and
// throwaway HOME dir from racing another test's — a deliberate
// simplification for this small, infrastructure-only suite (see the T5.1
// report's known limitations).
export default defineConfig({
  testDir: ".",
  outputDir: "./test-results",
  timeout: 30_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI
    ? [["github"], ["html", { open: "never", outputFolder: "./playwright-report" }]]
    : "list",
  use: {
    trace: "retain-on-failure",
  },
});
