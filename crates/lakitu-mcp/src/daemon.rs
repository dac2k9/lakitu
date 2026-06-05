//! HTTP daemon mode (`lakitu-mcp serve`).
//!
//! Serves the same `AgentBoardService` tools as the stdio binary, but over
//! MCP-over-HTTP (rmcp's streamable-HTTP transport) so agents on other machines
//! can reach one shared fleet store. A single bearer token (`LAKITU_FLEET_TOKEN`)
//! gates every request. The stdio path (default, no subcommand) is unchanged.
//!
//! Phase 2 mounts a `/v1` REST API (for the hooks + the remote cockpit) onto
//! this same router, under this same auth layer.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::Response,
    Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;

use crate::server::AgentBoardService;

/// Default loopback bind. Safe by default — expose the daemon on a private
/// interface (tailscale / reverse proxy), don't bind `0.0.0.0` directly.
const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// Run the coordination daemon: MCP-over-HTTP at `/mcp`, bearer-gated.
pub async fn serve() -> Result<()> {
    let listen = std::env::var("LAKITU_FLEET_LISTEN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());

    // Fail fast if launched without a token — the daemon must never run open.
    let token = std::env::var("LAKITU_FLEET_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "LAKITU_FLEET_TOKEN must be set (non-empty) to run `serve` — it bearer-gates every request",
        )?;
    let token = Arc::new(token);

    // One shared service; the per-session factory clones it so the project/field
    // ID cache (Arc<RwLock<…>>) is shared across all HTTP sessions instead of
    // starting cold on every connection.
    let shared = AgentBoardService::new();
    let ct = CancellationToken::new();

    let mcp = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(shared.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: true,
            sse_keep_alive: Some(Duration::from_secs(15)),
            sse_retry: Some(Duration::from_secs(3)),
            cancellation_token: ct.child_token(),
        },
    );

    let app = Router::new()
        .nest_service("/mcp", mcp)
        .merge(crate::rest::router())
        .layer(middleware::from_fn_with_state(token, bearer_auth));

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "lakitu-mcp daemon listening (MCP at /mcp)");

    let shutdown_ct = ct.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal — cancelling sessions");
            shutdown_ct.cancel();
        })
        .await
        .context("daemon server error")?;
    Ok(())
}

/// Bearer-token gate over every route. Reads `Authorization: Bearer <token>`,
/// constant-time compares it against `LAKITU_FLEET_TOKEN`, 401s otherwise.
async fn bearer_auth(
    State(expected): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(tok) if constant_time_eq(tok.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Length-aware constant-time byte compare, so a wrong token can't be recovered
/// from response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrey"));
        assert!(!constant_time_eq(b"secret", b"secre")); // length mismatch
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
