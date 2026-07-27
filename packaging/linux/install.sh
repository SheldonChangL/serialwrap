#!/usr/bin/env sh
# serialwrap install script (TASKS.md T6.1, issue #23).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/SheldonChangL/serialwrap/main/packaging/linux/install.sh | sh
#
# What it does, in order:
#   1. Try to fetch the latest tagged GitHub release's prebuilt
#      x86_64-unknown-linux-gnu tarball and install the binary from it.
#   2. If no release exists yet (this project's current state — see
#      README.md) or the download fails for any reason, fall back to
#      building from source: clone the repo into a temp dir, build the web
#      frontend, then `cargo build --release`, then install that binary.
#      This fallback needs `git`, `cargo`, `node`+`npm` already on PATH —
#      see README.md's Linux prerequisites section for the one-line install
#      commands; this script deliberately does not install a whole Rust/
#      Node toolchain on your behalf, only serialwrap itself.
#   3. Print (not silently run) the two permission-setup steps every
#      Linux install needs: dialout group membership and the udev rule
#      template, since both require sudo and this script must not assume
#      it's safe to grant unattended sudo access to itself.
#
# Everything installs under $SERIALWRAP_PREFIX (default: $HOME/.local), no
# sudo required for the binary itself — only the udev rule / dialout group
# step (printed, not run) touches system state.
set -eu

REPO="SheldonChangL/serialwrap"
PREFIX="${SERIALWRAP_PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
TARGET_TRIPLE="x86_64-unknown-linux-gnu"

log() { printf 'serialwrap-install: %s\n' "$*" >&2; }
die() {
    log "$*"
    exit 1
}

if [ "$(uname -s)" != "Linux" ]; then
    die "this script is for Linux; on macOS use Homebrew (see packaging/homebrew/README.md) or build from source"
fi

if [ "$(uname -m)" != "x86_64" ]; then
    log "no prebuilt release for $(uname -m) yet (only $TARGET_TRIPLE is published) - going straight to a source build"
    NEED_SOURCE_BUILD=1
else
    NEED_SOURCE_BUILD=0
fi

mkdir -p "$BIN_DIR"

try_prebuilt_release() {
    command -v curl >/dev/null 2>&1 || {
        log "curl not found, skipping prebuilt-release lookup"
        return 1
    }
    api_url="https://api.github.com/repos/$REPO/releases/latest"
    tag=$(curl -fsSL "$api_url" 2>/dev/null | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/') || true
    if [ -z "${tag:-}" ]; then
        log "no published GitHub release found (or GitHub API unreachable)"
        return 1
    fi
    asset="serialwrap-${tag}-${TARGET_TRIPLE}.tar.gz"
    url="https://github.com/$REPO/releases/download/${tag}/${asset}"
    log "found release ${tag}; downloading ${asset}"
    tmp=$(mktemp -d)
    if ! curl -fsSL "$url" -o "$tmp/$asset"; then
        log "download failed (asset may not exist for this release yet)"
        rm -rf "$tmp"
        return 1
    fi
    tar -xzf "$tmp/$asset" -C "$tmp"
    install -m 0755 "$tmp/serialwrap" "$BIN_DIR/serialwrap"
    rm -rf "$tmp"
    log "installed prebuilt $tag binary to $BIN_DIR/serialwrap"
    return 0
}

build_from_source() {
    for tool in git cargo node npm; do
        command -v "$tool" >/dev/null 2>&1 || die "'$tool' not found on PATH - see README.md's Linux prerequisites (Rust via rustup, Node.js 22+) before re-running this script"
    done
    src=$(mktemp -d)
    log "building from source in $src (this is the slow path: expect a few minutes for the first cargo build)"
    git clone --depth 1 "https://github.com/$REPO.git" "$src/serialwrap"
    (
        cd "$src/serialwrap/webui"
        npm ci
        npm run build
    )
    (
        cd "$src/serialwrap"
        cargo build --release -p serialwrap
    )
    install -m 0755 "$src/serialwrap/target/release/serialwrap" "$BIN_DIR/serialwrap"
    rm -rf "$src"
    log "built and installed to $BIN_DIR/serialwrap"
}

if [ "$NEED_SOURCE_BUILD" = "1" ] || ! try_prebuilt_release; then
    build_from_source
fi

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) log "note: $BIN_DIR is not on your PATH - add 'export PATH=\"$BIN_DIR:\$PATH\"' to your shell rc file" ;;
esac

cat >&2 <<EOF
serialwrap-install: binary installed. Two permission steps this script does
NOT run for you (both need sudo, and a piped install script should not
silently escalate privileges):

  1. Add yourself to the 'dialout' group, then log out and back in:
       sudo usermod -aG dialout "\$USER"

  2. (optional, only if 'serialwrap devices' can't see your adapter after
     step 1) install the udev rule template:
       sudo cp packaging/linux/60-serialwrap.rules /etc/udev/rules.d/
       sudo udevadm control --reload-rules && sudo udevadm trigger

Then run '$BIN_DIR/serialwrap service install' to start the daemon at
login, or 'serialwrap daemon' directly to try it now.
EOF
