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

Early development. Design and task breakdown are in [TASKS.md](TASKS.md).

Planned: macOS and Linux, single static binary, Rust.

## License

MIT
