//! `lakitu-mcp` — the MCP server + coordination daemon behind Lakitu.
//!
//! Gives a fleet of Claude Code agents the tools to register, report presence,
//! exchange messages, keep personal tasks and personas, and (optionally) drive
//! a GitHub Projects board — all writing the shared `~/.claude/lakitu-fleet`
//! store that the `lakitu` cockpit renders.
//!
//! Modes:
//!   * default (no args) — **stdio**, Claude Code's local per-agent MCP transport;
//!   * `serve` — the **HTTP daemon** (`daemon::serve`): MCP-over-HTTP plus a
//!     `/v1` REST API so a fleet can span machines (see `daemon.rs`, `rest.rs`);
//!   * `install-hooks` — materialize the lifecycle hooks + coordination skill
//!     into `~/.claude` (see `install.rs`).

// Nested `if`s are deliberate for readability; `nonminimal_bool` is allowed so
// `set_state`'s guard can stay written the way the shell hook expresses it.
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::nonminimal_bool
)]

mod daemon;
mod fleet;
mod install;
mod persona;
mod rest;
mod server;
mod wire;

use std::fs::OpenOptions;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use server::AgentBoardService;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => daemon::serve().await,
        Some("install-hooks") => install::run(),
        Some(other) => {
            anyhow::bail!(
                "unknown subcommand {other:?} \
                 (use `serve` for the HTTP daemon, `install-hooks` to set up the fleet \
                 hooks, or no args for stdio)"
            )
        }
        None => serve_stdio().await,
    }
}

/// tracing goes to a side-channel file because in stdio mode stdout is reserved
/// for the JSON-RPC wire. macOS' temp_dir lands in /var/folders/...; fine for
/// dev. If we ever want persistent logs, switch to ~/.claude/logs.
fn init_tracing() -> Result<()> {
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("lakitu-mcp.log"))?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();
    Ok(())
}

async fn serve_stdio() -> Result<()> {
    tracing::info!("lakitu-mcp starting (stdio)");
    let service = AgentBoardService::new();
    service.serve(stdio()).await?.waiting().await?;
    Ok(())
}
