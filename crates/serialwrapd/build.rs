//! Ensures `webui/dist` exists before `rust-embed`'s derive macro tries to
//! enumerate it (`TASKS.md` T5.1, issue #18: `web::assets::Assets`). A
//! fresh clone that hasn't run `npm run build` in `webui/` yet would
//! otherwise fail to compile this crate at all — this writes a minimal
//! placeholder page instead, so plain `cargo build`/`cargo test` never
//! *requires* Node/npm. A real UI still needs an actual frontend build
//! (see `webui/README.md`); CI always runs that first (see
//! `.github/workflows/ci.yml`), so this fallback only matters for a
//! Rust-only local workflow.
//!
//! Every build emits a `cargo:warning` for as long as the placeholder is
//! what's actually embedded — not just the first time it's written. A
//! review finding on PR #43 (#9) pointed out the original version only
//! warned on the write itself: every later `cargo build` silently kept
//! embedding the placeholder with no signal at all, which is exactly how
//! a `cargo build --release` on a Node-less machine could produce a
//! binary whose GUI is a placeholder page, indistinguishable from a real
//! build in the build output.
//!
//! Also tells cargo to rebuild whenever any file under `webui/dist`
//! changes — `rerun-if-changed` on the directory alone only reacts to
//! entries being added/removed on some platforms, not to an existing
//! file's content changing (e.g. `npm run build` overwriting
//! `assets/index-XXXX.js` in place), so every file is watched
//! individually.

use std::path::Path;

/// Present in `write_placeholder`'s output and nowhere a real Vite build
/// would produce it — used to tell "this is our placeholder" apart from
/// "this is a real frontend build" on every run, not just the run that
/// wrote it.
const PLACEHOLDER_MARKER: &str = "serialwrap-build-rs-placeholder";

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../webui/dist");
    let index = dist.join("index.html");

    let is_placeholder = match std::fs::read_to_string(&index) {
        Ok(contents) => contents.contains(PLACEHOLDER_MARKER),
        Err(_) => true, // doesn't exist yet, so it's about to become one
    };

    if is_placeholder {
        std::fs::create_dir_all(&dist).expect("create webui/dist placeholder directory");
        std::fs::write(&index, placeholder_html())
            .expect("write webui/dist placeholder index.html");
        println!(
            "cargo:warning=serialwrapd: webui/dist/index.html is build.rs's placeholder, not a \
             real frontend build — the embedded web GUI will show only a placeholder page. Run \
             `npm ci && npm run build` in webui/, then rebuild this crate, to embed the real UI."
        );
    }

    watch_recursively(&dist);
}

fn placeholder_html() -> String {
    format!(
        "<!doctype html>\n<!-- {PLACEHOLDER_MARKER} -->\n<html><head><title>serialwrap</title></head><body>\n\
         <p>serialwrap web GUI placeholder — no frontend build found. Run \
         <code>npm ci &amp;&amp; npm run build</code> in <code>webui/</code> to build \
         the real one, then rebuild this crate.</p>\n</body></html>\n"
    )
}

fn watch_recursively(dir: &Path) {
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            watch_recursively(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
