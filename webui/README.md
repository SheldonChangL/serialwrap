# webui

The serialwrap web GUI's frontend. Built with **TypeScript + Svelte 5 +
Vite**, embedded into the single `serialwrap` binary via `rust-embed` (see
`crates/serialwrapd/src/web/`) — there is no separate frontend server or
`node_modules` dependency at runtime. `serialwrap daemon` alone is enough
for a browser at `http://127.0.0.1:5590` to work (see the [Client-protocol
wiki](https://github.com/SheldonChangL/serialwrap/wiki/Client-protocol)).

This is `TASKS.md` T5.1 (issue #18): the foundation only — WebSocket
connectivity, an honest connection-status indicator, and one live API call
(`GET /api/devices`). The log view, timeline, approval cards, and
clients/audit/export panels are T5.2-T5.5.

## Why Svelte (not Preact)

TASKS.md left this open ("Svelte 或 Preact"). Svelte was chosen because:

- **T5.2's constraint is the deciding one**: a virtual-scrolled log view
  that has to stay at ≥30fps while 5,000 lines/sec arrive. Svelte compiles
  each component into fine-grained imperative DOM updates with no virtual
  DOM to diff; Preact still reconciles a vdom per update, which is more
  diffing work per incoming line at that rate. Either framework can be
  made fast with a hand-rolled virtual-scroll windowing scheme (bounded
  DOM node count regardless of buffer size), but Svelte's update model
  needs less fighting against the framework to get there.
- **Bundle size** (this task's build, gzip): ~15.8 KB JS + ~1 KB CSS —
  Svelte ships no runtime framework code, only the compiled output plus a
  tiny reactivity helper library. This matters because it's embedded into
  the release binary.
- **Build/CI cost**: Vite + `@sveltejs/vite-plugin-svelte` builds this
  project in well under a second; `svelte-check` type-checks `.svelte`
  files the same way `tsc` does `.ts`. Neither adds meaningful time next
  to the Rust build, and both run in their own CI job/step (see below),
  never inside `cargo test`.

## Layout

```
webui/
├── src/            frontend source (Svelte + TS)
│   └── lib/        connection.ts (WS client), api.ts (GET /api/*), components
├── e2e/            Playwright E2E — drives the real compiled daemon binary
├── dist/           build output (gitignored) — what rust-embed embeds
└── index.html, vite.config.ts, svelte.config.js, tsconfig*.json
```

## Building

```sh
npm ci
npm run build        # -> dist/, embedded by `cargo build` (see crates/serialwrapd/build.rs)
```

`crates/serialwrapd/build.rs` writes a minimal placeholder into `dist/` if
it doesn't exist yet, so a Rust-only checkout can still `cargo build`/
`cargo test` without Node installed — but the real UI needs an actual
`npm run build` first. CI always runs the frontend build before any cargo
step (see `.github/workflows/ci.yml`'s `test` job).

## Development

```sh
npm run dev           # Vite dev server with HMR, proxies /api -> http://127.0.0.1:5590
npm run check          # svelte-check + tsc (frontend and e2e/ sources)
npm run lint           # eslint
```

Run a `serialwrap daemon` separately (`SERIALWRAP_WEB_PORT` defaults to
`5590`) for `npm run dev`'s proxy to have something to talk to.

## E2E (Playwright)

```sh
npx playwright install --with-deps chromium   # once
npm run build && cargo build --release -p serialwrap    # from repo root
npm run e2e
```

`e2e/daemon.ts` spawns the actual compiled `serialwrap daemon` binary
(default: `target/release/serialwrap`; override with `SERIALWRAP_BIN` or
`SERIALWRAP_PROFILE=debug`) on a throwaway `HOME`/port per test, so it
never touches a real user's `~/.serialwrap`. This suite is intentionally
**not** part of `cargo test --all` — it runs in its own `e2e` CI job (see
`.github/workflows/ci.yml`) so a browser-driven suite never risks that
budget. `workers: 1` in `e2e/playwright.config.ts` is a deliberate
simplification: tests bind real TCP ports and kill/restart real
subprocesses, and this suite is small enough that serializing them is
simpler than parallel-safe port allocation.

Every wait in the E2E specs is for an actual observable condition (a DOM
attribute Playwright polls, or a real HTTP response) — never a fixed
`waitForTimeout` — per the timing-stability lesson from issue #39.

## Default port

The daemon's web GUI listens on `127.0.0.1:5590` by default
(`serialwrapd::web::DEFAULT_PORT`), overridable via `SERIALWRAP_WEB_PORT`
(mainly so tests can run several daemons at once). The bind address itself
is never configurable — always `127.0.0.1`; remote access is `ssh -L
5590:localhost:5590 <host>`, not a network-exposed listener.
