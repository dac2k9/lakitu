//! Agent-board MCP server.
//!
//! Exposes the four Phase-1 primitives used by board-issue-loop and
//! pr-review-fixup: `emit_event`, `move_card`, `set_blocker`,
//! `clear_blocker`. Each wraps the deterministic side-effectful
//! sequence that the LLM was previously re-deriving from skill markdown,
//! collapsing 10–40 line bash incantations into a single tool call.
//!
//! Two transports, one tool surface:
//!   * default (no args) — **stdio**, Claude Code's local MCP transport;
//!   * `serve` — the **HTTP daemon** (`daemon::serve`), MCP-over-HTTP so agents
//!     on other machines reach one shared fleet store (see `daemon.rs`).
//! State-guard, idempotency, and ID-cache live here so the skill stays
//! about decisions, not execution mechanics.

mod daemon;
mod fleet;
mod persona;
mod rest;
mod server;
mod wire;

use std::fs::OpenOptions;

use anyhow::Result;
use rmcp::{transport::stdio, ServiceExt};
use server::AgentBoardService;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing()?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => daemon::serve().await,
        Some(other) => {
            anyhow::bail!("unknown subcommand {other:?} (use `serve`, or no args for stdio)")
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
