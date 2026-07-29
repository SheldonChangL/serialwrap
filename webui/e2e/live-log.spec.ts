// E2E for the live log view (`TASKS.md` T5.2, issue #19). Drives the real
// compiled `serialwrap daemon` binary (`startDaemon({ testDeviceId })` —
// see `daemon.ts`'s doc comment on the `TestBackend` seam this needs) and
// the real built frontend, injecting records through the real
// recorder->query->presentation->WS/tail pipeline via `injectLog`
// (`POST /api/devices/:id/test/inject`) rather than mocking anything in
// the browser.
//
// Every wait below is for an actual observable condition (a DOM attribute/
// text Playwright polls, a real scroll/wheel event, a real HTTP response)
// — never a fixed `waitForTimeout` — per the timing-stability lesson from
// issue #39. Where *time itself* is the thing under test (fps, filter
// elapsed ms), each assertion's threshold is documented with where the
// number comes from — see each test's own comment.
import { expect, test } from "@playwright/test";
import { startDaemon, injectLog, type DaemonHandle, type InjectOp } from "./daemon.js";

let daemon: DaemonHandle | undefined;

const DEVICE_ID = "demo";

test.afterEach(async () => {
  await daemon?.stop();
  daemon = undefined;
});

async function gotoConnectedLiveLog(page: import("@playwright/test").Page): Promise<void> {
  daemon = await startDaemon({ testDeviceId: DEVICE_ID });
  await page.goto(daemon.url);
  await expect(page.getByTestId("connection-dot")).toHaveAttribute("data-state", "open", {
    timeout: 10_000,
  });
}

function rxLines(texts: string[]): InjectOp[] {
  return texts.map((text) => ({ kind: "rx", text: `${text}\n` }));
}

test("status bar shows unavailable error counts, never a bare zero", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  const counts = page.getByTestId("error-counts");
  // `TestBackend::error_counts` (crates/serialwrapd/src/protocol/backend.rs)
  // always reports `Unavailable` — it has no real fd/ioctl underneath, same
  // honest reason macOS itself has none. This proves the GUI's rendering
  // of that wire shape (`{"status":"unavailable"}`), independent of which
  // platform actually runs this test (the CI job is ubuntu-only).
  await expect(counts).toContainText("framing unavailable", { timeout: 10_000 });
  await expect(counts).toContainText("overrun unavailable");
});

test("data lines and broker events are visually and structurally distinct", async ({ page }) => {
  await gotoConnectedLiveLog(page);
  await injectLog(daemon!, DEVICE_ID, [
    { kind: "rx", text: "boot ok\n" },
    { kind: "tx", text: "status\n", client: "claude-code", client_type: "agent", gate: "whitelist" },
  ]);

  await expect(page.getByTestId("log-row")).toHaveCount(2, { timeout: 10_000 });
  const dataRow = page.locator('[data-row-kind="line"]').first();
  const eventRow = page.locator('[data-row-kind="tx"]').first();
  await expect(eventRow).toContainText("claude-code");

  // Structural: distinct `data-row-kind` values, asserted above by locator.
  // Visual: distinct font family and a colored left border on the event
  // row that the data row doesn't have — the UX-design wiki's "device data
  // renders in monospace on the plain surface; broker events render in
  // sans-serif with a coloured band."
  const [dataFont, eventFont] = await Promise.all([
    dataRow.evaluate((el) => getComputedStyle(el).fontFamily),
    eventRow.evaluate((el) => getComputedStyle(el).fontFamily),
  ]);
  expect(dataFont).not.toEqual(eventFont);

  // The criterion is a *coloured* band, so that is what this asserts. Every
  // row now reserves the same-width gutter (`--gutter-w`, see
  // `LogRow.svelte`'s doc comment) and device output leaves it transparent,
  // which keeps the monospace columns aligned across row kinds — so the band
  // is distinguished by color, not by width, and comparing widths would only
  // test the implementation detail that used to make the two differ.
  const [dataBand, eventBand] = await Promise.all([
    dataRow.evaluate((el) => getComputedStyle(el).borderLeftColor),
    eventRow.evaluate((el) => getComputedStyle(el).borderLeftColor),
  ]);
  expect(eventBand).not.toEqual(dataBand);
  expect(dataBand).toMatch(/rgba\(.*,\s*0\)/); // transparent on device output
});

test("duplicate lines fold and binary content collapses to a hex chip, both expandable", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);
  // Trailing 0x0a is load-bearing: `test/inject`'s `data_b64` op writes raw
  // bytes as-is (see `crates/serialwrapd/src/web/api.rs`'s `resolve_bytes`
  // doc comment) with no auto-appended newline, and the query layer's line
  // assembler only ever completes a line on an actual `\n` — an
  // unterminated chunk stays an invisible in-progress "partial" forever.
  const binary = Buffer.from([0xff, 0xfe, 0xfd, 0xfc, 1, 2, 3, 0xff, 0xfe, 0xfd, 0xfc, 1, 2, 3, 0x0a]);
  await injectLog(daemon!, DEVICE_ID, [
    ...rxLines(["read timeout", "read timeout", "read timeout", "read timeout"]),
    { kind: "rx", data_b64: binary.toString("base64") },
  ]);

  const foldRow = page.locator('[data-folded="true"]').first();
  await expect(foldRow).toContainText("expand", { timeout: 10_000 });
  await expect(foldRow).toContainText("4");
  await foldRow.locator(".fold-toggle").click();
  await expect(foldRow).toContainText("collapse");

  const binaryRow = page.locator('[data-binary="true"]').first();
  await expect(binaryRow).toContainText("view as hex", { timeout: 10_000 });
  await binaryRow.locator(".binary-toggle").click();
  await expect(binaryRow).toContainText(/ff fe fd fc/);
});

test("scrolling up pauses following; the pill count is correct; clicking it returns to the tail", async ({
  page,
}) => {
  await gotoConnectedLiveLog(page);

  // More than a viewport's worth (the viewport is a fixed 24rem/22px-rows
  // tall — roughly 17 rows) so there's real scrollable range.
  await injectLog(
    daemon!,
    DEVICE_ID,
    rxLines(Array.from({ length: 80 }, (_, i) => `line ${i}`)),
  );

  const viewport = page.getByTestId("log-viewport");
  await expect(viewport).toHaveAttribute("data-following", "true", { timeout: 10_000 });
  await expect(page.getByTestId("log-row").last()).toContainText("line 79", { timeout: 10_000 });

  const box = await viewport.boundingBox();
  if (!box) throw new Error("live log viewport has no bounding box");
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  // A real wheel gesture — this is what a user scrolling up does, and
  // what the "up-scroll auto-pauses" acceptance criterion means. Not a
  // synthetic `scrollTop` assignment.
  await page.mouse.wheel(0, -400);

  await expect(viewport).toHaveAttribute("data-following", "false", { timeout: 5_000 });
  await expect(page.getByTestId("paused-indicator")).toBeVisible();

  await injectLog(
    daemon!,
    DEVICE_ID,
    rxLines(Array.from({ length: 15 }, (_, i) => `late ${i}`)),
  );
  const pill = page.getByTestId("resume-following-pill");
  await expect(pill).toContainText("15 new lines", { timeout: 10_000 });

  await pill.click();
  await expect(viewport).toHaveAttribute("data-following", "true", { timeout: 5_000 });
  await expect(page.getByTestId("log-row").last()).toContainText("late 14", { timeout: 10_000 });
  await expect(pill).toHaveCount(0);
});

test("DOM node count does not grow linearly with total lines received (virtual scroll)", async ({
  page,
}) => {
  test.setTimeout(60_000);
  await gotoConnectedLiveLog(page);

  async function injectBulk(count: number, offset: number): Promise<void> {
    const CHUNK = 2000;
    for (let start = 0; start < count; start += CHUNK) {
      const n = Math.min(CHUNK, count - start);
      await injectLog(
        daemon!,
        DEVICE_ID,
        rxLines(Array.from({ length: n }, (_, i) => `bulk ${offset + start + i}`)),
      );
    }
  }

  await injectBulk(2_000, 0);
  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), {
      timeout: 20_000,
    })
    .toBeGreaterThanOrEqual(2_000);
  const domCountAt2k = await page.getByTestId("log-row").count();

  await injectBulk(20_000, 2_000);
  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), {
      timeout: 30_000,
    })
    .toBeGreaterThanOrEqual(22_000);
  const domCountAt22k = await page.getByTestId("log-row").count();

  // The point of virtual scrolling: 11x more total lines buffered, but the
  // mounted row count barely moves (a few rows of slack for the exact
  // scroll position at sampling time) instead of scaling with the total.
  expect(domCountAt22k).toBeLessThanOrEqual(domCountAt2k + 5);
  // Sanity bound independent of the comparison above: the fixed 24rem
  // viewport at 22px/row is ~17 rows visible plus 2x10 rows of overscan
  // (`LiveLog.svelte`'s `OVERSCAN`), so lands well under 200 regardless of
  // how many total lines were ever received.
  expect(domCountAt22k).toBeLessThan(200);
});

test("regex filter over ~100k lines completes in <=100ms", async ({ page }) => {
  test.setTimeout(90_000);
  await gotoConnectedLiveLog(page);

  const TOTAL = 100_000;
  const CHUNK = 5_000;
  for (let start = 0; start < TOTAL; start += CHUNK) {
    const ops = rxLines(
      Array.from({ length: CHUNK }, (_, i) => {
        const n = start + i;
        // Every 500th line is a real match, so the filter isn't just
        // scanning to an empty result.
        return n % 500 === 0 ? `NEEDLE ${n}` : `line ${n} filler text`;
      }),
    );
    await injectLog(daemon!, DEVICE_ID, ops);
  }

  await expect
    .poll(async () => Number(await page.getByTestId("buffered-count").textContent()), {
      timeout: 60_000,
    })
    .toBeGreaterThanOrEqual(TOTAL);

  await page.getByTestId("filter-input").fill("NEEDLE");
  // `fill()` dispatches a real `input` event, running the production
  // `applyFilter()` code path (`LiveLog.svelte`) synchronously — the same
  // function call the E2E measures via `filter-elapsed-ms`.
  const elapsedMs = await expect
    .poll(
      async () => {
        const text = await page.getByTestId("filter-elapsed-ms").textContent();
        const value = Number(text);
        return Number.isFinite(value) && value > 0 ? value : null;
      },
      { timeout: 10_000 },
    )
    .not.toBeNull()
    .then(() => page.getByTestId("filter-elapsed-ms").textContent())
    .then((text) => Number(text));

  // The report explicitly asks for the actual measured number, not just
  // pass/fail.
  console.log(`live-log filter over ${TOTAL} lines took ${elapsedMs}ms`);
  expect(elapsedMs).toBeLessThanOrEqual(100);
});

test("sustains >=30fps while a mock device streams 5,000 lines/sec", async ({ page }) => {
  test.setTimeout(30_000);
  await gotoConnectedLiveLog(page);

  const DURATION_MS = 3_000;
  const RATE_PER_SEC = 5_000;
  const BATCH = 100;
  const intervalMs = (BATCH / RATE_PER_SEC) * 1000;

  const fpsPromise = page.evaluate((durationMs) => {
    return new Promise<number>((resolve) => {
      let frames = 0;
      const start = performance.now();
      function tick(): void {
        frames++;
        const elapsed = performance.now() - start;
        if (elapsed < durationMs) {
          requestAnimationFrame(tick);
        } else {
          resolve((frames / elapsed) * 1000);
        }
      }
      requestAnimationFrame(tick);
    });
  }, DURATION_MS);

  const deadline = Date.now() + DURATION_MS;
  let n = 0;
  while (Date.now() < deadline) {
    const batchStart = Date.now();
    const ops = rxLines(Array.from({ length: BATCH }, () => `stream line ${n++}`));
    await injectLog(daemon!, DEVICE_ID, ops);
    const elapsed = Date.now() - batchStart;
    if (elapsed < intervalMs) {
      await new Promise((r) => setTimeout(r, intervalMs - elapsed));
    }
  }

  const fps = await fpsPromise;
  // The report explicitly asks for the actual measured fps, not just
  // pass/fail.
  console.log(`live-log fps while streaming ~${RATE_PER_SEC}/sec: ${fps.toFixed(1)}`);
  // 30fps is the acceptance criterion itself; see the PR report for the
  // actual measured number on this run and on CI, and for why headless
  // Chromium doing simple DOM text updates over a ~40-row virtualized
  // window comfortably clears it with margin.
  expect(fps).toBeGreaterThanOrEqual(30);
});
