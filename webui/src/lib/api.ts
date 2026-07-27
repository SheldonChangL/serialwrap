/**
 * Thin `GET /api/*` client (`TASKS.md` T5.1, issue #18) — proves the HTTP
 * side of the foundation works end to end. Deliberately minimal: this
 * task's job is the plumbing, not the device/log views (T5.2+).
 */
export interface DeviceSummary {
  id: string;
  path: string | null;
  connected: boolean;
  config: unknown;
}

export async function fetchDevices(): Promise<DeviceSummary[]> {
  const res = await fetch("/api/devices");
  if (!res.ok) {
    throw new Error(`GET /api/devices failed: ${res.status} ${res.statusText}`);
  }
  const body = (await res.json()) as { devices: DeviceSummary[] };
  return body.devices;
}
