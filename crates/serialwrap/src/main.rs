//! `serialwrap`: the single binary. Subcommands dispatch into the `cli`
//! module tree (or, for `daemon`/`mcp`, directly into `serialwrapd`/`mcp`).
//!
//! Dependency direction: `serialwrap` -> `serialwrapd` -> `wrap-proto`.

use clap::{Parser, Subcommand};

/// `devices`/`tail` (T1.5, issue #7) live in their own module tree rather
/// than inline here — see `cli`'s module docs for why.
mod cli;
/// `serialwrap mcp` (T3.1, issue #12) — see `mcp`'s module docs.
mod mcp;

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
    Write(cli::write::WriteArgs),
    /// Take a temporary lease and run an external command against the device
    /// (see TASKS.md T2.2).
    Run(cli::run::RunArgs),
    /// Read or update per-device configuration (see TASKS.md T2.3).
    Config(cli::config::ConfigArgs),
    /// List, kick, or demote connected clients (see TASKS.md T2.3).
    Clients(cli::clients::ClientsArgs),
    /// Export recorded data as jsonl/txt/bin (see TASKS.md T2.4).
    Export(cli::export::ExportArgs),
    /// Query the audit view over the record stream (see TASKS.md T4.3).
    Audit(cli::audit::AuditArgs),
    /// List, approve, or deny pending write approvals (see TASKS.md T4.2).
    Approvals(cli::approvals::ApprovalsArgs),
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => serialwrapd::run().await,
        Command::Mcp => cli::dispatch(mcp::run().await),
        Command::Devices => cli::dispatch(cli::devices::run().await),
        Command::Tail(args) => cli::dispatch(cli::tail::run(args).await),
        Command::Write(args) => cli::dispatch(cli::write::run(args).await),
        Command::Run(args) => cli::dispatch(cli::run::run(args).await),
        Command::Config(args) => cli::dispatch(cli::config::run(args).await),
        Command::Clients(args) => cli::dispatch(cli::clients::run(args).await),
        Command::Export(args) => cli::dispatch(cli::export::run(args).await),
        Command::Audit(args) => cli::dispatch(cli::audit::run(args).await),
        Command::Approvals(args) => cli::dispatch(cli::approvals::run(args).await),
    }
}
