// Spawns/kills the real, compiled `serialwrap daemon` binary for E2E tests
// (`TASKS.md` T5.1, issue #18). Deliberately drives the actual production
// entry point (not an in-process test double) — this suite exists
// specifically to prove the "open a browser, no separate frontend service"
// acceptance criterion end to end, including a real daemon restart for the
// WS reconnect criterion.
import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
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
  stop(): Promise<void>;
}

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
export async function startDaemon(port?: number): Promise<DaemonHandle> {
  const usePort = port ?? nextPort++;
  const home = mkdtempSync(path.join(tmpdir(), "serialwrap-e2e-"));

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
        async stop() {
          stopper.kill("SIGKILL");
          await new Promise<void>((resolve) => stopper.once("exit", () => resolve()));
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
