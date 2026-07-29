/**
 * Naming and ordering for the device picker.
 *
 * # Why a default other than "the first one the API returned"
 *
 * The GUI used to show `devices[0]` with no way to change it. On macOS that
 * is reliably `/dev/cu.debug-console` — a built-in node that never emits a
 * byte — so the first thing an operator saw was a connected-looking device
 * with an empty log, which reads as "this tool is broken" rather than
 * "you're looking at the wrong port". Every device is still listed and
 * selectable; only the *initial guess* is ranked, and the top bar always
 * names what it picked.
 *
 * The ranking is a heuristic about naming conventions, so it is deliberately
 * soft: it never hides anything, and a device it guesses wrong about is one
 * click from being right.
 */
import type { DeviceSummary } from "./api";

/** Path fragments that mean "a USB-serial adapter someone plugged in" across
 * the two platforms this project targets: macOS calls them `cu.usbserial-*`
 * (FTDI/CH340/CP210x) and `cu.usbmodem*` (CDC-ACM, e.g. a native-USB MCU);
 * Linux uses `ttyUSB*` and `ttyACM*` for the same two families. */
const USB_HINTS = ["usbserial", "usbmodem", "ttyusb", "ttyacm"];

/** Nodes that exist on a stock machine whether or not any hardware is
 * attached. Never hidden — just never the opening guess. */
const BUILTIN_HINTS = ["debug-console", "bluetooth", "wlan-debug"];

function pathOf(d: DeviceSummary): string {
  return (d.path ?? "").toLowerCase();
}

/** Lower sorts first. */
export function deviceRank(d: DeviceSummary): number {
  const p = pathOf(d);
  const builtin = BUILTIN_HINTS.some((h) => p.includes(h));
  const usb = USB_HINTS.some((h) => p.includes(h));
  if (d.connected && usb) return 0;
  if (d.connected && !builtin) return 1;
  if (usb) return 2;
  if (d.connected) return 3;
  return 4;
}

export function sortDevices(devices: DeviceSummary[]): DeviceSummary[] {
  return [...devices].sort(
    (a, b) => deviceRank(a) - deviceRank(b) || (a.path ?? a.id).localeCompare(b.path ?? b.id),
  );
}

/** The name a person uses for this port out loud. `/dev/cu.usbserial-1240`
 * is "usbserial-1240" — the part that distinguishes it from the other four
 * entries in the list. The full path and the full daemon id both stay
 * visible in the picker, so this shortening never costs identification. */
export function deviceLabel(d: DeviceSummary): string {
  const path = d.path;
  if (!path) return d.id;
  const base = path.replace(/^\/dev\//, "");
  return base.replace(/^cu\./, "").replace(/^tty\./, "");
}

/** Which device to open when the URL doesn't say. */
export function pickDefaultDevice(devices: DeviceSummary[]): string | null {
  return sortDevices(devices)[0]?.id ?? null;
}
