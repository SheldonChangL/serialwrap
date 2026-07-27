// Spawns/kills the real, compiled `serialwrap daemon` binary for E2E tests
// (`TASKS.md` T5.1, issue #18). Deliberately drives the actual production
// entry point (not an in-process test double) — this suite exists
// specifically to prove the "open a browser, no separate frontend service"
// acceptance criterion end to end, including a real daemon restart for the
// WS reconnect criterion.
import { spawn, type ChildProcess } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

function resolveBinary(): string {
  if (process.env.SERIALWRAP_BIN) return process.env.SERIALWRAP_BIN;
  const profile = process.env.SERIALWRAP_PROFILE ?? "release";
  return path.join(REPO_ROOT, "target", profile, "serialwrap");
}

export interface DaemonHandle {
  port: number;
  url: string;
  /** This instance's throwaway `HOME`/XDG dirs — exposed so `runCli` (T5.4,
   * issue #21's "GUI and CLI decide the same request" test) can spawn the
   * `serialwrap` CLI against the *same* daemon, pointed at the same UDS
   * socket and `rules.toml`, rather than a second, unrelated instance. */
  home: string;
  stop(): Promise<void>;
}

// Known limitation (deferred, not fixed here): auto-allocated ports are a
// fixed, incrementing sequence rather than OS-assigned ephemeral ports. If
// a previous run's daemon were ever orphaned (e.g. the process survived a
// hard test-runner kill) it could still be holding one of these ports,
// causing the next run's `startDaemon()` to fail its health check against
// the wrong (stale) daemon. `workers: 1` plus each daemon's own throwaway
// HOME dir makes this unlikely in practice; escalate to real OS-assigned
// ports (bind `0`, read back the actual port) if this is ever observed
// flaking in CI — same category of fix as issue #39.
let nextPort = 15590;

async function waitForHealthy(url: string, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  let lastErr: unknown = new Error("never attempted");
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
      lastErr = new Error(`GET ${url} -> ${res.status}`);
    } catch (e) {
      lastErr = e;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`daemon never became healthy at ${url}: ${String(lastErr)}`);
}

/**
 * Start a real `serialwrap daemon` subprocess bound to `port` (or a fresh
 * one, auto-allocated, if omitted — pass an explicit port to restart on the
 * *same* address, which the WS-reconnect test needs since the page's
 * `location.host` doesn't change across a daemon restart).
 *
 * Each instance gets its own throwaway `HOME`/XDG dirs so its UDS socket,
 * recorder data dir, and `rules.toml` lookup never collide with a real
 * user's `~/.serialwrap` or with other concurrently-running test daemons.
 */
export interface StartDaemonOptions {
  port?: number;
  /**
   * Registers a `TestBackend`-backed device named this instead of the
   * real `SystemEnumerator`/hotplug path (`TASKS.md` T5.2, issue #19) —
   * see `serialwrapd::TEST_BACKEND_DEVICE_ENV`'s doc comment for why this
   * exists and why it's safe. Every T5.2 live-log test (`live-log.spec.ts`)
   * needs *some* device with a real recorder behind it to inject records
   * into (`POST /api/devices/:id/test/inject`, see `injectLog` below);
   * `infrastructure.spec.ts`'s T5.1 tests never pass this, so they keep
   * exercising the zero-devices case exactly as before.
   */
  testDeviceId?: string;
  /**
   * `rules.toml` contents to write into this instance's throwaway config
   * dir *before* spawning — lets a test configure e.g. a short
   * `[approval] timeout_s` (T5.4, issue #21's countdown-timeout acceptance
   * criterion needs seconds, not the production 60s default) without
   * touching `crates/serialwrapd/src/gate/rules.rs`'s own defaults. See
   * `rulesTomlPath` for why the destination differs by platform.
   */
  rulesToml?: string;
}

/**
 * Where `serialwrapd::gate::rules::default_rules_path` resolves to for a
 * daemon started with `HOME`/`XDG_CONFIG_HOME` both set to `home` (see
 * `startDaemon`'s env block below) — mirrors the `directories` crate's own
 * platform split that function's doc comment describes: XDG-based on
 * Linux (respects `XDG_CONFIG_HOME`), `~/Library/Application Support` on
 * macOS (which `directories` resolves from `HOME` directly, ignoring any
 * XDG var — verified against that crate's source before writing this,
 * same "don't assume, check" standard the daemon's own code documents for
 * this same function).
 */
function rulesTomlPath(home: string): string {
  if (process.platform === "darwin") {
    return path.join(home, "Library", "Application Support", "serialwrap", "rules.toml");
  }
  return path.join(home, "serialwrap", "rules.toml");
}

export async function startDaemon(options?: number | StartDaemonOptions): Promise<DaemonHandle> {
  const opts: StartDaemonOptions = typeof options === "number" ? { port: options } : (options ?? {});
  const usePort = opts.port ?? nextPort++;
  const home = mkdtempSync(path.join(tmpdir(), "serialwrap-e2e-"));

  if (opts.rulesToml) {
    const rulesPath = rulesTomlPath(home);
    mkdirSync(path.dirname(rulesPath), { recursive: true });
    writeFileSync(rulesPath, opts.rulesToml);
  }

  let child: ChildProcess | undefined;
  let lastError: unknown;
  // A daemon killed moments ago can occasionally still be finishing socket
  // teardown; a couple of short retries absorbs that without resorting to
  // a fixed sleep before the very first attempt.
  for (let attempt = 0; attempt < 3; attempt++) {
    child = spawn(resolveBinary(), ["daemon"], {
      env: {
        ...process.env,
        SERIALWRAP_WEB_PORT: String(usePort),
        HOME: home,
        XDG_RUNTIME_DIR: home,
        XDG_DATA_HOME: home,
        XDG_CONFIG_HOME: home,
        ...(opts.testDeviceId ? { SERIALWRAP_TEST_BACKEND_DEVICE: opts.testDeviceId } : {}),
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stderr: Buffer[] = [];
    child.stderr?.on("data", (chunk: Buffer) => stderr.push(chunk));

    try {
      await waitForHealthy(`http://127.0.0.1:${usePort}/api/health`, 5_000);
      const stopper = child;
      return {
        port: usePort,
        url: `http://127.0.0.1:${usePort}`,
        home,
        async stop() {
          // `exit` is not a sticky/replayable event: if `stopper` already
          // exited on its own (crashed, or an earlier `stop()` call raced
          // with this one) before this listener attaches, awaiting a new
          // `once("exit", ...)` would hang forever — Node never re-fires
          // an event nothing was listening for the first time. Checking
          // `exitCode`/`signalCode` (both non-null once a child has
          // actually exited) lets an already-dead process skip straight
          // to cleanup instead of hanging the test's `afterEach` until
          // Playwright's own timeout, which would report an opaque hook
          // timeout instead of the real failure.
          if (stopper.exitCode === null && stopper.signalCode === null) {
            const exited = new Promise<void>((resolve) => stopper.once("exit", () => resolve()));
            stopper.kill("SIGKILL");
            await exited;
          }
          rmSync(home, { recursive: true, force: true });
        },
      };
    } catch (e) {
      lastError = new Error(`${String(e)}\nstderr:\n${Buffer.concat(stderr).toString("utf8")}`);
      child.kill("SIGKILL");
      await new Promise((r) => setTimeout(r, 200));
    }
  }
  throw lastError instanceof Error ? lastError : new Error(String(lastError));
}

/** One record `injectLog` can append — mirrors `serialwrapd::web::api`'s
 * `InjectOp` wire shape exactly (`crates/serialwrapd/src/web/api.rs`). */
export type InjectOp =
  | { kind: "rx"; text?: string; data_b64?: string }
  | { kind: "tx"; text?: string; data_b64?: string; client: string; client_type: "human" | "agent" | "tool"; gate: string }
  | { kind: "event"; name: string; extra?: Record<string, unknown> }
  | { kind: "gate"; action: string; reason: string; request_seq: number };

/**
 * Append records to a `startDaemon({ testDeviceId })`-registered device's
 * real recorder via `POST /api/devices/:id/test/inject` — the seam every
 * T5.2 live-log E2E test uses to put real data through the real
 * recorder→query→presentation→WS/tail pipeline (`crates/serialwrapd/src/web/api.rs`'s
 * `test_inject` handler; `serialwrapd::TEST_BACKEND_DEVICE_ENV`'s doc
 * comment covers why this only ever does anything against a
 * `testDeviceId`-started daemon).
 */
export async function injectLog(daemon: DaemonHandle, deviceId: string, ops: InjectOp[]): Promise<void> {
  const res = await fetch(`${daemon.url}/api/devices/${encodeURIComponent(deviceId)}/test/inject`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ ops }),
  });
  if (!res.ok) {
    throw new Error(`test/inject failed: ${res.status} ${await res.text()}`);
  }
}

export interface CliResult {
  code: number | null;
  stdout: string;
  stderr: string;
}

/**
 * Run the real `serialwrap` CLI binary against `daemon`'s own socket
 * (T5.4, issue #21's "GUI and CLI decide the same request" acceptance
 * criterion) — same binary `resolveBinary()` picks for the daemon itself,
 * invoked with the identical `HOME`/`XDG_RUNTIME_DIR` env so
 * `resolve_socket_path` (the CLI's own connection setup) finds the same
 * UDS socket this specific daemon instance is listening on, not some other
 * (or no) daemon.
 */
export async function runCli(daemon: DaemonHandle, args: string[]): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(resolveBinary(), args, {
      env: {
        ...process.env,
        HOME: daemon.home,
        XDG_RUNTIME_DIR: daemon.home,
        XDG_DATA_HOME: daemon.home,
        XDG_CONFIG_HOME: daemon.home,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    child.stdout?.on("data", (chunk: Buffer) => stdout.push(chunk));
    child.stderr?.on("data", (chunk: Buffer) => stderr.push(chunk));
    child.once("error", reject);
    child.once("exit", (code) => {
      resolve({
        code,
        stdout: Buffer.concat(stdout).toString("utf8"),
        stderr: Buffer.concat(stderr).toString("utf8"),
      });
    });
  });
}
