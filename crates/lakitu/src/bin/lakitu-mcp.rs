//! `lakitu-mcp` — deprecation shim.
//!
//! The two binaries were merged into one `lakitu` for 0.5.0. This thin shim
//! preserves the old `lakitu-mcp` command for one release so a running fleet
//! (whose MCP configs still say `"command": "lakitu-mcp"`) keeps working while
//! configs migrate to `lakitu mcp` / `lakitu serve` / `lakitu install-hooks`.
//! It is removed next release.
//!
//! It reuses the library entry points (`lakitu::server`, `lakitu::daemon`,
//! `lakitu::install`) — no logic is duplicated — and mirrors the old dispatch:
//! no args → stdio serve; `serve` → HTTP daemon; `install-hooks` → installer.

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};

use lakitu::server::AgentBoardService;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Deprecation notice + best-effort usage log go to STDERR / a side-channel
    // file — never stdout, which in stdio mode is the JSON-RPC wire.
    eprintln!(
        "lakitu-mcp is deprecated — use `lakitu mcp` / `lakitu serve` / `lakitu install-hooks`. This shim is removed next release."
    );
    note_shim_use();

    init_tracing()?;
    match std::env::args().nth(1).as_deref() {
        Some("serve") => lakitu::daemon::serve().await,
        Some("install-hooks") => lakitu::install::run(),
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

/// Append one line to a side-channel usage log so the shim can be dropped
/// data-driven later. Best-effort: any error is ignored.
fn note_shim_use() {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("lakitu-mcp-shim.log"))
    {
        let _ = writeln!(
            f,
            "{} lakitu-mcp shim invoked: {:?}",
            chrono::Local::now().to_rfc3339(),
            std::env::args().collect::<Vec<_>>()
        );
    }
}

/// tracing goes to a side-channel file because in stdio mode stdout is reserved
/// for the JSON-RPC wire.
fn init_tracing() -> Result<()> {
    use tracing_subscriber::EnvFilter;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("lakitu-mcp.log"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();
    Ok(())
}

async fn serve_stdio() -> Result<()> {
    tracing::info!("lakitu-mcp starting (stdio, via deprecation shim)");
    let service = AgentBoardService::new();
    service.serve(stdio()).await?.waiting().await?;
    Ok(())
}
