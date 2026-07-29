# serialwrap

A serial port broker for firmware development — one daemon owns the port, everyone else is a client.

Opening `screen` means you can't flash. Flashing means you miss the boot log. Letting an AI agent watch the serial output means fighting it for the same file descriptor. serialwrap separates *ownership* of the port from *use* of the data: a daemon holds the device open and continuously records, while humans, agents, and flashing tools all attach as clients.

## What it does

- **Never miss a boot log.** Recording starts the moment the device enumerates, not when you open a terminal.
- **Share the port.** A web GUI, a CLI tail, an MCP-connected agent, and `esptool` can all coexist. Flashing takes a temporary lease; the gap is recorded as an explicit event rather than a silent hole.
- **Give agents the right primitives.** `wait_for(pattern, timeout)`, `read_since(cursor)`, and filtered `tail` instead of a blocking `cat` that blows up a context window.
- **Gate writes.** Whitelisted commands pass; unknown ones wait for human approval; destructive patterns (`erase`, fuse writes, bootloader entry) always require it. Every byte written is attributable.
- **Export losslessly.** JSONL for round-tripping, plain text for reading, raw bytes for protocol decoders.

Devices are identified by USB serial number, so port settings follow the board across replugs and `ttyUSB0 → ttyUSB1` renumbering.

## Status

M0–M6 complete: workspace/CI, the mock-device test fixture, device detection and hotplug, the recording engine, port I/O and configuration, the UDS client protocol, the full CLI, the MCP bridge (read tools plus a gated write path), the write gate and approval flow, the embedded web GUI, and packaging (this document, plus `packaging/`).

**No tagged release exists yet** — `packaging/homebrew/serialwrap.rb` and `packaging/linux/install.sh` are ready for the first one (see [Installing](#installing) below for exactly what that means for you today: building from source, which is what this README's Quickstart walks through and what `packaging/linux/install.sh` itself falls back to when it can't find a published release).

Design docs: see the [wiki](https://github.com/SheldonChangL/serialwrap/wiki). Full task history: [TASKS.md](TASKS.md).

Target platforms: macOS and Linux, shipped as a single Rust binary.

## Quickstart

Fifteen minutes, clean machine to a log on screen — that's the bar this project holds itself to (see `docs/manual-checklist.md` for how that's verified on a genuinely clean VM; a Docker-based approximation for Linux is in this repo's PR history).

### 1. Prerequisites

**macOS:**

```sh
xcode-select --install                    # cc/linker, if you don't already have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
# Node.js 22+ — via nvm, or https://nodejs.org
```

**Linux (Debian/Ubuntu):**

```sh
sudo apt-get update && sudo apt-get install -y build-essential pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust
# Node.js 22+ — via https://github.com/nodesource/distributions or nvm
```

(Other distros: the package names differ, but you need a C compiler/linker, `pkg-config`, `git`, `curl`, Rust via `rustup`, and Node.js 22+.)

### 2. Build

```sh
git clone https://github.com/SheldonChangL/serialwrap.git
cd serialwrap
(cd webui && npm ci && npm run build)     # the web GUI — see the warning below
cargo build --release -p serialwrap
```

**Do not skip the `npm run build` step.** `crates/serialwrapd/build.rs` writes a placeholder page into `webui/dist/` if it's missing, specifically so a Rust-only checkout can still `cargo build` at all — but a release binary built that way embeds a placeholder GUI, not the real one. Every `cargo build`/`cargo build --release` prints a `cargo:warning` for as long as that placeholder is what's actually embedded; if you see it, go back and run the `npm` step. (The project's own CI and release workflow always build the frontend first — see `.github/workflows/ci.yml` and `.github/workflows/release.yml` — so this only bites a manual, Rust-only local build.)

Once a tagged release exists, `packaging/linux/install.sh` and the Homebrew formula in `packaging/homebrew/` will fetch a prebuilt binary instead of building from source — see `packaging/homebrew/serialwrap.rb`'s header comment and `docs/manual-checklist.md` for their current status.

### 3. Install the binary somewhere on your `PATH`, then start the daemon

```sh
cp target/release/serialwrap ~/.local/bin/   # or anywhere already on your PATH
```

Either register it as a background service that starts at login:

```sh
serialwrap service install
```

...which writes and loads a `launchd` user agent on macOS (`~/Library/LaunchAgents/com.serialwrap.daemon.plist`) or a `systemctl --user` unit on Linux (`~/.config/systemd/user/com.serialwrap.daemon.service`). Pass `--dry-run` to preview the generated file without writing or registering anything. `serialwrap service uninstall` reverses it.

Or just run it in the foreground to watch it start:

```sh
./target/release/serialwrap daemon
```

### 4. See a log

Open `http://127.0.0.1:5590` in a browser — this works the moment the daemon is running, with no separate frontend server. Pick a port from the selector in the top-left (the choice lives in the URL as `?device=<id>`, so a second board is just a second tab), watch the log fill the window, and type into the bar at the bottom to send bytes back. Or from the CLI:

```sh
serialwrap devices          # nothing plugged in yet? says so, rather than an empty silence
serialwrap tail -f          # follow the log for the one device, once something's plugged in
```

Plug in a USB-serial adapter or dev board. Recording starts the instant the device enumerates — before any client, including this one, ever connects — so the boot banner is already in the log the first time you look, not lost to the race of "reopen the terminal after flashing".

**No hardware yet?** `cargo test --all` exercises the exact same recording/query pipeline end-to-end against a simulated PTY device (`crates/mock-device`), which is the fastest way to confirm your build actually works before hardware is in hand.

## Linux: permissions

Serial devices are owned by the `dialout` group on most distributions. Add yourself to it once:

```sh
sudo usermod -aG dialout "$USER"
# then log out and back in — group membership doesn't apply to your current session
```

If `serialwrap devices` still can't see your adapter after that (some minimal or hardened setups lack the distro's generic USB-serial udev rule), install the template in this repo:

```sh
sudo cp packaging/linux/60-serialwrap.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Reconnect the device after either step.

## macOS: common USB-serial drivers

- **CH340/CH341** (common on cheap ESP32/Arduino clones): install [WCH's official driver](https://www.wch-ic.com/downloads/CH341SER_MAC_ZIP.html), then reboot. macOS will likely warn about an unsigned kernel extension the first time — approve it in **System Settings → Privacy & Security**.
- **CP2102/CP210x** (Silicon Labs): install the [CP210x VCP driver](https://www.silabs.com/developer-tools/usb-to-uart-bridge-vcp-drivers), same approval step as above.
- **FTDI** (FT232/FT230X): recent macOS versions include a native driver (`AppleUSBFTDI`) for many FTDI chips; if `serialwrap devices` doesn't see the board, install [FTDI's own VCP driver](https://ftdichip.com/drivers/vcp-drivers/) instead.

After installing a driver, re-plug the device and confirm with `serialwrap devices`. Always use the `/dev/cu.*` node, never `/dev/tty.*` — serialwrap does this for you, but if you're ever comparing against a manual `screen`/`minicom` session, `tty.*` blocks waiting for a carrier signal that a USB-serial adapter typically never raises.

## Flashing (lease mode)

`serialwrap run` hands the port to an external tool for the duration of one command, then reclaims it:

```sh
serialwrap run -- esptool.py --port "$SERIALWRAP_LEASE_PATH" write_flash 0x0 firmware.bin
```

`serialwrap run` doesn't rewrite the flashing tool's own arguments — it sets `SERIALWRAP_LEASE_PATH` in the child's environment to whatever device path the daemon just released, so your own command line picks it up explicitly (as above), or you can hardcode a known `/dev/cu.*`/`/dev/ttyUSB*` path instead. The gap is recorded as a `lease_start`/`lease_end` event, and any other client's `tail -f` sees that event rather than a disconnect — recording resumes automatically the moment the command exits, so the post-flash boot log is never lost either.

## Connecting an AI agent (MCP)

```sh
claude mcp add serialwrap -- serialwrap mcp
```

This registers `serialwrap mcp` as a stdio MCP server with Claude Code. It bridges to the same daemon over the same Unix domain socket every other client uses, registering as `client_type: agent` — which is what puts its writes through the gate described below. Tools exposed: `list_devices`, `get_config`, `tail`, `read_since`, `wait_for`, `write`, `set_config`, `dtr_pulse`, `export`. See the [Client protocol wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol#mcp-tool-surface) for each tool's exact shape.

Every read tool's result is data about the device, not instructions for the agent — see the next section.

## Security model

serialwrap's threat model is narrow (a localhost developer tool, not a multi-tenant service) but organized around one property: **a write to a serial port can be physically irreversible.** An erased bootloader or a blown one-time-programmable fuse doesn't come back the way a deleted file does.

**The write gate has three branches**, evaluated in this priority order:

```
danger pattern?  ──yes──▶  force approval (cannot be whitelisted away)
       │no
whitelist match? ──yes──▶  allow immediately
       │no
                          pending approval (default 60s → deny)
```

- **Danger always wins.** `erase`, `fuse`/`otp`/`efuse`, `unlock`/`lock`, bootloader-entry sequences, and `format`/`factory_reset` (see `docs/rules.toml.example` for the full built-in list with each pattern's stated reason) force human approval even if the same bytes also match a whitelist entry. The only way to change what counts as dangerous is hand-editing `rules.toml` — never a checkbox on an approval card in the moment.
- **Timeout means deny, fail-safe.** An unattended pending request is denied, never silently allowed, after the configured timeout (60s by default).
- **Humans bypass the gate; agents don't.** A `human` client's writes go straight through — gating the operator would only teach them to disable the gate — but every write, from any client type, is fully audited regardless. This covers the GUI's own write bar (`POST /api/devices/:id/write`), which sends as `human` and appends the same `tx` record `serialwrap write` does; an agent reaching the same daemon over MCP still waits for a human.

**Log content is data, never an instruction** — for a human reading the GUI and for an agent reading over MCP alike. Firmware logs routinely contain human-readable strings a developer wrote for other humans (`// TODO: reflash with production key before shipping`), and can relay content verbatim from external peers (sensors, BLE, network links) that serialwrap has no way to vouch for. Every MCP read tool's description says this explicitly, and the GUI renders device data (`kind: rx`) and broker-generated events in visually distinct styles so the boundary is never just a convention — see the [Security-model wiki](https://github.com/SheldonChangL/serialwrap/wiki/Security-model) for the full reasoning.

**Audit is a query over the one event stream, not a second store.** Every write, gate decision, config change, lease, and client (dis)connection lives in the exact same append-only stream as the device's own log data, so "what was the board doing right when this was approved" is a query (`serialwrap audit --context <seq>`), never a correlation exercise across separate logs. `serialwrap approvals` / `serialwrap audit` are the CLI surface; the GUI's approval card and audit panel call the same daemon API.

The daemon binds its web GUI to `127.0.0.1` only — remote access is `ssh -L 5590:localhost:5590 <host>`, not a network-exposed listener; there is no authentication layer or TLS in v1 (see the wiki for why that's a deliberate, stated limitation rather than an oversight).

## Timestamp precision

Every record carries a monotonic clock reading (`t_mono`) and a wall-clock timestamp (`t_wall`), both applied **at the moment the daemon reads bytes from the host-side serial port** — not when the device actually emitted them. This is host-side arrival time, and it is honestly labeled as such rather than presented as more precise than it is.

**USB buffering distorts it.** A USB-serial adapter's own firmware batches bytes before handing them to the host; an FTDI chip's *latency timer* defaults to **16ms**, meaning up to 16ms of device output can be coalesced into what the daemon sees as a single, later read. Two lines the device actually emitted 1ms apart can show up in `serialwrap tail`/the GUI as arriving together, or with a gap that reflects USB scheduling rather than firmware timing. This limitation is structural — no daemon-side change fixes it — so serialwrap does not claim sub-USB-buffering timing accuracy anywhere in its output.

**If you're debugging something timing-sensitive** (an ISR latency question, a race between two log lines), on Linux you can lower an FTDI device's latency timer:

```sh
# find the right device first (replace ttyUSB0 with yours):
cat /sys/bus/usb-serial/devices/ttyUSB0/latency_timer   # current value, ms
echo 1 | sudo tee /sys/bus/usb-serial/devices/ttyUSB0/latency_timer   # 1ms minimum
```

This trades USB bus overhead (more, smaller transfers) for lower coalescing latency, and reverts on replug/reboot — there's no persistent equivalent shipped with serialwrap, since it's a per-device, per-session tradeoff you make deliberately when timing precision actually matters for the task at hand, not a default worth silently changing system-wide. CH340/CP210x-family chips don't expose an equivalent tunable the same way; their internal buffering is comparable in effect but isn't user-adjustable through sysfs the way FTDI's is.

## Documentation

- [Wiki](https://github.com/SheldonChangL/serialwrap/wiki) — architecture, event stream/storage schema, client protocol, security model, UX design.
- [TASKS.md](TASKS.md) — the full task-by-task breakdown and acceptance criteria this project was built against.
- [docs/manual-checklist.md](docs/manual-checklist.md) — the hardware-dependent acceptance items that can't be verified by CI (real baud timing, real DTR/RTS electrical behavior, a genuinely clean install VM), tracked separately with who verified what and when.
- `packaging/` — the Homebrew formula, the Linux install script, the udev rule template, and their own README-level comments on current status.

## License

MIT
