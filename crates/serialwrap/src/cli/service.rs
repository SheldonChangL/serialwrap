//! `serialwrap service install` / `serialwrap service uninstall` (`TASKS.md`
//! T6.1, issue #23): write and (un)register the platform-native user service
//! unit that runs `serialwrap daemon` in the background, so recording starts
//! at login rather than only while someone happens to have run `serialwrap
//! daemon` by hand in a terminal — the same "no client involvement required"
//! promise the [Architecture
//! wiki](https://github.com/SheldonChangL/serialwrap/wiki/Architecture#process-and-lifecycle-model)
//! describes ("The daemon runs as a user-level service ... and starts at
//! login").
//!
//! # Why a user-level service, not a system one
//!
//! The daemon opens the serial device as the invoking user (`dialout` group
//! membership on Linux, a vendor driver on macOS — see the Security-model
//! wiki's "Local privilege" section) and binds its web GUI to
//! `127.0.0.1` for that same user's browser session. A system-level
//! service (root launchd daemon / systemd system unit) would need to either
//! run as root — a strictly larger privilege than this project ever wants —
//! or juggle per-user socket permissions for no benefit. macOS: a
//! `~/Library/LaunchAgents` user agent. Linux: a `systemctl --user` unit.
//!
//! # `--dry-run`
//!
//! Every subcommand takes `--dry-run`: prints the exact file that would be
//! written (path plus content) to stdout and exits, touching neither disk
//! nor the platform's service manager. This is what lets both this crate's
//! own tests and a cautious operator inspect the generated unit before it
//! is ever installed — see [`run`] for the one place the split between
//! "pure content generation" (unit-tested directly, no filesystem or
//! subprocess involved) and "actually install it" happens.
//!
//! # Idempotency
//!
//! `install` overwrites any existing file at the target path (re-running it
//! after a binary move is how you update the recorded `ProgramArguments`/
//! `ExecStart` path). `uninstall` is a no-op, not an error, when nothing is
//! installed — removing something that was never there is success, not
//! failure, matching `rm -f`'s convention rather than `rm`'s.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Subcommand;

/// launchd label / systemd unit name, shared by both platforms' generated
/// content and both platforms' file names (`<LABEL>.plist` /
/// `<LABEL>.service`) — one constant so the two can never drift apart.
const LABEL: &str = "com.serialwrap.daemon";

#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Write the platform service unit for `serialwrap daemon` and register
    /// it (`launchctl load -w` on macOS, `systemctl --user enable --now` on
    /// Linux) so it starts now and again at every future login.
    Install {
        /// Print the unit file path and content to stdout; write nothing
        /// and invoke no service manager.
        #[arg(long)]
        dry_run: bool,
    },
    /// Unregister the service and remove its unit file. Safe to run when
    /// nothing is installed (a no-op, not an error).
    Uninstall {
        /// Print what would be removed; touch nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args, Debug)]
pub struct ServiceArgs {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

pub async fn run(args: ServiceArgs) -> io::Result<()> {
    let binary_path = current_exe()?;
    let home = directories::BaseDirs::new().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not resolve a home directory to install a service unit into",
        )
    })?;

    match args.command {
        ServiceCommand::Install { dry_run } => install(&binary_path, &home, dry_run),
        ServiceCommand::Uninstall { dry_run } => uninstall(&home, dry_run),
    }
}

/// The absolute, symlink-resolved path to this running binary — what goes
/// into `ProgramArguments`/`ExecStart`, so the generated unit keeps working
/// regardless of *how* it was invoked (a relative `./target/release/
/// serialwrap`, a `PATH` lookup, a Homebrew Cellar symlink) and independent
/// of the current working directory, which a launchd/systemd-started
/// process does not inherit from whoever ran `install`.
fn current_exe() -> io::Result<PathBuf> {
    std::env::current_exe()?.canonicalize()
}

#[cfg(target_os = "macos")]
fn install(binary_path: &Path, home: &directories::BaseDirs, dry_run: bool) -> io::Result<()> {
    let path = macos_plist_path(home.home_dir());
    let content = launchd_plist(binary_path, &macos_log_dir(home.home_dir()));
    if dry_run {
        print_dry_run(&path, &content);
        return Ok(());
    }
    write_unit_file(&path, &content)?;
    // `load -w` (rather than the newer `bootstrap gui/<uid>`) is
    // deliberate: it needs no uid lookup, is understood by every macOS
    // version this project targets, and remains fully functional despite
    // being the "legacy" spelling — `bootstrap`/`bootout` is the more
    // modern replacement should this ever need per-uid domain targeting.
    run_service_manager("launchctl", &["load", "-w", &path.to_string_lossy()])?;
    println!(
        "serialwrap: installed and started launchd user agent at {}",
        path.display()
    );
    println!(
        "serialwrap: logs: {}",
        macos_log_dir(home.home_dir()).display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall(home: &directories::BaseDirs, dry_run: bool) -> io::Result<()> {
    let path = macos_plist_path(home.home_dir());
    if dry_run {
        println!("serialwrap: would unload and remove {}", path.display());
        return Ok(());
    }
    if path.exists() {
        // Best-effort: an already-unloaded or never-loaded agent makes
        // `launchctl unload` exit non-zero, which is not this command's
        // business to fail over — the goal ("nothing installed and
        // nothing running") is the same either way.
        let _ = run_service_manager("launchctl", &["unload", &path.to_string_lossy()]);
        std::fs::remove_file(&path)?;
        println!(
            "serialwrap: uninstalled launchd user agent ({})",
            path.display()
        );
    } else {
        println!(
            "serialwrap: no launchd user agent installed at {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install(binary_path: &Path, home: &directories::BaseDirs, dry_run: bool) -> io::Result<()> {
    let path = linux_unit_path(home.config_dir());
    let content = systemd_unit(binary_path);
    if dry_run {
        print_dry_run(&path, &content);
        return Ok(());
    }
    write_unit_file(&path, &content)?;
    run_service_manager("systemctl", &["--user", "daemon-reload"])?;
    run_service_manager(
        "systemctl",
        &["--user", "enable", "--now", &format!("{LABEL}.service")],
    )?;
    println!(
        "serialwrap: installed and started systemd user unit at {}",
        path.display()
    );
    println!("serialwrap: logs: journalctl --user -u {LABEL}.service -f");
    println!(
        "serialwrap: note: a systemd --user unit only starts at boot without an interactive \
         login if lingering is enabled — run `loginctl enable-linger $USER` once so the daemon \
         survives logout/reboot with no session open."
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall(home: &directories::BaseDirs, dry_run: bool) -> io::Result<()> {
    let path = linux_unit_path(home.config_dir());
    if dry_run {
        println!("serialwrap: would disable and remove {}", path.display());
        return Ok(());
    }
    if path.exists() {
        let _ = run_service_manager(
            "systemctl",
            &["--user", "disable", "--now", &format!("{LABEL}.service")],
        );
        std::fs::remove_file(&path)?;
        let _ = run_service_manager("systemctl", &["--user", "daemon-reload"]);
        println!(
            "serialwrap: uninstalled systemd user unit ({})",
            path.display()
        );
    } else {
        println!(
            "serialwrap: no systemd user unit installed at {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install(_binary_path: &Path, _home: &directories::BaseDirs, _dry_run: bool) -> io::Result<()> {
    Err(unsupported_platform_error())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn uninstall(_home: &directories::BaseDirs, _dry_run: bool) -> io::Result<()> {
    Err(unsupported_platform_error())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn unsupported_platform_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "serialwrap service install/uninstall only supports macOS (launchd) and Linux \
         (systemd --user); run `serialwrap daemon` directly on other platforms",
    )
}

fn print_dry_run(path: &Path, content: &str) {
    println!("# {} (not written; --dry-run)", path.display());
    print!("{content}");
}

fn write_unit_file(path: &Path, content: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
}

/// Run a service-manager subcommand, turning a missing binary or non-zero
/// exit into a single actionable [`io::Error`] line rather than a bare
/// `ExitStatus` the caller has to interpret — same "one actionable line"
/// convention `cli::error`'s `describe_connect_error`/`describe_wire_error`
/// already established for the daemon-connection error paths.
fn run_service_manager(program: &str, args: &[&str]) -> io::Result<()> {
    let output = Command::new(program).args(args).output().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "failed to run `{program} {}`: {e} (is it installed and on PATH?)",
                args.join(" ")
            ),
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(io::Error::other(format!(
        "`{program} {}` failed: {}",
        args.join(" "),
        stderr.trim()
    )))
}

// Every helper below is pure (no filesystem/subprocess access) so it's
// directly unit-testable without touching disk — see the module docs on
// why that split exists. Each is gated to the platform it's for, `or test`
// so both platforms' generation logic is exercised by `cargo test` on
// either CI runner (`.github/workflows/ci.yml`'s matrix is macOS + Linux)
// rather than only on its own native platform — without the `test` half of
// this `cfg`, the *other* platform's helpers would be unreachable dead code
// in a normal (non-test) build and fail `clippy -D warnings`.

#[cfg(any(target_os = "macos", test))]
fn macos_plist_path(home_dir: &Path) -> PathBuf {
    home_dir
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

#[cfg(any(target_os = "macos", test))]
fn macos_log_dir(home_dir: &Path) -> PathBuf {
    home_dir.join("Library/Logs/serialwrap")
}

#[cfg(any(target_os = "linux", test))]
fn linux_unit_path(config_dir: &Path) -> PathBuf {
    config_dir
        .join("systemd/user")
        .join(format!("{LABEL}.service"))
}

/// Generate the launchd plist content.
#[cfg(any(target_os = "macos", test))]
fn launchd_plist(binary_path: &Path, log_dir: &Path) -> String {
    let bin = xml_escape(&binary_path.to_string_lossy());
    let out_log = xml_escape(&log_dir.join("daemon.log").to_string_lossy());
    let err_log = xml_escape(&log_dir.join("daemon.err.log").to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{out_log}</string>
    <key>StandardErrorPath</key>
    <string>{err_log}</string>
</dict>
</plist>
"#
    )
}

/// Generate the systemd user unit content. No `StandardOutput`/
/// `StandardError` override: a systemd user unit's stdout/stderr already go
/// to the user journal by default (`journalctl --user -u
/// com.serialwrap.daemon.service -f`), unlike launchd, which has no
/// built-in equivalent and so gets explicit log file paths above.
#[cfg(any(target_os = "linux", test))]
fn systemd_unit(binary_path: &Path) -> String {
    let bin = binary_path.to_string_lossy();
    format!(
        r#"[Unit]
Description=serialwrap daemon (serial port broker)
After=default.target

[Service]
Type=simple
ExecStart={bin} daemon
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
"#
    )
}

/// Escape the five XML-significant characters for a plist string value.
/// `binary_path`/`log_dir` are filesystem paths under the invoking user's
/// own home directory, never attacker-controlled, but a path containing
/// e.g. `&` (an unusual but legal path component) must not produce
/// malformed XML.
#[cfg(any(target_os = "macos", test))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_plist_names_the_binary_and_daemon_subcommand() {
        let plist = launchd_plist(
            Path::new("/usr/local/bin/serialwrap"),
            Path::new("/tmp/logs"),
        );
        assert!(plist.contains("<string>/usr/local/bin/serialwrap</string>"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<true/>"));
    }

    #[test]
    fn launchd_plist_is_well_formed_enough_to_have_matched_tags() {
        let plist = launchd_plist(Path::new("/bin/x"), Path::new("/tmp"));
        for tag in ["plist", "dict", "array"] {
            let opens = plist.matches(&format!("<{tag}")).count();
            let closes = plist.matches(&format!("</{tag}>")).count();
            assert_eq!(opens, closes, "mismatched <{tag}> tags");
        }
    }

    #[test]
    fn launchd_plist_escapes_xml_metacharacters_in_paths() {
        let plist = launchd_plist(Path::new("/tmp/a&b"), Path::new("/tmp"));
        assert!(plist.contains("/tmp/a&amp;b"));
        assert!(!plist.contains("/tmp/a&b<"));
    }

    #[test]
    fn systemd_unit_names_the_binary_and_daemon_subcommand() {
        let unit = systemd_unit(Path::new("/usr/local/bin/serialwrap"));
        assert!(unit.contains("ExecStart=/usr/local/bin/serialwrap daemon"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn macos_plist_path_is_under_library_launch_agents() {
        let path = macos_plist_path(Path::new("/Users/alice"));
        assert_eq!(
            path,
            Path::new("/Users/alice/Library/LaunchAgents/com.serialwrap.daemon.plist")
        );
    }

    #[test]
    fn linux_unit_path_is_under_systemd_user() {
        let path = linux_unit_path(Path::new("/home/alice/.config"));
        assert_eq!(
            path,
            Path::new("/home/alice/.config/systemd/user/com.serialwrap.daemon.service")
        );
    }

    #[test]
    fn write_unit_file_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/dir/unit.plist");
        write_unit_file(&path, "content").expect("write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content");
    }

    #[test]
    fn xml_escape_handles_all_five_metacharacters() {
        assert_eq!(
            xml_escape(r#"a&b<c>d"e'f"#),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f"
        );
    }
}
