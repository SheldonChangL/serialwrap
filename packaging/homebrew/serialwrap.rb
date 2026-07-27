# Homebrew formula for serialwrap (TASKS.md T6.1, issue #23).
#
# Status: no tagged release exists yet (see README.md's "Status" section),
# so `url`/`sha256` below are placeholders rather than a real, resolvable
# release tarball - a real release workflow run (`.github/workflows/
# release.yml`, triggered by pushing a `v*.*.*` tag) publishes the actual
# release archive and its checksum; fill those in (or run
# `brew bump-formula-pr`, once this formula lives in a
# `homebrew-serialwrap` tap repo) at that point. `head` works today with no
# release needed - see below.
#
# Tap setup (until/unless this graduates to homebrew-core):
#   brew tap-new SheldonChangL/serialwrap   # or create a `homebrew-serialwrap` repo
#   cp packaging/homebrew/serialwrap.rb $(brew --repo SheldonChangL/serialwrap)/Formula/
#   brew install --HEAD serialwrap
#
# Building from source (not a prebuilt bottle) is deliberate: this formula
# embeds the web frontend into the binary at build time (`npm ci && npm run
# build` in `webui/`, consumed by `crates/serialwrapd/build.rs` - see that
# file's own doc comment on why skipping this step silently produces a
# binary whose GUI is a placeholder page), and cross-building the combined
# Rust+embedded-JS artifact as a portable bottle is more moving parts than
# this task's scope covers.
class Serialwrap < Formula
  desc "Serial port broker for firmware development"
  homepage "https://github.com/SheldonChangL/serialwrap"
  # TODO(first tagged release): point these at the real tag/tarball, e.g.
  #   url "https://github.com/SheldonChangL/serialwrap/archive/refs/tags/v0.1.0.tar.gz"
  #   sha256 "<sha256 of that tarball>"
  url "https://github.com/SheldonChangL/serialwrap/archive/refs/tags/v0.1.0.tar.gz"
  sha256 ""
  license "MIT"
  head "https://github.com/SheldonChangL/serialwrap.git", branch: "main"

  depends_on "node" => :build
  depends_on "rust" => :build

  def install
    system "npm", "ci", "--prefix", "webui"
    system "npm", "run", "build", "--prefix", "webui"
    system "cargo", "install", *std_cargo_args(path: "crates/serialwrap")
  end

  def caveats
    <<~EOS
      Start the daemon at login:
        serialwrap service install

      Or run it in the foreground first, to watch it start up:
        serialwrap daemon

      Common USB-serial drivers (CH340/CP210x) and the Linux dialout/udev
      setup are documented in README.md.
    EOS
  end

  test do
    assert_match "serialwrap", shell_output("#{bin}/serialwrap --version")
  end
end
