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
//! Also tells cargo to rebuild whenever any file under `webui/dist`
//! changes — `rerun-if-changed` on the directory alone only reacts to
//! entries being added/removed on some platforms, not to an existing
//! file's content changing (e.g. `npm run build` overwriting
//! `assets/index-XXXX.js` in place), so every file is watched
//! individually.

use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../webui/dist");

    if dist.join("index.html").exists() {
        watch_recursively(&dist);
        return;
    }

    std::fs::create_dir_all(&dist).expect("create webui/dist placeholder directory");
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html>\n<html><head><title>serialwrap</title></head><body>\n\
         <p>serialwrap web GUI placeholder — no frontend build found. Run \
         <code>npm ci &amp;&amp; npm run build</code> in <code>webui/</code> to build \
         the real one, then rebuild this crate.</p>\n</body></html>\n",
    )
    .expect("write webui/dist placeholder index.html");
    // The placeholder itself now exists at `dist/index.html`, so the next
    // build sees the early-return branch above; still watch the directory
    // so a later real `npm run build` is picked up.
    watch_recursively(&dist);
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
