# webui

Placeholder for the serialwrap web GUI's frontend assets.

Nothing here is built or served yet. `index.html` is a static placeholder so
the directory isn't empty in git. There is no build step in this repo yet —
that, plus the actual GUI (live log view, timeline, approval cards, clients
panel, export dialog), lands starting with T5.1 in `TASKS.md` (M5 Web GUI).

Once real assets exist, `axum` will serve the API/WebSocket endpoints and
`rust-embed` will embed the built frontend into the single `serialwrap`
binary — no separate frontend server, no runtime file dependencies.
