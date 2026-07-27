//! `serialwrap`: the single binary. Subcommands dispatch to either the
//! daemon core (`serialwrapd`) or, for now, a stub that names the future
//! implementation's `TASKS.md` entry.
//!
//! Dependency direction: `serialwrap` -> `serialwrapd` -> `wrap-proto`.

use clap::{Parser, Subcommand};

/// `devices`/`tail` (T1.5, issue #7) live in their own module tree rather
/// than inline here — see `cli`'s module docs for why.
mod cli;

#[derive(Parser)]
#[command(
    name = "serialwrap",
    version,
    about = "Serial port broker: one daemon owns the port, everyone else is a client."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the serialwrapd daemon (see TASKS.md T1.1-T1.4).
    Daemon,
    /// Run the MCP stdio bridge (see TASKS.md T3.1).
    Mcp,
    /// List known devices (see TASKS.md T1.5).
    Devices,
    /// Tail a device's record stream (see TASKS.md T1.5).
    Tail(cli::tail::TailArgs),
    /// Write bytes to a device, subject to the write gate (see TASKS.md T2.1).
    Write,
    /// Take a temporary lease and run an external command against the device
    /// (see TASKS.md T2.2).
    Run,
    /// Read or update per-device configuration (see TASKS.md T2.3).
    Config,
    /// List, kick, or demote connected clients (see TASKS.md T2.3).
    Clients,
    /// Export recorded data as jsonl/txt/bin (see TASKS.md T2.4).
    Export,
    /// Query the audit view over the record stream (see TASKS.md T4.3).
    Audit,
    /// Manage pending write approvals (see TASKS.md T4.2).
    Approvals,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => serialwrapd::run().await,
        Command::Mcp => stub("mcp", "T3.1"),
        Command::Devices => cli::dispatch(cli::devices::run().await),
        Command::Tail(args) => cli::dispatch(cli::tail::run(args).await),
        Command::Write => stub("write", "T2.1"),
        Command::Run => stub("run", "T2.2"),
        Command::Config => stub("config", "T2.3"),
        Command::Clients => stub("clients", "T2.3"),
        Command::Export => stub("export", "T2.4"),
        Command::Audit => stub("audit", "T4.3"),
        Command::Approvals => stub("approvals", "T4.2"),
    }
}

/// Skeleton subcommand body: not implemented yet at this stage (TASKS.md T0.1).
fn stub(name: &str, task: &str) -> std::io::Result<()> {
    println!("serialwrap {name}: not implemented yet (see TASKS.md {task})");
    Ok(())
}
